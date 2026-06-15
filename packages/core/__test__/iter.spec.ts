import { test } from 'node:test';
import assert from 'node:assert/strict';

import { open, DEFAULT_ITER_BATCH, MAX_ITER_BATCH, PathError, type SeekableSource } from '../src/index.ts';
import { memorySource, enc, ORDERS } from './fixtures.ts';

function countingSource(data: Uint8Array, chunkBytes: number): { source: SeekableSource; reads: { n: number } } {
  const reads = { n: 0 };
  const source: SeekableSource = {
    seekable: true,
    open: () =>
      Promise.resolve({
        size: data.length,
        chunkBytes,
        read: (offset, length) => {
          reads.n++;
          const slice = data.subarray(offset, Math.min(offset + length, data.length));
          return Promise.resolve({ data: slice, eof: offset + slice.length >= data.length });
        },
      }),
  };
  return { source, reads };
}

test('iter_array_yields_elements', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30,40]}')));
  assert.deepEqual(await cursor.iter('xs').toArray(), [10, 20, 30, 40]);
});

test('iter_item_iteration_yields_items_in_order_across_batch_boundaries', async () => {
  // With a batch smaller than the element count, the default item loop must
  // still see every element once, in document order, seamlessly across the
  // internal batch seams.
  const items = Array.from({ length: 25 }, (_, i) => i);
  const cursor = await open(memorySource(enc(JSON.stringify({ xs: items }))));
  const seen: number[] = [];
  for await (const x of cursor.iter('xs', { batch: 4 })) {
    seen.push(x as number);
  }
  assert.deepEqual(seen, items);
});

test('iter_item_and_batches_agree_on_contents', async () => {
  // The two consumption paths must flatten to the same sequence.
  const items = Array.from({ length: 2500 }, (_, i) => i);
  const doc = enc(JSON.stringify({ xs: items }));
  const byItem: number[] = [];
  const itemCursor = await open(memorySource(doc));
  for await (const x of itemCursor.iter('xs')) {
    byItem.push(x as number);
  }
  const byBatch: number[] = [];
  const batchCursor = await open(memorySource(doc));
  for await (const batch of batchCursor.iter('xs').raw()) {
    for (const x of batch) {
      byBatch.push(x as number);
    }
  }
  assert.deepEqual(byItem, byBatch);
  assert.deepEqual(byItem, items);
});

test('iter_batches_default_batch_size_is_DEFAULT_ITER_BATCH', async () => {
  // 2500 items at the default 1000-item batch -> sizes [1000, 1000, 500].
  // Also asserts the exported constant matches the value the native side
  // actually uses, so a mismatch surfaces here instead of a perf cliff.
  assert.equal(DEFAULT_ITER_BATCH, 1000);
  const items = Array.from({ length: 2500 }, (_, i) => i);
  const cursor = await open(memorySource(enc(JSON.stringify({ xs: items }))));
  const sizes: number[] = [];
  for await (const batch of cursor.iter('xs').raw()) {
    sizes.push(batch.length);
  }
  assert.deepEqual(sizes, [1000, 1000, 500]);
});

test('iter_batches_flushes_partial_final_batch', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30,40]}')));
  const batches: number[][] = [];
  for await (const batch of cursor.iter('xs').raw()) {
    batches.push(batch as number[]);
  }
  assert.deepEqual(batches, [[10, 20, 30, 40]]);
});

test('iter_select_single_path_yields_bare_values', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const totals = await db.iter('orders', { select: ['total'] }).toArray();
  assert.deepEqual(totals, [120, 80, 50, 200, 999]);
});

test('iter_select_map_yields_objects_in_declared_order', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const rows = await db.iter('orders', { select: { total: ['total'], country: ['customer', 'country'] } }).toArray();
  assert.deepEqual(rows[0], { total: 120, country: 'US' });
  assert.deepEqual(Object.keys(rows[0] as object), ['total', 'country']);
});

