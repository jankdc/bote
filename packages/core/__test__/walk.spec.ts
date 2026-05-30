import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

// walk_ yields a subcursor per child: its `key` reflects the parent step and its
// relative gets resolve against the child anchor.

test('walk_object_yields_keys_and_values', async () => {
  const cursor = await open(memorySource(enc('{"first":1,"second":"two","third":[3,4]}')))
  const entries: Array<{ key: string | number | null; value: unknown }> = []
  for await (const sub of cursor.walk()) {
    entries.push({ key: sub.key, value: await sub.get() })
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
  const cursor = await open(memorySource(enc('[10,20,30]')))
  const entries: Array<{ key: string | number | null; value: unknown }> = []
  for await (const sub of cursor.walk()) {
    entries.push({ key: sub.key, value: await sub.get() })
  }
  assert.deepEqual(entries, [
    { key: 0, value: 10 },
    { key: 1, value: 20 },
    { key: 2, value: 30 },
  ])
})

test('walk_subcursor_key_and_get_resolve_against_anchor', async () => {
  const data = enc('{"users":[{"name":"Alice","age":30},{"name":"Bob","age":25}]}')
  const cursor = await open(memorySource(data))
  const keys: Array<string | number | null> = []
  const names: string[] = []
  for await (const user of cursor.walk('users')) {
    keys.push(user.key)
    names.push((await user.get('name')) as string)
  }
  assert.deepEqual(keys, [0, 1]) // key reflects the parent step, even when nested
  assert.deepEqual(names, ['Alice', 'Bob']) // relative get resolves against the child anchor
})

test('walk_missing_path_yields_empty', async (t) => {
  // A missing path on `walk` produces no children, mirroring the same total
  // / non-throwing semantics get/has/count already have.
  const cursor = await open(memorySource(enc('{"users":[1,2]}')))
  t.after(() => cursor.close())
  const seen: unknown[] = []
  for await (const sub of cursor.walk('missing')) seen.push(sub.key)
  assert.deepEqual(seen, [])
})

test('walk_large_array_under_tight_budget', async () => {
  const items = Array.from({ length: 100 }, (_, i) => `{"id":${i},"name":"item-${i}"}`)
  const data = enc('[' + items.join(',') + ']')
  const cursor = await open(memorySource(data, 128), { maxResidentBytes: 16 * 128 })
  const ids: number[] = []
  for await (const item of cursor.walk()) {
    ids.push((await item.get('id')) as number)
  }
  assert.equal(ids.length, 100)
  assert.equal(ids[0], 0)
  assert.equal(ids[99], 99)
})
