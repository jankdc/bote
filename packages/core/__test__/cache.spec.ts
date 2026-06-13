import { test } from 'node:test';
import assert from 'node:assert/strict';

import { open, type SeekableSource } from '../src/index.ts';
import { memorySource, enc, bigObject } from './fixtures.ts';

/** A `SeekableSource` that counts `read` calls, so the cache's effect on chunk faulting
 *  is observable from the facade (the only public signal - there is no
 *  `cacheStats()`). `reads.n` is the live count; assign 0 to reset it. */
function countingSource(data: Uint8Array, chunkBytes: number): { source: SeekableSource; reads: { n: number } } {
  const reads = { n: 0 };
  const source: SeekableSource = {
    open: () =>
      Promise.resolve({
        size: data.length,
        chunkBytes,
        read: (offset, length) => {
          reads.n++;
          return Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length)));
        },
      }),
  };
  return { source, reads };
}

/** `{"a":{"b":{"f0":0,...,"f199":199,"c":1,"d":2}}}` - c and d are the last two
 *  members of a large object, so a cold scan of `b` faults many chunks. */
function deepObjectDoc(): string {
  const fields = Array.from({ length: 200 }, (_, i) => `"f${i}":${i}`).join(',');
  return `{"a":{"b":{${fields},"c":1,"d":2}}}`;
}

/** A long flat array used by the backward / scattered effect tests: one deep get
 *  plants chunk-cadence array members across it, which earlier indices reuse. */
function flatArrayDoc(n: number): Uint8Array {
  return enc('{"arr":[' + Array.from({ length: n }, () => '"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"').join(',') + ']}');
}

test('reads_are_chunk_aligned_and_coalesce_into_multi_chunk_bursts', async () => {
  const data = enc('[' + Array.from({ length: 200 }, () => '1').join(',') + ']');
  const reads: Array<{ offset: number; length: number }> = [];
  const source: SeekableSource = {
    open: () =>
      Promise.resolve({
        size: data.length,
        chunkBytes: 64,
        read: (offset, length) => {
          reads.push({ offset, length });
          return Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length)));
        },
      }),
  };
  const cursor = await open(source);
  assert.equal(await cursor.get(100), 1);
  for (const r of reads) {
    assert.equal(r.offset % 64, 0, `unaligned offset ${r.offset}`);
    assert.ok(r.length > 0, `non-positive length ${r.length}`);
    const end = r.offset + r.length;
    assert.ok(end % 64 === 0 || end >= data.length, `read ${r.offset}+${r.length} neither whole-chunk nor at EOF`);
  }
  assert.ok(
    reads.some((r) => r.length > 64),
    `expected at least one coalesced multi-chunk read, got ${JSON.stringify(reads)}`,
  );
});

test('reads_large_doc_resolves_under_heavy_forward_faulting_with_small_chunks', async () => {
  const cursor = await open(memorySource(enc(bigObject(2000)), 256));
  assert.equal(await cursor.get('k1500'), 1500);
  assert.equal(await cursor.get('k0042'), 42);
  assert.equal(await cursor.has('k9999'), false);
});

test('transparency_repeated_overlapping_get_returns_identical_values', async (t) => {
  const doc = '{"data":{"meta":{"v":2},"users":[{"id":1,"name":"a"},{"id":2,"name":"b"}]}}';
  const cursor = await open(memorySource(enc(doc)));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('data', 'meta', 'v'), 2);
  assert.equal(await cursor.get('data', 'users', 1, 'name'), 'b');
  assert.equal(await cursor.get('data', 'users', 0, 'id'), 1);
  assert.equal(await cursor.get('data', 'meta', 'v'), 2);
  assert.equal(await cursor.get('data', 'users', 1, 'name'), 'b');
  assert.equal(await cursor.get('data', 'users', 0, 'name'), 'a');
  assert.equal(await cursor.get('data', 'users', 0, 'missing'), undefined);
});

test('transparency_object_sibling_access_consistent_first_and_last_members', async (t) => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2,"c":3}')));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('c'), 3);
  assert.equal(await cursor.get('a'), 1);
  assert.equal(await cursor.get('b'), 2);
  assert.equal(await cursor.get('c'), 3);
  assert.equal(await cursor.get('missing'), undefined);
});