test('iter_select_bare_segment_is_shorthand_for_one_segment_path', async (t) => {
  // `select: 'id'` == `select: ['id']`; `select: 0` == `select: [0]`.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.deepEqual(await db.iter('orders', { select: 'total' }).toArray(), [120, 80, 50, 200, 999]);
  const matrix = await open(memorySource(enc('{"rows":[[10,20],[30,40],[50,60]]}')));
  t.after(() => matrix.close());
  assert.deepEqual(await matrix.iter('rows', { select: 0 }).toArray(), [10, 30, 50]);
});

test('iter_select_map_infers_keys_and_accepts_bare_segment_subpaths', async (t) => {
  // The map literal `{ id: 'id', country: ['customer', 'country'] }` should
  // (a) accept the bare-segment shorthand on the `id` field at runtime, and
  // (b) infer the yielded item type as `{ id: unknown, country: unknown }`
  //     so unknown keys (`row.nope`) are a compile error. The type assertion
  //     below is the load-bearing part - tsc rejects the spec if inference
  //     widens to `unknown`.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const rows = await db.iter('orders', { select: { id: 'id', country: ['customer', 'country'] } }).toArray();
  const first: { id: unknown; country: unknown } = rows[0];
  assert.equal(typeof first.id, 'string');
  assert.equal(first.country, 'US');
  assert.deepEqual(Object.keys(first), ['id', 'country']);
});

test('iter_select_missing_sub_path_yields_null', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.deepEqual(await db.iter('orders', { select: ['nope'] }).toArray(), [null, null, null, null, null]);
});

test('iter_select_batch_combined_byCountry_fold', async (t) => {
  // The doc's headline example: project, batch, fold in JS. Pins the batch shape,
  // so it iterates `.raw()`.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const byCountry = new Map<string, number>();
  for await (const rows of db
    .iter('orders', {
      select: { total: ['total'], country: ['customer', 'country'] },
      batch: 1024,
    })
    .raw()) {
    for (const row of rows as Array<{ total: number; country: string }>) {
      byCountry.set(row.country, (byCountry.get(row.country) ?? 0) + row.total);
    }
  }
  // All 5 orders: a/c/e -> US (120+50+999=1169), b -> GB (80), d -> DE (200).
  assert.equal(byCountry.get('US'), 1169);
  assert.equal(byCountry.get('GB'), 80);
  assert.equal(byCountry.get('DE'), 200);
  assert.equal(byCountry.size, 3);
});

test('iter_select_batch_with_small_chunks_stays_correct', async (t) => {
  const rows = Array.from({ length: 4000 }, (_, i) => `{"id":${i},"v":"value-${i}"}`);
  const db = await open(memorySource(enc('[' + rows.join(',') + ']'), 256));
  t.after(() => db.close());
  let count = 0;
  for await (const batch of db.iter({ select: ['id'], batch: 256 }).raw()) {
    count += batch.length;
  }
  assert.equal(count, 4000);
});

test('iter_select_rejects_empty_map', async (t) => {
  // An empty `select: {}` would yield one empty object per child silently.
  // Reject at the facade so the failure mode is a clear error - symmetric
  // with the `batch <= 0` rejection above.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.throws(() => db.iter('orders', { select: {} }), RangeError);
});

test('iter_select_rejects_empty_sub_path', async (t) => {
  // An empty sub-path (`select: []`, or a map field mapped to `[]`) would
  // project the whole child, silently defeating select's purpose. Reject it
  // at the facade like the empty-map case above.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.throws(() => db.iter('orders', { select: [] }), RangeError);
  assert.throws(() => db.iter('orders', { select: { whole: [] } }), RangeError);
});

test('iter_select_rejects_non_path_values', async (t) => {
  // A non-segment/path select (null, boolean) or a field mapped to one used to
  // leak a raw `Object.entries(null)` / `Cannot read properties` deref or a
  // native serde error. The facade rejects them with a clean TypeError.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  // @ts-expect-error select must be a segment, path, or field map
  assert.throws(() => db.iter('orders', { select: null }), TypeError);
  // @ts-expect-error
  assert.throws(() => db.iter('orders', { select: true }), TypeError);
  // @ts-expect-error a field value must be a segment or path
  assert.throws(() => db.iter('orders', { select: { a: null } }), TypeError);
  // @ts-expect-error a nested object is not a path
  assert.throws(() => db.iter('orders', { select: { a: { nested: 1 } } }), TypeError);
});

