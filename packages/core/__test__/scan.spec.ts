import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, eq, gte, and } from '../src/index.ts'
import { memorySource, enc, ORDERS } from './fixtures.ts'

// scan_ iteration over containers, plus the select (projection) and batch options.
// where-filtered scans live in predicate.spec.ts; schema-validated scans in schema.spec.ts.

test('scan_array_elements', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30,40]}')))
  const values: unknown[] = []
  for await (const v of cursor.scan('/xs')) values.push(v)
  assert.deepEqual(values, [10, 20, 30, 40])
})

test('scan_object_members', async () => {
  const cursor = await open(memorySource(enc('{"o":{"a":1,"b":2,"c":3}}')))
  const values: unknown[] = []
  for await (const v of cursor.scan('/o')) values.push(v)
  assert.deepEqual(values.sort(), [1, 2, 3])
})

test('scan_non_container_yields_nothing', async () => {
  const cursor = await open(memorySource(enc('{"scalar":42}')))
  const values: unknown[] = []
  for await (const v of cursor.scan('/scalar')) values.push(v)
  assert.deepEqual(values, [])
})

test('scan_select_single_pointer_yields_bare_values', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const totals: number[] = []
  for await (const total of db.scan('/orders', { select: '/total' })) totals.push(total as number)
  assert.deepEqual(totals, [120, 80, 50, 200, 999])
})

test('scan_select_map_yields_objects_in_declared_order', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const rows: Array<Record<string, unknown>> = []
  for await (const row of db.scan('/orders', { select: { total: '/total', country: '/customer/country' } })) {
    rows.push(row as Record<string, unknown>)
  }
  assert.deepEqual(rows[0], { total: 120, country: 'US' })
  assert.deepEqual(Object.keys(rows[0]), ['total', 'country'])
})

test('scan_select_missing_sub_pointer_yields_null', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const vals: unknown[] = []
  for await (const v of db.scan('/orders', { select: '/nope' })) vals.push(v)
  assert.deepEqual(vals, [null, null, null, null, null])
})

test('scan_batch_yields_arrays', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const sizes: number[] = []
  for await (const batch of db.scan('/orders', { select: '/id', batch: 3 })) {
    sizes.push((batch as unknown[]).length)
  }
  assert.deepEqual(sizes, [3, 2]) // 5 items, batch of 3
})

test('scan_where_select_batch_combined_byCountry_fold', async (t) => {
  // The doc's headline example: filter natively, project, batch, fold in JS.
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const byCountry = new Map<string, number>()
  for await (const rows of db.scan('/orders', {
    where: and(eq('/status', 'paid'), gte('/total', 100)),
    select: { total: '/total', country: '/customer/country' },
    batch: 1024,
  })) {
    for (const row of rows as Array<{ total: number; country: string }>) {
      byCountry.set(row.country, (byCountry.get(row.country) ?? 0) + row.total)
    }
  }
  // paid AND total >= 100 -> a (US, 120), d (DE, 200)
  assert.equal(byCountry.get('US'), 120)
  assert.equal(byCountry.get('DE'), 200)
  assert.equal(byCountry.size, 2)
})

test('scan_batch_rejects_non_positive', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.throws(() => db.scan('/orders', { batch: 0 }), RangeError)
  assert.throws(() => db.scan('/orders', { batch: -1 }), RangeError)
  assert.throws(() => db.scan('/orders', { batch: 1.5 }), RangeError)
})

test('scan_select_rejects_empty_map', async (t) => {
  // An empty `select: {}` would yield one empty object per child silently.
  // Reject at the facade so the failure mode is a clear error - symmetric
  // with the `batch <= 0` rejection above.
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.throws(() => db.scan('/orders', { select: {} }), RangeError)
})

test('scan_select_batch_under_tight_budget_stays_bounded', async (t) => {
  // Projecting + batching a big array under a tight cap stays under the ceiling.
  const rows = Array.from({ length: 4000 }, (_, i) => `{"id":${i},"v":"value-${i}"}`)
  const db = await open(memorySource(enc('[' + rows.join(',') + ']'), 256), { maxResidentChunks: 16 })
  t.after(() => db.close())
  let count = 0
  for await (const batch of db.scan('', { select: '/id', batch: 256 })) count += (batch as unknown[]).length
  assert.equal(count, 4000)
  const stats = db.cacheStats()
  assert.ok(stats.residentBytes + stats.bitmapBytes <= stats.ceilingBytes)
})
