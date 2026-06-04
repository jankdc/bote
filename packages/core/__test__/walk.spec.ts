import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

// walk_ yields a [key, cursor] pair per object member: the key is the member name
// and the cursor's relative gets resolve against its anchor.

test('walk_object_yields_keys_and_values', async () => {
  const cursor = await open(memorySource(enc('{"first":1,"second":"two","third":[3,4]}')))
  const entries: Array<{ key: string; value: unknown }> = []
  for await (const [key, sub] of cursor.walk()) {
    entries.push({ key, value: await sub.get() })
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

test('walk_on_array_target_throws', async () => {
  // walk is the object mirror of iter: an array target steers the caller to iter(),
  // just as iter on an object steers to walk().
  const cursor = await open(memorySource(enc('{"nums":[10,20,30]}')))
  await assert.rejects(
    (async () => {
      for await (const _ of cursor.walk('nums')) void _
    })(),
    /iter\(\)/,
  )
})

test('walk_non_container_yields_empty', async () => {
  // A scalar target yields nothing (not an error), matching iter and get/has/count.
  const cursor = await open(memorySource(enc('{"scalar":42}')))
  const seen: unknown[] = []
  for await (const [key] of cursor.walk('scalar')) seen.push(key)
  assert.deepEqual(seen, [])
})

test('walk_subcursor_key_and_get_resolve_against_anchor', async () => {
  const data = enc('{"users":{"alice":{"name":"Alice","age":30},"bob":{"name":"Bob","age":25}}}')
  const cursor = await open(memorySource(data))
  const keys: string[] = []
  const names: string[] = []
  for await (const [key, user] of cursor.walk('users')) {
    keys.push(key)
    names.push((await user.get('name')) as string)
  }
  assert.deepEqual(keys, ['alice', 'bob'])
  assert.deepEqual(names, ['Alice', 'Bob'])
})

test('walk_large_object_with_small_chunks', async () => {
  const members = Array.from({ length: 100 }, (_, i) => `"item-${i}":{"id":${i},"name":"item-${i}"}`)
  const data = enc('{' + members.join(',') + '}')
  const cursor = await open(memorySource(data, 128))
  const ids: number[] = []
  for await (const [, item] of cursor.walk()) {
    ids.push((await item.get('id')) as number)
  }
  assert.equal(ids.length, 100)
  assert.equal(ids[0], 0)
  assert.equal(ids[99], 99)
})

test('walk_missing_path_yields_empty', async (t) => {
  // A missing path on `walk` produces no children, mirroring the same total
  // / non-throwing semantics get/has/count already have.
  const cursor = await open(memorySource(enc('{"users":[1,2]}')))
  t.after(() => cursor.close())
  const seen: unknown[] = []
  for await (const [key] of cursor.walk('missing')) seen.push(key)
  assert.deepEqual(seen, [])
})