test('iter_rejects_non_boolean_withKey', async (t) => {
  // A non-boolean withKey is rejected at the facade with a TypeError naming the
  // option, rather than passed through to surface a raw napi error.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  // @ts-expect-error withKey must be a boolean
  assert.throws(() => db.iter('orders', { withKey: 'yes' }), /iter: withKey must be a boolean/);
});

test('iter_rejects_invalid_onInvalid', async (t) => {
  // onInvalid was type-only: a typo like 'SKIP' silently fell through to 'throw'.
  // It is now runtime-validated like its sibling knobs.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  // @ts-expect-error onInvalid must be 'throw' or 'skip'
  assert.throws(() => db.iter('orders', { onInvalid: 'SKIP' }), /onInvalid must be/);
  // @ts-expect-error
  assert.throws(() => db.iter('orders', { onInvalid: 'bogus' }), /onInvalid must be/);
});

test('iter_batches_override_yields_arrays', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const sizes: number[] = [];
  for await (const batch of db.iter('orders', { select: ['id'], batch: 3 }).raw()) {
    sizes.push(batch.length);
  }
  assert.deepEqual(sizes, [3, 2]); // 5 items, batch of 3
});

test('iter_batch_rejects_non_positive', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.throws(() => db.iter('orders', { batch: 0 }), RangeError);
  assert.throws(() => db.iter('orders', { batch: -1 }), RangeError);
  assert.throws(() => db.iter('orders', { batch: 1.5 }), RangeError);
});

test('iter_batch_rejects_above_max', async (t) => {
  // An unbounded batch reserves a native Vec of that capacity, so a huge value
  // could over-allocate or abort the process. The facade caps it first.
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.throws(() => db.iter('orders', { batch: MAX_ITER_BATCH + 1 }), RangeError);
  assert.throws(() => db.iter('orders', { batch: 1e9 }), RangeError);
  assert.throws(() => db.iter('orders', { batch: 2 ** 53 }), RangeError);
  // The cap itself is accepted (constructing the iterator must not throw).
  assert.doesNotThrow(() => db.iter('orders', { batch: MAX_ITER_BATCH }));
});

test('iter_withKey_array_yields_index_value_tuples', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30]}')));
  const pairs = await cursor.iter('xs', { withKey: true }).toArray();
  assert.deepEqual(pairs, [
    [0, 10],
    [1, 20],
    [2, 30],
  ]);
});

test('iter_withKey_with_select_yields_index_and_projected_value', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const rows = await db.iter('orders', { select: ['total'], withKey: true }).toArray();
  assert.deepEqual(rows, [
    [0, 120],
    [1, 80],
    [2, 50],
    [3, 200],
    [4, 999],
  ]);
});

test('iter_withKey_with_select_map_yields_index_and_object', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const rows = await db
    .iter('orders', {
      select: { total: ['total'], country: ['customer', 'country'] },
      withKey: true,
    })
    .toArray();
  assert.equal(rows.length, 5);
  assert.deepEqual(rows[0], [0, { total: 120, country: 'US' }]);
  assert.deepEqual(rows[4], [4, { total: 999, country: 'US' }]);
});

test('iter_withKey_batches_override_yields_arrays_of_tuples', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const batches: Array<Array<[unknown, unknown]>> = [];
  for await (const batch of db.iter('orders', { select: ['total'], withKey: true, batch: 3 }).raw()) {
    batches.push(batch as Array<[unknown, unknown]>);
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
  ]);
});

test('iter_withKey_with_schema_validates_value_part_only', async (t) => {
  // The schema sees the projected value (a number), not the [index, value] tuple.
  // The index is passed through unchanged in the yielded pair.
  const numberSchema = {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate: (v: unknown) => (typeof v === 'number' ? { value: v * 10 } : { issues: [{ message: 'not a number' }] }),
    },
  } as const;
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const rows = await db
    .iter('orders', {
      select: ['total'],
      withKey: true,
      schema: numberSchema,
    })
    .toArray();
  assert.deepEqual(rows, [
    [0, 1200],
    [1, 800],
    [2, 500],
    [3, 2000],
    [4, 9990],
  ]);
});

