import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, fromBuffer, ValidationError, type Source, type StandardSchemaV1 } from '../src/index.ts'

function memorySource(data: Uint8Array): Source {
  return fromBuffer(data)
}

const DOC = JSON.stringify({
  users: [
    { id: 1, name: 'Alice', tags: ['admin', 'staff'] },
    { id: 2, name: 'Bob', tags: ['guest'] },
    { id: 'oops', name: 'Carol', tags: [] },
  ],
  meta: { version: 'v2', enabled: true },
})

type User = { id: number; name: string; tags: string[] }

function userSchema(): StandardSchemaV1<unknown, User> {
  return {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate(value) {
        if (typeof value !== 'object' || value === null) {
          return { issues: [{ message: 'expected object' }] }
        }
        const v = value as Record<string, unknown>
        if (typeof v.id !== 'number') return { issues: [{ message: 'id must be number', path: ['id'] }] }
        if (typeof v.name !== 'string') return { issues: [{ message: 'name must be string', path: ['name'] }] }
        if (!Array.isArray(v.tags) || !v.tags.every((t) => typeof t === 'string')) {
          return { issues: [{ message: 'tags must be string[]', path: ['tags'] }] }
        }
        return { value: { id: v.id, name: v.name, tags: v.tags as string[] } }
      },
    },
  }
}

function asyncStringSchema(): StandardSchemaV1<unknown, string> {
  return {
    '~standard': {
      version: 1,
      vendor: 'test',
      async validate(value) {
        await Promise.resolve()
        if (typeof value !== 'string') return { issues: [{ message: 'not a string' }] }
        return { value }
      },
    },
  }
}

test('get_with_schema_returns_typed_value', async (t) => {
  const cursor = await open(memorySource(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  const alice = await cursor.get('/users/0', userSchema())
  assert.deepEqual(alice, { id: 1, name: 'Alice', tags: ['admin', 'staff'] })
})

test('get_with_schema_throws_validation_error_on_failure', async (t) => {
  const cursor = await open(memorySource(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  await assert.rejects(
    () => cursor.get('/users/2', userSchema()),
    (err: unknown) => {
      assert.ok(err instanceof ValidationError)
      assert.equal(err.pointer, '/users/2')
      assert.equal(err.issues[0]?.message, 'id must be number')
      return true
    },
  )
})

test('get_with_async_schema_awaits_validator', async (t) => {
  const cursor = await open(memorySource(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/meta/version', asyncStringSchema()), 'v2')
  await assert.rejects(() => cursor.get('/meta/enabled', asyncStringSchema()), ValidationError)
})

test('has_with_schema_returns_true_only_when_valid', async (t) => {
  const cursor = await open(memorySource(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  assert.equal(await cursor.has('/users/0', userSchema()), true)
  assert.equal(await cursor.has('/users/2', userSchema()), false)
  assert.equal(await cursor.has('/users/99', userSchema()), false)
})

test('scan_with_schema_yields_validated_items_then_throws', async (t) => {
  const cursor = await open(memorySource(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  const seen: User[] = []
  await assert.rejects(
    async () => {
      for await (const u of cursor.scan('/users', userSchema())) seen.push(u)
    },
    (err: unknown) => {
      assert.ok(err instanceof ValidationError)
      assert.equal(err.pointer, '/users/2')
      return true
    },
  )
  assert.deepEqual(
    seen.map((u) => u.name),
    ['Alice', 'Bob'],
  )
})

test('scan_with_schema_completes_when_all_items_valid', async (t) => {
  const doc = JSON.stringify({ tags: ['a', 'b', 'c'] })
  const cursor = await open(memorySource(new TextEncoder().encode(doc)))
  t.after(() => cursor.close())
  const collected: string[] = []
  for await (const tag of cursor.scan('/tags', asyncStringSchema())) collected.push(tag)
  assert.deepEqual(collected, ['a', 'b', 'c'])
})