test('transparency_iter_object_then_out_of_order_multi_get_on_child_is_consistent', async (t) => {
  const cursor = await open(memorySource(enc('{"rows":{"r0":{"a":1,"b":2,"c":3},"r1":{"a":4,"b":5,"c":6}}}')));
  t.after(() => cursor.close());
  const keys: string[] = [];
  for await (const [key] of cursor.iter('rows', { withKey: true, select: 'a' })) {
    keys.push(key as string);
  }
  const seen: Array<[unknown, unknown, unknown]> = [];
  for (const key of keys) {
    const row = await cursor.hop('rows', key);
    assert.ok(row);
    const a = await row.get('a');
    const c = await row.get('c');
    const b = await row.get('b');
    seen.push([a, b, c]);
  }
  assert.deepEqual(seen, [
    [1, 2, 3],
    [4, 5, 6],
  ]);
});

test('transparency_array_repeated_and_overlapping_index_access_is_consistent', async (t) => {
  const data = enc('{"arr":[' + Array.from({ length: 50 }, (_, i) => `${i * 2}`).join(',') + ']}');
  const cursor = await open(memorySource(data, 64));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('arr', 5), 10);
  assert.equal(await cursor.get('arr', 40), 80);
  assert.equal(await cursor.get('arr', 5), 10);
  assert.equal(await cursor.get('arr', 49), 98);
  assert.equal(await cursor.get('arr', 50), undefined);
});

test('transparency_scattered_and_backward_index_access_identical_with_cache_on_and_off', async (t) => {
  const n = 300;
  const data = enc('{"arr":[' + Array.from({ length: n }, (_, i) => `${i * 3}`).join(',') + ']}');
  const idxs = [299, 0, 150, 75, 200, 10, 299, 150, 1];
  const on = await open(memorySource(data, 64));
  const off = await open(memorySource(data, 64), { objectMemberCap: 0, arrayIndexInterval: 0 });
  t.after(() => on.close());
  t.after(() => off.close());
  for (const i of idxs) {
    assert.equal(await on.get('arr', i), i * 3);
    assert.equal(await off.get('arr', i), i * 3);
  }
  assert.equal(await on.get('arr', n), undefined);
});

test('transparency_small_index_cache_budget_stays_identical_under_constant_eviction', async (t) => {
  const n = 200;
  const rows = Array.from({ length: n }, (_, i) => `{"id":${i},"name":"r${i}","v":${i * 2}}`).join(',');
  const data = enc(`{"rows":[${rows}]}`);
  const tiny = await open(memorySource(data, 64), { indexCacheEntries: 4 });
  const dflt = await open(memorySource(data, 64));
  const off = await open(memorySource(data, 64), { indexCacheEntries: 0 });
  t.after(() => tiny.close());
  t.after(() => dflt.close());
  t.after(() => off.close());
  const idxs = [199, 0, 100, 50, 150, 5, 199, 100, 1];
  for (const i of idxs) {
    assert.equal(await tiny.get('rows', i, 'name'), `r${i}`);
    assert.equal(await dflt.get('rows', i, 'name'), `r${i}`);
    assert.equal(await off.get('rows', i, 'name'), `r${i}`);
    assert.equal(await tiny.get('rows', i, 'v'), i * 2);
    assert.equal(await tiny.get('rows', i, 'missing'), undefined);
  }
  assert.equal(await tiny.count('rows'), n);
  assert.equal(await dflt.count('rows'), n);
  assert.equal(await off.count('rows'), n);
});

test('transparency_capped_object_members_resolve_past_the_cap_boundary', async (t) => {
  const cursor = await open(memorySource(enc(bigObject(500)), 256), { objectMemberCap: 4 });
  t.after(() => cursor.close());
  assert.equal(await cursor.get('k0001'), 1);
  assert.equal(await cursor.get('k0400'), 400);
  assert.equal(await cursor.get('k0001'), 1);
  assert.equal(await cursor.has('k0499'), true); // last member, terminated by `}`
  assert.equal(await cursor.has('k9999'), false);
});

test('transparency_disabled_cache_is_correct', async (t) => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2,"c":3}')), { objectMemberCap: 0, arrayIndexInterval: 0 });
  t.after(() => cursor.close());
  assert.equal(await cursor.get('c'), 3);
  assert.equal(await cursor.get('a'), 1);
  assert.equal(await cursor.get('missing'), undefined);
  assert.equal(await cursor.count(), 3);
});