test('iter_withKey_with_skip_preserves_source_indices_across_skipped_items', async () => {
  const evenOnly = {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate: (v: unknown) =>
        typeof v === 'number' && v % 2 === 0 ? { value: v } : { issues: [{ message: 'odd' }] },
    },
  } as const;
  const cursor = await open(memorySource(enc('{"xs":[10,11,12,13,14]}')));
  const pairs = await cursor.iter('xs', { schema: evenOnly, withKey: true, onInvalid: 'skip' }).toArray();
  assert.deepEqual(pairs, [
    [0, 10],
    [2, 12],
    [4, 14],
  ]);
});

test('iter_scalar_target_throws_PathError', async () => {
  // A container operation aimed at a present scalar is a shape error, surfaced
  // on first iteration. Holds for objects and arrays alike (iter is kind-agnostic).
  const cursor = await open(memorySource(enc('{"scalar":42}')));
  await assert.rejects(
    (async () => {
      for await (const _ of cursor.iter('scalar')) {
        void _;
      }
    })(),
    PathError,
  );
});

test('iter_missing_path_yields_zero_items', async () => {
  // A clean miss (unresolved path) yields zero items - the not-found sentinel
  // for iter, distinct from a present-scalar target which throws.
  const cursor = await open(memorySource(enc('{"xs":[1,2]}')));
  assert.deepEqual(await cursor.iter('nope').toArray(), []);
  const batches: unknown[][] = [];
  for await (const b of cursor.iter('nope').raw()) {
    batches.push(b);
  }
  assert.deepEqual(batches, []);
});

test('iter_early_break_releases_scan_without_faulting_whole_doc', async () => {
  const items = Array.from({ length: 5000 }, (_, i) => `{"id":${i},"name":"item-${i}"}`);
  const data = enc('{"items":[' + items.join(',') + ']}');

  const full = countingSource(data, 256);
  const fc = await open(full.source);
  let seen = 0;
  for await (const _ of fc.iter('items')) {
    seen++;
  }
  await fc.close();
  assert.equal(seen, 5000);
  const fullReads = full.reads.n;

  const partial = countingSource(data, 256);
  const pc = await open(partial.source);
  const got: unknown[] = [];
  for await (const item of pc.iter('items', { batch: 1 })) {
    got.push(item);
    if (got.length === 3) {
      break;
    }
  }
  await pc.close();
  assert.equal(got.length, 3);
  assert.ok(
    partial.reads.n < fullReads / 10,
    `early break faulted ${partial.reads.n} reads; a released scan should be far below the full walk's ${fullReads}`,
  );
});

// iter_object_ folds in the cases that used to live in walk.spec.ts: iter is now
// kind-agnostic, so an object target yields member values (and, with withKey,
// [name, value] tuples) in document order.

test('iter_object_yields_member_values', async () => {
  const cursor = await open(memorySource(enc('{"first":1,"second":"two","third":[3,4]}')));
  assert.deepEqual(await cursor.iter().toArray(), [1, 'two', [3, 4]]);
});

test('iter_object_withKey_yields_name_value_pairs', async () => {
  const cursor = await open(memorySource(enc('{"first":1,"second":"two","third":[3,4]}')));
  const pairs = await cursor.iter({ withKey: true }).toArray();
  assert.deepEqual(pairs, [
    ['first', 1],
    ['second', 'two'],
    ['third', [3, 4]],
  ]);
});

test('iter_object_withKey_preserves_duplicate_keys', async () => {
  // A source object can carry duplicate keys; tuple yields preserve every
  // occurrence (unlike JSON.parse, which keeps only the last). A desirable divergence.
  // TODO: Re-examine this we want to avoid different behaviour from JSON.parse
  const cursor = await open(memorySource(enc('{"a":1,"a":2,"b":3}')));
  const pairs = await cursor.iter({ withKey: true }).toArray();
  assert.deepEqual(pairs, [
    ['a', 1],
    ['a', 2],
    ['b', 3],
  ]);
});

