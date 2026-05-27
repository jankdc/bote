import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

// Point lookups: get_ (value retrieval), has_ (presence), pointer_ (pointer validation).

test('get_scalar_from_object', async () => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2}')))
  assert.equal(await cursor.get('/a'), 1)
  assert.equal(await cursor.get('/b'), 2)
})

test('get_nested_value', async () => {
  const cursor = await open(memorySource(enc('{"user":{"name":"Alice","age":30}}')))
  assert.equal(await cursor.get('/user/name'), 'Alice')
  assert.equal(await cursor.get('/user/age'), 30)
})

test('get_array_index', async () => {
  const cursor = await open(memorySource(enc('{"items":[10,20,30,40,50]}')))
  assert.equal(await cursor.get('/items/0'), 10)
  assert.equal(await cursor.get('/items/4'), 50)
})

test('get_full_subobject', async () => {
  const cursor = await open(memorySource(enc('{"a":{"b":1,"c":[2,3]}}')))
  assert.deepEqual(await cursor.get('/a'), { b: 1, c: [2, 3] })
})

test('get_string_with_structural_chars', async () => {
  const cursor = await open(memorySource(enc('{"x":"has } and , inside","y":2}')))
  assert.equal(await cursor.get('/y'), 2)
  assert.equal(await cursor.get('/x'), 'has } and , inside')
})

test('get_missing_rejects', async () => {
  const cursor = await open(memorySource(enc('{"a":1}')))
  await assert.rejects(cursor.get('/missing'))
})

test('has_presence_and_absence', async () => {
  const cursor = await open(memorySource(enc('{"a":1,"b":[10,20]}')))
  assert.equal(await cursor.has('/a'), true)
  assert.equal(await cursor.has('/b/1'), true)
  assert.equal(await cursor.has('/missing'), false)
  assert.equal(await cursor.has('/b/5'), false)
})

test('pointer_rejects_malformed_literals', async (t) => {
  const cursor = await open(memorySource(enc('{"users":[{"name":"Alice"}]}')))
  t.after(() => cursor.close())
  // Each line below should be a TS error at the call site; we still await
  // the rejected promise so the runtime check doesn't leak an unhandled
  // rejection if the type validator is ever weakened.
  // @ts-expect-error pointer must start with '/' or be empty
  await assert.rejects(cursor.get('users'))
  // @ts-expect-error '~' must be followed by '0' or '1'
  await assert.rejects(cursor.get('/foo~'))
  // @ts-expect-error '~2' is not a valid escape
  await assert.rejects(cursor.get('/foo~2bar'))
  // Sanity: well-formed pointers compile.
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
})
