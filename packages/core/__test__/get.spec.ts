import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

// Point lookups: get_ (value retrieval), has_ (presence). Path segments are
// passed variadically: strings address object members, numbers address array
// elements.

test('get_root_returns_whole_document', async () => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2}')))
  assert.deepEqual(await cursor.get(), { a: 1, b: 2 })
})

test('get_scalar_from_object', async () => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2}')))
  assert.equal(await cursor.get('a'), 1)
  assert.equal(await cursor.get('b'), 2)
})

test('get_nested_value', async () => {
  const cursor = await open(memorySource(enc('{"user":{"name":"Alice","age":30}}')))
  assert.equal(await cursor.get('user', 'name'), 'Alice')
  assert.equal(await cursor.get('user', 'age'), 30)
})

test('get_array_index', async () => {
  const cursor = await open(memorySource(enc('{"items":[10,20,30,40,50]}')))
  assert.equal(await cursor.get('items', 0), 10)
  assert.equal(await cursor.get('items', 4), 50)
})

test('get_full_subobject', async () => {
  const cursor = await open(memorySource(enc('{"a":{"b":1,"c":[2,3]}}')))
  assert.deepEqual(await cursor.get('a'), { b: 1, c: [2, 3] })
})

test('get_path_segments_can_be_spread_from_an_array', async (t) => {
  const cursor = await open(memorySource(enc('{"users":[{"name":"Alice"}]}')))
  t.after(() => cursor.close())
  const path = ['users', 0, 'name'] as const
  assert.equal(await cursor.get(...path), 'Alice')
})

test('get_string_with_structural_chars', async () => {
  const cursor = await open(memorySource(enc('{"x":"has } and , inside","y":2}')))
  assert.equal(await cursor.get('y'), 2)
  assert.equal(await cursor.get('x'), 'has } and , inside')
})

test('get_keys_with_slashes_and_tildes_are_just_keys', async (t) => {
  // What used to need ~1 / ~0 escapes in RFC 6901 is now just a string.
  const cursor = await open(memorySource(enc('{"a/b":1,"c~d":2,"":{"":7}}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('a/b'), 1)
  assert.equal(await cursor.get('c~d'), 2)
  assert.equal(await cursor.get('', ''), 7)
})

test('get_unicode_keys_pass_through', async (t) => {
  const doc = JSON.stringify({ 日本語: { αβγ: 42 } })
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('日本語', 'αβγ'), 42)
})

test('get_keys_with_json_escapes_decode_then_compare', async (t) => {
  // Keys whose stored JSON form uses escapes (\", \\, \n) decode to the
  // user-facing string before the equality check.
  const doc = JSON.stringify({ 'with"quote': 'v', 'with\\backslash': 'w', 'newline\nkey': 'x' })
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('with"quote'), 'v')
  assert.equal(await cursor.get('with\\backslash'), 'w')
  assert.equal(await cursor.get('newline\nkey'), 'x')
})

test('get_array_out_of_range_is_missing', async (t) => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30]}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('xs', 0), 10)
  assert.equal(await cursor.get('xs', 2), 30)
  assert.equal(await cursor.has('xs', 3), false)
})

test('get_missing_returns_undefined_distinct_from_json_null', async () => {
  const cursor = await open(memorySource(enc('{"a":1,"b":null}')))
  assert.equal(await cursor.get('missing'), undefined)
  assert.equal(await cursor.get('b'), null)
  assert.equal(await cursor.has('missing'), false)
  assert.equal(await cursor.has('b'), true)
})

test('get_type_mismatch_is_missing_not_error', async (t) => {
  // Member-name against an array, or numeric index against an object, both
  // resolve to nothing rather than throwing - same total / non-throwing
  // shape as `has` and `count`.
  const cursor = await open(memorySource(enc('{"xs":[10,20],"obj":{"k":"v"}}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.has('xs', 'name'), false)
  assert.equal(await cursor.has('obj', 0), false)
})

test('get_rejects_fractional_negative_nan_and_non_string_number_segments', async (t) => {
  const cursor = await open(memorySource(enc('{"xs":[1,2,3]}')))
  t.after(() => cursor.close())
  // @ts-expect-error fractional index is not a valid segment
  await assert.rejects(() => cursor.get('xs', 1.5), TypeError)
  // @ts-expect-error negative index is not a valid segment
  await assert.rejects(() => cursor.get('xs', -1), TypeError)
  // @ts-expect-error NaN is not a valid segment
  await assert.rejects(() => cursor.get('xs', Number.NaN), TypeError)
  // @ts-expect-error a non-string/non-number segment is rejected
  await assert.rejects(() => cursor.get('xs', null), TypeError)
})

test('has_presence_and_absence', async () => {
  const cursor = await open(memorySource(enc('{"a":1,"b":[10,20]}')))
  assert.equal(await cursor.has('a'), true)
  assert.equal(await cursor.has('b', 1), true)
  assert.equal(await cursor.has('missing'), false)
  assert.equal(await cursor.has('b', 5), false)
})

test('has_resolves_value_like_get', async () => {
  assert.equal(await (await open(memorySource(enc('{"a":1}')))).has(), true)
  assert.equal(await (await open(memorySource(enc('{"a":1}')))).has('missing'), false)
  await assert.rejects((await open(memorySource(enc('')))).has())
})

test('iter_select_rejects_bad_sub_path_segments', async (t) => {
  const cursor = await open(memorySource(enc('{"xs":[{"a":1}]}')))
  t.after(() => cursor.close())
  // Sub-path validation runs at .iter() construction (synchronous), so the
  // throw lands before any iteration starts.
  // @ts-expect-error fractional segment is rejected
  assert.throws(() => cursor.iter('xs', { select: ['a', 1.5] }), TypeError)
  // @ts-expect-error sub-path in a map is also validated
  assert.throws(() => cursor.iter('xs', { select: { a: [-1] } }), TypeError)
})