test('iter_object_withKey_with_select_projects_each_value', async (t) => {
  const data = enc('{"users":{"alice":{"name":"Alice","age":30},"bob":{"name":"Bob","age":25}}}');
  const cursor = await open(memorySource(data));
  t.after(() => cursor.close());
  const pairs = await cursor.iter('users', { withKey: true, select: 'name' }).toArray();
  assert.deepEqual(pairs, [
    ['alice', 'Alice'],
    ['bob', 'Bob'],
  ]);
});

test('iter_object_withKey_then_hop_descends_into_a_member', async (t) => {
  // The interim lazy-descent recipe: withKey + select to learn the keys, then
  // hop(key) to descend into the few members you care about.
  const data = enc('{"users":{"alice":{"name":"Alice","age":30},"bob":{"name":"Bob","age":25}}}');
  const cursor = await open(memorySource(data));
  t.after(() => cursor.close());
  const keys: string[] = [];
  for await (const [key] of cursor.iter('users', { withKey: true, select: 'name' })) {
    keys.push(key as string);
  }
  assert.deepEqual(keys, ['alice', 'bob']);
  const bob = await cursor.hop('users', keys[1]);
  assert.ok(bob);
  assert.equal(await bob.get('age'), 25);
});

test('iter_object_withKey_large_with_small_chunks', async () => {
  const members = Array.from({ length: 100 }, (_, i) => `"item-${i}":{"id":${i},"name":"item-${i}"}`);
  const data = enc('{' + members.join(',') + '}');
  const cursor = await open(memorySource(data, 128));
  const seen: Array<[string, number]> = [];
  for await (const [key, id] of cursor.iter({ withKey: true, select: 'id' })) {
    seen.push([key as string, id as number]);
  }
  assert.equal(seen.length, 100);
  assert.deepEqual(seen[0], ['item-0', 0]);
  assert.deepEqual(seen[99], ['item-99', 99]);
});

test('iter_object_missing_path_yields_zero_items', async () => {
  const cursor = await open(memorySource(enc('{"o":{"a":1}}')));
  assert.deepEqual(await cursor.iter('missing', { withKey: true }).toArray(), []);
});

test('iter_map_transforms_each_item_with_zero_based_counter', async () => {
  const cursor = await open(memorySource(enc('{"xs":[10,20,30]}')));
  assert.deepEqual(
    await cursor
      .iter('xs')
      .map((x) => (x as number) * 2)
      .toArray(),
    [20, 40, 60],
  );
  const indexed = await open(memorySource(enc('{"xs":[10,20,30]}')));
  assert.deepEqual(
    await indexed
      .iter('xs')
      .map((_x, i) => i)
      .toArray(),
    [0, 1, 2],
  );
});

test('iter_map_awaits_async_callback', async () => {
  const cursor = await open(memorySource(enc('{"xs":[1,2,3]}')));
  const out = await cursor
    .iter('xs')
    .map((x) => Promise.resolve((x as number) + 100))
    .toArray();
  assert.deepEqual(out, [101, 102, 103]);
});

test('iter_filter_keeps_matching_items', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const big = await db
    .iter('orders', { select: 'total' })
    .filter((n) => (n as number) >= 120)
    .toArray();
  assert.deepEqual(big, [120, 200, 999]);
});

test('iter_filter_awaits_async_predicate', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const paid = await db
    .iter('orders')
    .filter((o) => Promise.resolve((o as { status: string }).status === 'paid'))
    .map((o) => (o as { id: string }).id)
    .toArray();
  assert.deepEqual(paid, ['a', 'c', 'd']);
});

test('iter_filter_type_guard_narrows_element_type', async () => {
  const cursor = await open(memorySource(enc('{"xs":[1,"two",3,"four"]}')));
  const nums: number[] = await cursor
    .iter('xs')
    .filter((x): x is number => typeof x === 'number')
    .toArray();
  assert.deepEqual(nums, [1, 3]);
});

