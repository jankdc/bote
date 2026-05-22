import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, fromBuffer, type Source } from '../src/index.ts'

function memorySource(data: Uint8Array, chunkBytes?: number): Source {
  return fromBuffer(data, chunkBytes === undefined ? undefined : { chunkBytes })
}

test('get_scalar_from_object', async () => {
  const data = new TextEncoder().encode('{"a":1,"b":2}')
  const cursor = await open(memorySource(data))
  assert.equal(await cursor.get('/a'), 1)
  assert.equal(await cursor.get('/b'), 2)
})

test('get_nested_value', async () => {
  const data = new TextEncoder().encode('{"user":{"name":"Alice","age":30}}')
  const cursor = await open(memorySource(data))
  assert.equal(await cursor.get('/user/name'), 'Alice')
  assert.equal(await cursor.get('/user/age'), 30)
})

test('get_array_index', async () => {
  const data = new TextEncoder().encode('{"items":[10,20,30,40,50]}')
  const cursor = await open(memorySource(data))
  assert.equal(await cursor.get('/items/0'), 10)
  assert.equal(await cursor.get('/items/4'), 50)
})

test('get_full_subobject', async () => {
  const data = new TextEncoder().encode('{"a":{"b":1,"c":[2,3]}}')
  const cursor = await open(memorySource(data))
  assert.deepEqual(await cursor.get('/a'), { b: 1, c: [2, 3] })
})

test('get_string_with_structural_chars', async () => {
  const data = new TextEncoder().encode('{"x":"has } and , inside","y":2}')
  const cursor = await open(memorySource(data))
  assert.equal(await cursor.get('/y'), 2)
  assert.equal(await cursor.get('/x'), 'has } and , inside')
})

test('get_missing_rejects', async () => {
  const data = new TextEncoder().encode('{"a":1}')
  const cursor = await open(memorySource(data))
  await assert.rejects(cursor.get('/missing'))
})

test('has_presence_and_absence', async () => {
  const data = new TextEncoder().encode('{"a":1,"b":[10,20]}')
  const cursor = await open(memorySource(data))
  assert.equal(await cursor.has('/a'), true)
  assert.equal(await cursor.has('/b/1'), true)
  assert.equal(await cursor.has('/missing'), false)
  assert.equal(await cursor.has('/b/5'), false)
})

test('iter_array_elements', async () => {
  const data = new TextEncoder().encode('{"xs":[10,20,30,40]}')
  const cursor = await open(memorySource(data))
  const values: unknown[] = []
  for await (const v of cursor.iter('/xs')) values.push(v)
  assert.deepEqual(values, [10, 20, 30, 40])
})

test('iter_object_members', async () => {
  const data = new TextEncoder().encode('{"o":{"a":1,"b":2,"c":3}}')
  const cursor = await open(memorySource(data))
  const values: unknown[] = []
  for await (const v of cursor.iter('/o')) values.push(v)
  assert.deepEqual(values.sort(), [1, 2, 3])
})

test('iter_non_container_yields_nothing', async () => {
  const data = new TextEncoder().encode('{"scalar":42}')
  const cursor = await open(memorySource(data))
  const values: unknown[] = []
  for await (const v of cursor.iter('/scalar')) values.push(v)
  assert.deepEqual(values, [])
})

test('walk_object_yields_keys_and_values', async () => {
  const data = new TextEncoder().encode('{"first":1,"second":"two","third":[3,4]}')
  const cursor = await open(memorySource(data))
  const entries: Array<{ key: string | number | null; value: unknown }> = []
  for await (const sub of cursor.walk('')) {
    entries.push({ key: sub.key, value: await sub.get('') })
  }
  assert.equal(entries.length, 3)
  assert.deepEqual(
    entries.find((e) => e.key === 'first'),
    { key: 'first', value: 1 },
  )
  assert.deepEqual(
    entries.find((e) => e.key === 'second'),
    { key: 'second', value: 'two' },
  )
  assert.deepEqual(
    entries.find((e) => e.key === 'third'),
    { key: 'third', value: [3, 4] },
  )
})

test('walk_array_yields_numeric_keys', async () => {
  const data = new TextEncoder().encode('[10,20,30]')
  const cursor = await open(memorySource(data))
  const entries: Array<{ key: string | number | null; value: unknown }> = []
  for await (const sub of cursor.walk('')) {
    entries.push({ key: sub.key, value: await sub.get('') })
  }
  assert.deepEqual(entries, [
    { key: 0, value: 10 },
    { key: 1, value: 20 },
    { key: 2, value: 30 },
  ])
})

test('walk_subcursor_get_resolves_relative_to_anchor', async () => {
  const data = new TextEncoder().encode('{"users":[{"name":"Alice","age":30},{"name":"Bob","age":25}]}')
  const cursor = await open(memorySource(data))
  const names: string[] = []
  for await (const user of cursor.walk('/users')) {
    names.push((await user.get('/name')) as string)
  }
  assert.deepEqual(names, ['Alice', 'Bob'])
})

test('walk_large_array_under_tight_budget', async () => {
  const items = Array.from({ length: 100 }, (_, i) => `{"id":${i},"name":"item-${i}"}`)
  const data = new TextEncoder().encode('[' + items.join(',') + ']')
  const cursor = await open(memorySource(data, 128), { maxResidentChunks: 16 })
  const ids: number[] = []
  for await (const item of cursor.walk('')) {
    ids.push((await item.get('/id')) as number)
  }
  assert.equal(ids.length, 100)
  assert.equal(ids[0], 0)
  assert.equal(ids[99], 99)
})

test('cache_reads_are_chunk_aligned', async () => {
  const data = new TextEncoder().encode('[' + Array.from({ length: 200 }, () => '1').join(',') + ']')
  const reads: Array<{ offset: number; length: number }> = []
  const source: Source = {
    open: () =>
      Promise.resolve({
        size: data.length,
        chunkBytes: 64,
        read: async (offset, dst) => {
          reads.push({ offset, length: dst.byteLength })
          const end = Math.min(offset + dst.byteLength, data.length)
          const n = Math.max(0, end - offset)
          if (n > 0) dst.set(data.subarray(offset, end))
          return n
        },
      }),
  }
  const cursor = await open(source)
  assert.equal(await cursor.get('/100'), 1)
  for (const r of reads) {
    assert.equal(r.offset % 64, 0, `unaligned offset ${r.offset}`)
    assert.equal(r.length, 64, `unexpected length ${r.length}`)
  }
})

test('cache_large_doc_under_tight_slot_cap', async () => {
  // 30 KB object with 2000 keys; cap = 16 slots, chunk = 256 bytes.
  // The query must succeed under heavy fetching and eviction.
  const parts = ['{']
  for (let i = 0; i < 2000; i++) {
    if (i > 0) parts.push(',')
    parts.push(`"k${String(i).padStart(4, '0')}":${i}`)
  }
  parts.push('}')
  const data = new TextEncoder().encode(parts.join(''))
  const cursor = await open(memorySource(data, 256), { maxResidentChunks: 16 })
  assert.equal(await cursor.get('/k1500'), 1500)
  assert.equal(await cursor.get('/k0042'), 42)
  assert.equal(await cursor.has('/k9999'), false)
})
