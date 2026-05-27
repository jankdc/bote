import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, eq, gte, exists, and } from '../src/index.ts'
import { memorySource, enc, ORDERS } from './fixtures.ts'

// where_ predicate pushdown across count / scan / walk, and the totality contract.
// where combined with schema validation lives in schema.spec.ts.

test('where_count_eq', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.equal(await db.count('/orders'), 5)
  assert.equal(await db.count('/orders', { where: eq('/status', 'paid') }), 3)
  assert.equal(await db.count('/orders', { where: eq('/status', 'refunded') }), 1)
})

test('where_count_and_combines', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  // paid AND total >= 100 -> orders a (120) and d (200)
  const n = await db.count('/orders', { where: and(eq('/status', 'paid'), gte('/total', 100)) })
  assert.equal(n, 2)
})

test('where_scan_yields_only_matches_in_order', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const ids: string[] = []
  for await (const order of db.scan('/orders', { where: eq('/status', 'paid') })) {
    ids.push((order as { id: string }).id)
  }
  assert.deepEqual(ids, ['a', 'c', 'd'])
})

test('where_walk_filters_then_drills_in', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const countries: string[] = []
  for await (const order of db.walk('/orders', { where: eq('/status', 'refunded') })) {
    countries.push((await order.get('/customer/country')) as string)
  }
  assert.deepEqual(countries, ['GB'])
})

test('where_is_total_and_non_throwing', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  // missing sub-pointer -> false (no match), never throws
  assert.equal(await db.count('/orders', { where: eq('/nope', 'x') }), 0)
  // type mismatch (number comparison against a string value) -> false
  assert.equal(await db.count('/orders', { where: gte('/status', 100) }), 0)
  // exists is true whenever the sub-pointer resolves
  assert.equal(await db.count('/orders', { where: exists('/customer/country') }), 5)
})

test('where_sub_pointer_is_compile_validated', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  // @ts-expect-error predicate sub-pointer must be a valid JSON pointer
  eq('status', 'paid')
  assert.equal(await db.count('/orders', { where: eq('/status', 'paid') }), 3)
})

test('where_filters_large_array_under_tight_budget', async (t) => {
  const rows = Array.from({ length: 4000 }, (_, i) => `{"id":${i},"keep":${i % 100 === 0}}`)
  const db = await open(memorySource(enc('[' + rows.join(',') + ']'), 256), { maxResidentChunks: 16 })
  t.after(() => db.close())
  let n = 0
  for await (const _row of db.scan('', { where: eq('/keep', true) })) n += 1
  assert.equal(n, 40)
  const stats = db.cacheStats()
  assert.ok(stats.residentBytes + stats.bitmapBytes <= stats.ceilingBytes)
})
