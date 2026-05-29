import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, ValidationError, formatPath, type StandardSchemaV1 } from '../src/index.ts'
import { memorySource, enc, USERS_WITH_INVALID, MIXED, ORDERS } from './fixtures.ts'

// Standard Schema validation across get / has / iter, including onInvalid, batch,
// and select combinations. USERS_WITH_INVALID fails at users[2]; MIXED at rows[2].

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

function numberN(): StandardSchemaV1<unknown, { n: number }> {
  return {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate(value) {
        const o = value as Record<string, unknown>
        if (typeof o?.n !== 'number') return { issues: [{ message: 'n must be a number' }] }
        return { value: { n: o.n } }
      },
    },
  }
}

function asyncNumberN(): StandardSchemaV1<unknown, { n: number }> {
  return {
    '~standard': {
      version: 1,
      vendor: 'test',
      async validate(value) {
        await Promise.resolve()
        const o = value as Record<string, unknown>
        if (typeof o?.n !== 'number') return { issues: [{ message: 'n must be a number' }] }
        return { value: { n: o.n } }
      },
    },
  }
}

test('schema_get_returns_typed_value', async (t) => {
  const cursor = await open(memorySource(enc(USERS_WITH_INVALID)))
  t.after(() => cursor.close())
  const alice = await cursor.get('users', 0, userSchema())
  assert.deepEqual(alice, { id: 1, name: 'Alice', tags: ['admin', 'staff'] })
})

test('schema_get_throws_on_invalid', async (t) => {
  const cursor = await open(memorySource(enc(USERS_WITH_INVALID)))
  t.after(() => cursor.close())
  await assert.rejects(
    () => cursor.get('users', 2, userSchema()),
    (err: unknown) => {
      assert.ok(err instanceof ValidationError)
      assert.deepEqual(err.path, ['users', 2])
      assert.equal(err.issues[0]?.message, 'id must be number')
      return true
    },
  )
})

test('schema_get_async_awaits_validator', async (t) => {
  const cursor = await open(memorySource(enc(USERS_WITH_INVALID)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('meta', 'version', asyncStringSchema()), 'v2')
  await assert.rejects(() => cursor.get('meta', 'enabled', asyncStringSchema()), ValidationError)
})

test('schema_has_true_only_when_valid', async (t) => {
  const cursor = await open(memorySource(enc(USERS_WITH_INVALID)))
  t.after(() => cursor.close())
  assert.equal(await cursor.has('users', 0, userSchema()), true)
  assert.equal(await cursor.has('users', 2, userSchema()), false)
  assert.equal(await cursor.has('users', 99, userSchema()), false)
})

test('schema_iter_yields_valid_then_throws', async (t) => {
  // `batch: 1` exposes per-item yield ordering: the iterator emits each
  // user before validating the next, so Alice and Bob land in `seen`
  // before user 2 trips the validator. With the default batch all three
  // would be grouped into one batch and the throw would drop that
  // in-progress batch entirely - see `schema_iter_default_batch_throw_loses_partial`.
  const cursor = await open(memorySource(enc(USERS_WITH_INVALID)))
  t.after(() => cursor.close())
  const seen: User[] = []
  await assert.rejects(
    async () => {
      for await (const batch of cursor.iter('users', { schema: userSchema(), batch: 1 })) {
        for (const u of batch) seen.push(u)
      }
    },
    (err: unknown) => {
      assert.ok(err instanceof ValidationError)
      assert.deepEqual(err.path, ['users', 2])
      return true
    },
  )
  assert.deepEqual(
    seen.map((u) => u.name),
    ['Alice', 'Bob'],
  )
})

test('schema_iter_default_batch_throw_loses_partial', async (t) => {
  // Documents the new tradeoff: when validation throws mid-batch, the
  // batch is never yielded - earlier-validated items in the same batch
  // are not observable. Users who need per-item observability set `batch: 1`.
  const cursor = await open(memorySource(enc(USERS_WITH_INVALID)))
  t.after(() => cursor.close())
  const seen: User[] = []
  await assert.rejects(async () => {
    for await (const batch of cursor.iter('users', userSchema())) {
      for (const u of batch) seen.push(u)
    }
  }, ValidationError)
  assert.deepEqual(seen, [])
})

test('schema_iter_completes_when_all_valid', async (t) => {
  const doc = JSON.stringify({ tags: ['a', 'b', 'c'] })
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  const collected: string[] = []
  for await (const batch of cursor.iter('tags', asyncStringSchema())) {
    for (const tag of batch) collected.push(tag)
  }
  assert.deepEqual(collected, ['a', 'b', 'c'])
})

test('schema_iter_skip_filters_invalid', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const ns: number[] = []
  for await (const batch of db.iter('rows', { schema: numberN(), onInvalid: 'skip' })) {
    for (const row of batch) ns.push(row.n)
  }
  assert.deepEqual(ns, [1, 2, 4]) // the invalid row is dropped, not thrown
})