test('effect_warm_sibling_get_faults_fewer_reads_than_cold', async (t) => {
  const data = enc(deepObjectDoc());
  const warm = countingSource(data, 256);
  const wc = await open(warm.source);
  t.after(() => wc.close());
  assert.equal(await wc.get('a', 'b', 'c'), 1);
  warm.reads.n = 0;
  assert.equal(await wc.get('a', 'b', 'd'), 2);
  const warmReads = warm.reads.n;

  const cold = countingSource(data, 256);
  const cc = await open(cold.source);
  t.after(() => cc.close());
  assert.equal(await cc.get('a', 'b', 'd'), 2);
  const coldReads = cold.reads.n;

  assert.ok(warmReads < coldReads, `warm sibling get (${warmReads} reads) should be < cold (${coldReads})`);
});

test('effect_warm_array_index_get_faults_fewer_reads_than_cold', async (t) => {
  const data = enc('{"arr":[' + Array.from({ length: 100 }, () => '{"v":"xxxxxxxxxxxxxxxxxxxx"}').join(',') + ']}');
  const warm = countingSource(data, 256);
  const wc = await open(warm.source);
  t.after(() => wc.close());
  await wc.get('arr', 40);
  warm.reads.n = 0;
  await wc.get('arr', 60); // resumes from index 40's array member
  const warmReads = warm.reads.n;

  const cold = countingSource(data, 256);
  const cc = await open(cold.source);
  t.after(() => cc.close());
  await cc.get('arr', 60);
  const coldReads = cold.reads.n;

  assert.ok(warmReads < coldReads, `warm index get (${warmReads} reads) should be < cold (${coldReads})`);
});

test('effect_warm_backward_array_get_faults_fewer_reads_than_cold', async (t) => {
  const data = flatArrayDoc(400);
  const warm = countingSource(data, 256);
  const wc = await open(warm.source);
  t.after(() => wc.close());
  await wc.get('arr', 380);
  warm.reads.n = 0;
  await wc.get('arr', 40);
  const warmReads = warm.reads.n;

  const cold = countingSource(data, 256);
  const cc = await open(cold.source);
  t.after(() => cc.close());
  await cc.get('arr', 40);
  const coldReads = cold.reads.n;

  assert.ok(warmReads < coldReads, `warm backward get (${warmReads} reads) should be < cold (${coldReads})`);
});

test('effect_warm_scattered_revisit_faults_fewer_reads_than_cold', async (t) => {
  const data = flatArrayDoc(400);
  const idxs = [350, 50, 220, 120, 300, 80];
  const warm = countingSource(data, 256);
  const wc = await open(warm.source);
  t.after(() => wc.close());
  for (const i of idxs) {
    await wc.get('arr', i);
  }
  warm.reads.n = 0;
  for (const i of idxs) {
    await wc.get('arr', i);
  }
  const warmReads = warm.reads.n;

  const cold = countingSource(data, 256);
  const cc = await open(cold.source);
  t.after(() => cc.close());
  for (const i of idxs) {
    await cc.get('arr', i);
  }
  const coldReads = cold.reads.n;

  assert.ok(warmReads < coldReads, `warm scattered revisit (${warmReads} reads) should be < cold (${coldReads})`);
});

test('effect_repeat_count_issues_no_reads', async (t) => {
  const data = enc('{"xs":[' + Array.from({ length: 300 }, (_, i) => `${i}`).join(',') + ']}');
  const { source, reads } = countingSource(data, 256);
  const cursor = await open(source);
  t.after(() => cursor.close());
  assert.equal(await cursor.count('xs'), 300);
  assert.ok(reads.n > 0, 'the cold count must read');
  reads.n = 0;
  assert.equal(await cursor.count('xs'), 300);
  assert.equal(reads.n, 0, 'a repeat count must be served from the cache with no reads');
});

test('knobs_open_rejects_invalid_cache_knobs', async () => {
  await assert.rejects(() => open(memorySource(enc('{}')), { objectMemberCap: -1 }), RangeError);
  await assert.rejects(() => open(memorySource(enc('{}')), { objectMemberCap: 1.5 }), RangeError);
  await assert.rejects(() => open(memorySource(enc('{}')), { arrayIndexInterval: -1 }), RangeError);
  await assert.rejects(() => open(memorySource(enc('{}')), { arrayIndexInterval: Number.NaN }), RangeError);
  await assert.rejects(() => open(memorySource(enc('{}')), { indexCacheEntries: 1e21 }), RangeError);
});
