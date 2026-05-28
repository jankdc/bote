import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, DEFAULT_ITER_BATCH } from '../src/index.ts'
import { memorySource, enc, ORDERS } from './fixtures.ts'

// iter_ iteration over containers, plus the select (projection) and batch options.
// schema-validated iters live in schema.spec.ts. Every yield from `.iter` is a
// batch (array) - that contract is exercised explicitly in iter_default_*
// below and assumed by the flatten helper used by the rest.

async function collect<T>(it: AsyncIterable<T[]>): Promise<T[]> {
  const out: T[] = []
  for await (const batch of it) for (const v of batch) out.push(v)
  return out
}

test('iter_array_elements', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30,40]}')))
  assert.deepEqual(await collect(cursor.iter('/xs')), [10, 20, 30, 40])
})

test('iter_on_object_target_throws', async () => {
  const cursor = await open(memorySource(enc('{"o":{"a":1,"b":2,"c":3}}')))
  await assert.rejects(
    (async () => {
      for await (const _ of cursor.iter('/o')) void _
    })(),
    /walk\(\)/,
  )
})

test('iter_non_container_yields_no_batches', async () => {
  // Empty result means *zero* yields, not a single empty batch - keeps the
  // happy-path consumer (`for await (const b of ...) for (const v of b)`)
  // from observing a meaningless `[]`.
  const cursor = await open(memorySource(enc('{"scalar":42}')))
  const batches: unknown[][] = []
  for await (const b of cursor.iter('/scalar')) batches.push(b)
  assert.deepEqual(batches, [])
})

test('iter_default_batch_size_is_DEFAULT_ITER_BATCH', async () => {
  // 2500 items at the default 1000-item batch -> sizes [1000, 1000, 500].
  // Also asserts the exported constant matches the value the native side
  // actually uses, so a mismatch surfaces here instead of a perf cliff.
  assert.equal(DEFAULT_ITER_BATCH, 1000)
  const items = Array.from({ length: 2500 }, (_, i) => i)
  const cursor = await open(memorySource(enc(JSON.stringify({ xs: items }))))
  const sizes: number[] = []
  for await (const batch of cursor.iter('/xs')) sizes.push(batch.length)
  assert.deepEqual(sizes, [1000, 1000, 500])
})

test('iter_default_batch_flushes_partial_final_batch', async () => {
  // Fewer items than the default batch: one yield, exactly that many items.
  const cursor = await open(memorySource(enc('{"xs":[10,20,30,40]}')))
  const batches: number[][] = []
  for await (const batch of cursor.iter('/xs')) batches.push(batch as number[])
  assert.deepEqual(batches, [[10, 20, 30, 40]])
})

test('iter_select_single_pointer_yields_bare_values', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const totals = await collect(db.iter('/orders', { select: '/total' }))
  assert.deepEqual(totals, [120, 80, 50, 200, 999])
})

test('iter_select_map_yields_objects_in_declared_order', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const rows = await collect(
    db.iter('/orders', { select: { total: '/total', country: '/customer/country' } }),
  )
  assert.deepEqual(rows[0], { total: 120, country: 'US' })
  assert.deepEqual(Object.keys(rows[0] as object), ['total', 'country'])
})

test('iter_select_missing_sub_pointer_yields_null', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.deepEqual(await collect(db.iter('/orders', { select: '/nope' })), [null, null, null, null, null])
})

test('iter_batch_override_yields_arrays', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const sizes: number[] = []
  for await (const batch of db.iter('/orders', { select: '/id', batch: 3 })) sizes.push(batch.length)
  assert.deepEqual(sizes, [3, 2]) // 5 items, batch of 3
})

