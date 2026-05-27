import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

// JSON Pointer (RFC 6901) resolution semantics observed through get_/has_: tilde
// escapes, JSON string escapes in keys, unicode, empty tokens, and array-index
// rules. Static rejection of malformed pointer literals lives in get.spec.ts.

test('pointer_decodes_tilde_escapes_in_keys', async (t) => {
  const cursor = await open(memorySource(enc('{"a/b":1,"c~d":2}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/a~1b'), 1) // ~1 decodes to '/'
  assert.equal(await cursor.get('/c~0d'), 2) // ~0 decodes to '~'
})

test('pointer_nested_escapes_decode_left_to_right', async (t) => {
  // Keys literally named "~1" and "/0".
  const cursor = await open(memorySource(enc('{"~1":1,"/0":2}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/~01'), 1) // ~0 then '1' -> "~1"
  assert.equal(await cursor.get('/~10'), 2) // ~1 then '0' -> "/0"
})

test('pointer_resolves_json_string_escapes_in_keys', async (t) => {
  const doc = JSON.stringify({ 'with"quote': 'v', 'with\\backslash': 'w', 'newline\nkey': 'x' })
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/with"quote'), 'v')
  assert.equal(await cursor.get('/with\\backslash'), 'w')
  assert.equal(await cursor.get('/newline\nkey'), 'x')
})

test('pointer_unicode_keys_pass_through', async (t) => {
  const doc = JSON.stringify({ 日本語: { αβγ: 42 } })
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/日本語/αβγ'), 42)
})

test('pointer_empty_tokens_are_real_keys', async (t) => {
  // RFC 6901: "" is a valid member name; "//" addresses the "" key at depth two.
  const cursor = await open(memorySource(enc('{"":{"":7}}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('//'), 7)
})

test('pointer_array_index_rules', async (t) => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30]}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/xs/0'), 10)
  assert.equal(await cursor.get('/xs/2'), 30)
  // Out-of-range, leading-zero, and the "-" end-marker never resolve.
  assert.equal(await cursor.has('/xs/3'), false)
  assert.equal(await cursor.has('/xs/01'), false)
  assert.equal(await cursor.has('/xs/-'), false)
})

test('pointer_rfc6901_section_5_examples', async (t) => {
  const doc = JSON.stringify({
    foo: ['bar', 'baz'],
    '': 0,
    'a/b': 1,
    'c%d': 2,
    'e^f': 3,
    'g|h': 4,
    'i\\j': 5,
    'k"l': 6,
    ' ': 7,
    'm~n': 8,
  })
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  assert.deepEqual(await cursor.get('/foo'), ['bar', 'baz'])
  assert.equal(await cursor.get('/foo/0'), 'bar')
  assert.equal(await cursor.get('/'), 0)
  assert.equal(await cursor.get('/a~1b'), 1)
  assert.equal(await cursor.get('/c%d'), 2)
  assert.equal(await cursor.get('/e^f'), 3)
  assert.equal(await cursor.get('/g|h'), 4)
  assert.equal(await cursor.get('/i\\j'), 5)
  assert.equal(await cursor.get('/k"l'), 6)
  assert.equal(await cursor.get('/ '), 7)
  assert.equal(await cursor.get('/m~0n'), 8)
})
