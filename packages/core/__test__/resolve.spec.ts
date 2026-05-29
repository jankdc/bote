import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, type Segment } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

// Resolution correctness across chunk sizes: the same value must resolve whether
// it sits within a single chunk or straddles several. Mirrors the chunk-size
// sweep the native resolver is exercised under, but through the public facade.

const CHUNK_SIZES = [64, 128, 4096]

async function assertGet(doc: string, path: readonly Segment[], expected: unknown): Promise<void> {
  for (const chunkBytes of CHUNK_SIZES) {
    const cursor = await open(memorySource(enc(doc), chunkBytes))
    try {
      assert.deepEqual(await cursor.get(...path), expected, `path=${JSON.stringify(path)} chunkBytes=${chunkBytes}`)
    } finally {
      await cursor.close()
    }
  }
}

test('resolve_root_returns_whole_document', async () => {
  await assertGet('{"a":1,"b":2}', [], { a: 1, b: 2 })
})

test('resolve_nested_object_traversal', async () => {
  const doc = '{"user":{"name":{"first":"Alice","last":"Smith"},"age":30}}'
  await assertGet(doc, ['user', 'name', 'first'], 'Alice')
  await assertGet(doc, ['user', 'name', 'last'], 'Smith')
  await assertGet(doc, ['user', 'age'], 30)
})

test('resolve_nested_arrays', async () => {
  await assertGet('[[1,2],[3,4],[5,6]]', [1, 1], 4)
  await assertGet('[[1,2],[3,4],[5,6]]', [2, 0], 5)
})

test('resolve_primitive_values', async () => {
  const doc = '{"t":true,"f":false,"n":null,"i":-42,"x":1.5e3}'
  await assertGet(doc, ['t'], true)
  await assertGet(doc, ['f'], false)
  await assertGet(doc, ['n'], null)
  await assertGet(doc, ['i'], -42)
  await assertGet(doc, ['x'], 1500)
})

test('resolve_tolerates_insignificant_whitespace', async () => {
  await assertGet('  {  "a"  :  [  1  ,  2  ,  3  ]  }  ', ['a', 1], 2)
})

test('resolve_skips_deeply_nested_siblings', async () => {
  const doc = '{"first":{"a":{"b":{"c":[1,[2,[3,{"d":4}]]]}}},"target":99}'
  await assertGet(doc, ['target'], 99)
  await assertGet(doc, ['first', 'a', 'b', 'c', 1, 1, 1, 'd'], 4)
})

test('resolve_skips_strings_containing_structural_chars', async () => {
  await assertGet('{"x":"this has } and , in it","y":2}', ['y'], 2)
})

test('resolve_value_straddling_chunk_boundaries', async () => {
  // Each level is padded so the next sits in a later 64-byte chunk; every
  // traversal then crosses at least one boundary at the smallest chunk size.
  const pad = ' '.repeat(60)
  const doc = `{"a":${pad}{"b":${pad}{"c":${pad}42}}}`
  await assertGet(doc, ['a', 'b', 'c'], 42)
})

test('resolve_array_index_through_many_chunks', async () => {
  const items = Array.from({ length: 1000 }, (_, i) => `"item-${String(i).padStart(4, '0')}"`)
  await assertGet('[' + items.join(',') + ']', [500], 'item-0500')
})