test('iter_select_batch_combined_byCountry_fold', async (t) => {
  // The doc's headline example: project, batch, fold in JS.
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const byCountry = new Map<string, number>()
  for await (const rows of db.iter('/orders', {
    select: { total: '/total', country: '/customer/country' },
    batch: 1024,
  })) {
    for (const row of rows as Array<{ total: number; country: string }>) {
      byCountry.set(row.country, (byCountry.get(row.country) ?? 0) + row.total)
    }
  }
  // All 5 orders: a/c/e -> US (120+50+999=1169), b -> GB (80), d -> DE (200).
  assert.equal(byCountry.get('US'), 1169)
  assert.equal(byCountry.get('GB'), 80)
  assert.equal(byCountry.get('DE'), 200)
  assert.equal(byCountry.size, 3)
})

test('iter_batch_rejects_non_positive', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.throws(() => db.iter('/orders', { batch: 0 }), RangeError)
  assert.throws(() => db.iter('/orders', { batch: -1 }), RangeError)
  assert.throws(() => db.iter('/orders', { batch: 1.5 }), RangeError)
})

test('iter_select_rejects_empty_map', async (t) => {
  // An empty `select: {}` would yield one empty object per child silently.
  // Reject at the facade so the failure mode is a clear error - symmetric
  // with the `batch <= 0` rejection above.
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  assert.throws(() => db.iter('/orders', { select: {} }), RangeError)
})

test('iter_withKey_array_yields_index_value_tuples', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30]}')))
  const pairs = await collect(cursor.iter('/xs', { withIndex: true }))
  assert.deepEqual(pairs, [
    [0, 10],
    [1, 20],
    [2, 30],
  ])
})

test('iter_withKey_with_select_yields_key_and_projected_value', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const rows = await collect(db.iter('/orders', { select: '/total', withIndex: true }))
  assert.deepEqual(rows, [
    [0, 120],
    [1, 80],
    [2, 50],
    [3, 200],
    [4, 999],
  ])
})

test('iter_withKey_with_select_map_yields_key_and_object', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const rows = await collect(
    db.iter('/orders', {
      select: { total: '/total', country: '/customer/country' },
      withIndex: true,
    }),
  )
  assert.equal(rows.length, 5)
  assert.deepEqual(rows[0], [0, { total: 120, country: 'US' }])
  assert.deepEqual(rows[4], [4, { total: 999, country: 'US' }])
})

test('iter_withKey_batch_override_yields_arrays_of_tuples', async (t) => {
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const batches: Array<Array<[unknown, unknown]>> = []
  for await (const batch of db.iter('/orders', { select: '/total', withIndex: true, batch: 3 })) {
    batches.push(batch as Array<[unknown, unknown]>)
  }
  assert.deepEqual(batches, [
    [
      [0, 120],
      [1, 80],
      [2, 50],
    ],
    [
      [3, 200],
      [4, 999],
    ],
  ])
})

test('iter_withKey_with_schema_validates_value_part_only', async (t) => {
  // The schema sees the projected value (a number), not the [key, value] tuple.
  // The key is passed through unchanged in the yielded pair.
  const numberSchema = {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate: (v: unknown) =>
        typeof v === 'number'
          ? { value: v * 10 }
          : { issues: [{ message: 'not a number' }] },
    },
  } as const
  const db = await open(memorySource(enc(ORDERS)))
  t.after(() => db.close())
  const rows = await collect(
    db.iter('/orders', {
      select: '/total',
      withIndex: true,
      schema: numberSchema,
    }),
  )
  assert.deepEqual(rows, [
    [0, 1200],
    [1, 800],
    [2, 500],
    [3, 2000],
    [4, 9990],
  ])
})

test('iter_select_batch_under_tight_budget_stays_bounded', async (t) => {
  // Projecting + batching a big array under a tight cap stays under the ceiling.
  const rows = Array.from({ length: 4000 }, (_, i) => `{"id":${i},"v":"value-${i}"}`)
  const db = await open(memorySource(enc('[' + rows.join(',') + ']'), 256), { maxResidentChunks: 16 })
  t.after(() => db.close())
  let count = 0
  for await (const batch of db.iter('', { select: '/id', batch: 256 })) count += batch.length
  assert.equal(count, 4000)
  const stats = db.cacheStats()
  assert.ok(stats.residentBytes + stats.bitmapBytes <= stats.ceilingBytes)
})