test('iter_take_yields_at_most_limit_then_stops', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.deepEqual(await db.iter('orders', { select: 'id' }).take(2).toArray(), ['a', 'b']);
  const all = await open(memorySource(enc(ORDERS)));
  t.after(() => all.close());
  assert.deepEqual(await all.iter('orders', { select: 'id' }).take(99).toArray(), ['a', 'b', 'c', 'd', 'e']);
});

test('iter_take_zero_yields_nothing', async () => {
  const cursor = await open(memorySource(enc('{"xs":[1,2,3]}')));
  assert.deepEqual(await cursor.iter('xs').take(0).toArray(), []);
});

test('iter_drop_skips_leading_items', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  assert.deepEqual(await db.iter('orders', { select: 'id' }).drop(3).toArray(), ['d', 'e']);
});

test('iter_chained_filter_map_take_composes_lazily', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const ids = await db
    .iter('orders')
    .filter((o) => (o as { total: number }).total > 60)
    .map((o) => (o as { id: string }).id)
    .take(2)
    .toArray();
  assert.deepEqual(ids, ['a', 'b']);
});

test('iter_find_returns_first_match_else_undefined', async (t) => {
  const db = await open(memorySource(enc(ORDERS)));
  t.after(() => db.close());
  const hit = await db.iter('orders').find((o) => (o as { total: number }).total === 50);
  assert.deepEqual(hit, { id: 'c', status: 'paid', total: 50, customer: { country: 'US' } });
  const miss = await open(memorySource(enc(ORDERS)));
  t.after(() => miss.close());
  assert.equal(await miss.iter('orders').find((o) => (o as { total: number }).total < 0), undefined);
});

test('iter_some_and_every_short_circuit_on_predicate', async (t) => {
  const some = await open(memorySource(enc(ORDERS)));
  t.after(() => some.close());
  assert.equal(await some.iter('orders', { select: 'total' }).some((n) => (n as number) > 900), true);
  const someNone = await open(memorySource(enc(ORDERS)));
  t.after(() => someNone.close());
  assert.equal(await someNone.iter('orders', { select: 'total' }).some((n) => (n as number) > 9999), false);
  const every = await open(memorySource(enc(ORDERS)));
  t.after(() => every.close());
  assert.equal(await every.iter('orders', { select: 'total' }).every((n) => (n as number) > 0), true);
  const everyFail = await open(memorySource(enc(ORDERS)));
  t.after(() => everyFail.close());
  assert.equal(await everyFail.iter('orders', { select: 'total' }).every((n) => (n as number) > 100), false);
});

test('iter_transform_batches_regroup_to_batch_size', async () => {
  const cursor = await open(memorySource(enc('{"xs":[1,2,3,4,5]}')));
  const sizes: number[] = [];
  for await (const batch of cursor
    .iter('xs', { batch: 2 })
    .map((x) => (x as number) + 1)
    .raw()) {
    sizes.push(batch.length);
  }
  assert.deepEqual(sizes, [2, 2, 1]);
});

test('iter_take_stops_faulting_chunks_early', async () => {
  const items = Array.from({ length: 400 }, (_, i) => i);
  const data = enc(JSON.stringify({ xs: items }));
  const full = countingSource(data, 64);
  const fullCursor = await open(full.source);
  await fullCursor.iter('xs').toArray();
  const taken = countingSource(data, 64);
  const takeCursor = await open(taken.source);
  await takeCursor.iter('xs', { batch: 4 }).take(2).toArray();
  assert.ok(taken.reads.n < full.reads.n, `take faulted ${taken.reads.n} chunks, full walk faulted ${full.reads.n}`);
});

test('iter_find_stops_faulting_chunks_after_match', async () => {
  const items = Array.from({ length: 400 }, (_, i) => i);
  const data = enc(JSON.stringify({ xs: items }));
  const full = countingSource(data, 64);
  const fullCursor = await open(full.source);
  await fullCursor.iter('xs').toArray();
  const found = countingSource(data, 64);
  const findCursor = await open(found.source);
  const hit = await findCursor.iter('xs', { batch: 4 }).find((x) => (x as number) === 1);
  assert.equal(hit, 1);
  assert.ok(found.reads.n < full.reads.n, `find faulted ${found.reads.n} chunks, full walk faulted ${full.reads.n}`);
});