test('schema_iter_skip_async_validator', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const ns: number[] = []
  for await (const batch of db.iter('rows', { schema: asyncNumberN(), onInvalid: 'skip' })) {
    for (const row of batch) ns.push(row.n)
  }
  assert.deepEqual(ns, [1, 2, 4])
})

test('schema_iter_select_skip_validates_projected_shape', async (t) => {
  // select reshapes each child to { n }, then the schema validates that shape.
  const doc = JSON.stringify({ rows: [{ v: 1 }, { v: 'bad' }, { v: 3 }] })
  const db = await open(memorySource(enc(doc)))
  t.after(() => db.close())
  const ns: number[] = []
  for await (const batch of db.iter('rows', { select: { n: ['v'] }, schema: numberN(), onInvalid: 'skip' })) {
    for (const row of batch) ns.push(row.n)
  }
  assert.deepEqual(ns, [1, 3])
})

test('schema_iter_batch_skip_shrinks_batches', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const batches: number[][] = []
  for await (const b of db.iter('rows', { schema: numberN(), onInvalid: 'skip', batch: 2 })) {
    batches.push(b.map((r) => r.n))
  }
  // native batches the raw rows [1,2],[bad,4]; skip drops `bad` -> [[1,2],[4]]
  assert.deepEqual(batches, [[1, 2], [4]])
})

test('schema_iter_batch_throws_on_invalid', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  await assert.rejects(async () => {
    for await (const _b of db.iter('rows', { schema: numberN(), batch: 2 })) {
      // consume
    }
  }, ValidationError)
})

test('schema_iter_validates_all_yields', async (t) => {
  // The schema validates every yielded child.
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const orderId: StandardSchemaV1<unknown, { id: string }> = {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate(value) {
        const o = value as Record<string, unknown>
        if (typeof o.id !== 'string') return { issues: [{ message: 'id must be string' }] }
        return { value: { id: o.id } }
      },
    },
  }
  const ids: string[] = []
  for await (const batch of db.iter('orders', { schema: orderId })) {
    for (const order of batch) ids.push(order.id)
  }
  assert.deepEqual(ids, ['a', 'b', 'c', 'd', 'e'])
})

test('formatPath_renders_dot_bracket_notation', () => {
  // The string in ValidationError.message goes through this; mixed types
  // and tricky keys (empty, with dots, with quotes) need to be unambiguous.
  assert.equal(formatPath([]), '(root)')
  assert.equal(formatPath(['users', 0, 'name']), 'users[0].name')
  assert.equal(formatPath(['orders', 3, 'customer', 'country']), 'orders[3].customer.country')
  // Non-identifier keys fall back to quoted bracket form.
  assert.equal(formatPath(['a.b']), '["a.b"]')
  assert.equal(formatPath(['']), '[""]')
  assert.equal(formatPath(['ok', 'a/b', 0]), 'ok["a/b"][0]')
})
