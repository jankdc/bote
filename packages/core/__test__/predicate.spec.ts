import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, fromBuffer, eq, gte, exists, and, type Source, type StandardSchemaV1 } from '../src/index.ts'

function memorySource(data: Uint8Array, chunkBytes?: number): Source {
  return fromBuffer(data, chunkBytes === undefined ? undefined : { chunkBytes })
}

const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

const ORDERS = JSON.stringify({
  orders: [
    { id: 'a', status: 'paid', total: 120, customer: { country: 'US' } },
    { id: 'b', status: 'refunded', total: 80, customer: { country: 'GB' } },
    { id: 'c', status: 'paid', total: 50, customer: { country: 'US' } },
    { id: 'd', status: 'paid', total: 200, customer: { country: 'DE' } },
    { id: 'e', status: 'pending', total: 999, customer: { country: 'US' } },
  ],
})

test('count_with_where_eq', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.equal(await db.count('/orders'), 5)
  assert.equal(await db.count('/orders', { where: eq('/status', 'paid') }), 3)
  assert.equal(await db.count('/orders', { where: eq('/status', 'refunded') }), 1)
})

test('count_with_where_and_combines', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  // paid AND total >= 100 -> orders a (120) and d (200)
  const n = await db.count('/orders', { where: and(eq('/status', 'paid'), gte('/total', 100)) })
  assert.equal(n, 2)
})

test('scan_with_where_yields_only_matches_in_order', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const ids: string[] = []
  for await (const order of db.scan('/orders', { where: eq('/status', 'paid') })) {
    ids.push((order as { id: string }).id)
  }
  assert.deepEqual(ids, ['a', 'c', 'd'])
})

test('walk_with_where_filters_then_drills_in', async (t) => {
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

test('predicate_sub_pointer_is_compile_validated', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  // @ts-expect-error predicate sub-pointer must be a valid JSON pointer
  eq('status', 'paid')
  assert.equal(await db.count('/orders', { where: eq('/status', 'paid') }), 3)
})

test('scan_where_with_schema_validates_filtered_yields', async (t) => {
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
  for await (const order of db.scan('/orders', { where: eq('/status', 'paid'), schema: orderId })) {
    ids.push(order.id)
  }
  assert.deepEqual(ids, ['a', 'c', 'd'])
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
