import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, type Source } from '../src/index.ts'
import { memorySource, enc, bigObject } from './fixtures.ts'

/** A `Source` that counts `read` calls, so the cache's effect on chunk faulting
 *  is observable from the facade (the only public signal - there is no
 *  `cacheStats()`). `reads.n` is the live count; assign 0 to reset it. */
function countingSource(data: Uint8Array, chunkBytes: number): { source: Source; reads: { n: number } } {
  const reads = { n: 0 }
  const source: Source = {
    open: () =>
      Promise.resolve({
        size: data.length,
        chunkBytes,
        read: (offset, length) => {
          reads.n++
          return Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length)))
        },
      }),
  }
  return { source, reads }
}

/** `{"a":{"b":{"f0":0,...,"f199":199,"c":1,"d":2}}}` - c and d are the last two
 *  members of a large object, so a cold scan of `b` faults many chunks. */
function deepObjectDoc(): string {
  const fields = Array.from({ length: 200 }, (_, i) => `"f${i}":${i}`).join(',')
  return `{"a":{"b":{${fields},"c":1,"d":2}}}`
}

// Chunked-read behavior: reads are chunk-aligned and a large document resolves
// correctly while only a bounded transient window of chunks is ever resident
// (the streaming walk stores no chunk or bitmap cache).

test('reads_are_chunk_aligned', async () => {
  const data = enc('[' + Array.from({ length: 200 }, () => '1').join(',') + ']')
  const reads: Array<{ offset: number; length: number }> = []
  const source: Source = {
    open: () =>
      Promise.resolve({
        size: data.length,
        chunkBytes: 64,
        read: (offset, length) => {
          reads.push({ offset, length })
          return Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length)))
        },
      }),
  }
  const cursor = await open(source)
  assert.equal(await cursor.get(100), 1)
  for (const r of reads) {
    assert.equal(r.offset % 64, 0, `unaligned offset ${r.offset}`)
    assert.ok(r.length > 0, `non-positive length ${r.length}`)
    const end = r.offset + r.length
    assert.ok(end % 64 === 0 || end >= data.length, `read ${r.offset}+${r.length} neither whole-chunk nor at EOF`)
  }
  // The burst path must actually coalesce: at least one read spans >1 chunk.
  assert.ok(
    reads.some((r) => r.length > 64),
    `expected at least one coalesced multi-chunk read, got ${JSON.stringify(reads)}`,
  )
})

test('large_doc_resolves_with_small_chunks', async () => {
  // 30 KB object with 2000 keys, 256-byte chunks: the query succeeds under heavy
  // forward faulting with only a bounded window of chunks resident at a time.
  const cursor = await open(memorySource(enc(bigObject(2000)), 256))
  assert.equal(await cursor.get('k1500'), 1500)
  assert.equal(await cursor.get('k0042'), 42)
  assert.equal(await cursor.has('k9999'), false)
})

// Cache TRANSPARENCY: these assert only that values are correct - identical
// whether resolved cold, served warm, or with the cache disabled. They do NOT
// prove a cache hit (a fresh scan passes them too); the cache *effect* is
// asserted separately below via read counts. They still earn their keep: warm
// and disabled paths must never diverge, and the last-member / missing-key cases
// guard the frontier-correctness invariant (the bug this caught was a frontier
// that skipped an untabled member).

test('repeated_overlapping_get_returns_identical_values', async (t) => {
  const doc = '{"data":{"meta":{"v":2},"users":[{"id":1,"name":"a"},{"id":2,"name":"b"}]}}'
  const cursor = await open(memorySource(enc(doc)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('data', 'meta', 'v'), 2)
  assert.equal(await cursor.get('data', 'users', 1, 'name'), 'b')
  assert.equal(await cursor.get('data', 'users', 0, 'id'), 1)
  // Re-query the same and overlapping paths - results must be identical.
  assert.equal(await cursor.get('data', 'meta', 'v'), 2)
  assert.equal(await cursor.get('data', 'users', 1, 'name'), 'b')
  // A sibling of an already-resolved member, then a missing sibling.
  assert.equal(await cursor.get('data', 'users', 0, 'name'), 'a')
  assert.equal(await cursor.get('data', 'users', 0, 'missing'), undefined)
})

test('object_sibling_access_consistent_first_and_last_members', async (t) => {
  // Resolving the last member first must still let earlier members resolve, and
  // a missing key must stay undefined (the frontier never skips a real member).
  const cursor = await open(memorySource(enc('{"a":1,"b":2,"c":3}')))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('c'), 3)
  assert.equal(await cursor.get('a'), 1)
  assert.equal(await cursor.get('b'), 2)
  assert.equal(await cursor.get('c'), 3)
  assert.equal(await cursor.get('missing'), undefined)
})

test('walk_then_multi_get_on_child_is_consistent', async (t) => {
  const cursor = await open(memorySource(enc('{"rows":[{"a":1,"b":2,"c":3},{"a":4,"b":5,"c":6}]}')))
  t.after(() => cursor.close())
  const seen: Array<[unknown, unknown, unknown]> = []
  for await (const row of cursor.walk('rows')) {
    // Several gets on one walked child, out of source order.
    const a = await row.get('a')
    const c = await row.get('c')
    const b = await row.get('b')
    seen.push([a, b, c])
  }
  assert.deepEqual(seen, [
    [1, 2, 3],
    [4, 5, 6],
  ])
})

test('array_repeated_and_overlapping_index_access_is_consistent', async (t) => {
  const data = enc('{"arr":[' + Array.from({ length: 50 }, (_, i) => `${i * 2}`).join(',') + ']}')
  const cursor = await open(memorySource(data, 64))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('arr', 5), 10)
  assert.equal(await cursor.get('arr', 40), 80)
  assert.equal(await cursor.get('arr', 5), 10)
  assert.equal(await cursor.get('arr', 49), 98)
  assert.equal(await cursor.get('arr', 50), undefined)
})

test('scattered_and_backward_index_access_is_consistent', async (t) => {
  // Multi-member resume must stay transparent for backward and scattered
  // access: identical values with the cache on and off, in any index order.
  const n = 300
  const data = enc('{"arr":[' + Array.from({ length: n }, (_, i) => `${i * 3}`).join(',') + ']}')
  const idxs = [299, 0, 150, 75, 200, 10, 299, 150, 1]
  const on = await open(memorySource(data, 64))
  const off = await open(memorySource(data, 64), { objectMemberCap: 0, arrayIndexInterval: 0 })
  t.after(() => on.close())
  t.after(() => off.close())
  for (const i of idxs) {
    assert.equal(await on.get('arr', i), i * 3)
    assert.equal(await off.get('arr', i), i * 3)
  }
  assert.equal(await on.get('arr', n), undefined)
})

test('small_index_cache_budget_is_transparent_under_eviction', async (t) => {
  // A tight slot budget forces near-constant eviction; values must stay identical
  // to the default (effectively unbounded here) and the disabled cache. The
  // scattered, repeated workload re-derives both object members and array indices,
  // so any node dropped by eviction is exercised again on re-query - eviction is a
  // performance detail, never a correctness one.
  const n = 200
  const rows = Array.from({ length: n }, (_, i) => `{"id":${i},"name":"r${i}","v":${i * 2}}`).join(',')
  const data = enc(`{"rows":[${rows}]}`)
  const tiny = await open(memorySource(data, 64), { indexCacheEntries: 4 })
  const dflt = await open(memorySource(data, 64))
  const off = await open(memorySource(data, 64), { indexCacheEntries: 0 })
  t.after(() => tiny.close())
  t.after(() => dflt.close())
  t.after(() => off.close())
  const idxs = [199, 0, 100, 50, 150, 5, 199, 100, 1]
  for (const i of idxs) {
    assert.equal(await tiny.get('rows', i, 'name'), `r${i}`)
    assert.equal(await dflt.get('rows', i, 'name'), `r${i}`)
    assert.equal(await off.get('rows', i, 'name'), `r${i}`)
    assert.equal(await tiny.get('rows', i, 'v'), i * 2)
    assert.equal(await tiny.get('rows', i, 'missing'), undefined)
  }
  assert.equal(await tiny.count('rows'), n)
  assert.equal(await dflt.count('rows'), n)
  assert.equal(await off.count('rows'), n)
})

// Cache EFFECT: a warm query must fault strictly fewer chunks than an identical
// cold one. This is the only forgery-proof signal that the cache did something -
// a fresh scan cannot out-read itself.

test('warm_sibling_get_faults_fewer_reads_than_cold', async (t) => {
  const data = enc(deepObjectDoc())
  // Warm: resolve c (populates the chain + b's member table), reset the counter,
  // then resolve sibling d - which resumes from c's frontier.
  const warm = countingSource(data, 256)
  const wc = await open(warm.source)
  t.after(() => wc.close())
  assert.equal(await wc.get('a', 'b', 'c'), 1)
  warm.reads.n = 0
  assert.equal(await wc.get('a', 'b', 'd'), 2)
  const warmReads = warm.reads.n

  // Cold: resolve d on a fresh cursor - scans b from its open.
  const cold = countingSource(data, 256)
  const cc = await open(cold.source)
  t.after(() => cc.close())
  assert.equal(await cc.get('a', 'b', 'd'), 2)
  const coldReads = cold.reads.n

  assert.ok(warmReads < coldReads, `warm sibling get (${warmReads} reads) should be < cold (${coldReads})`)
})

test('warm_array_index_get_faults_fewer_reads_than_cold', async (t) => {
  const data = enc('{"arr":[' + Array.from({ length: 100 }, () => '{"v":"xxxxxxxxxxxxxxxxxxxx"}').join(',') + ']}')
  const warm = countingSource(data, 256)
  const wc = await open(warm.source)
  t.after(() => wc.close())
  await wc.get('arr', 40)
  warm.reads.n = 0
  await wc.get('arr', 60) // resumes from index 40's array member
  const warmReads = warm.reads.n

  const cold = countingSource(data, 256)
  const cc = await open(cold.source)
  t.after(() => cc.close())
  await cc.get('arr', 60)
  const coldReads = cold.reads.n

  assert.ok(warmReads < coldReads, `warm index get (${warmReads} reads) should be < cold (${coldReads})`)
})

// A long flat array used by the backward / scattered effect tests below: one
// deep get plants chunk-cadence array members across it, which earlier indices reuse.
function flatArrayDoc(n: number): Uint8Array {
  return enc('{"arr":[' + Array.from({ length: n }, () => '"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"').join(',') + ']}')
}

test('warm_backward_array_get_faults_fewer_reads_than_cold', async (t) => {
  // One deep get plants array members along the way; a backward re-get then resumes
  // from a nearby array member instead of rescanning from the open. Impossible under
  // the old single forward-only array member, which parked at the deep index.
  const data = flatArrayDoc(400)
  const warm = countingSource(data, 256)
  const wc = await open(warm.source)
  t.after(() => wc.close())
  await wc.get('arr', 380)
  warm.reads.n = 0
  await wc.get('arr', 40)
  const warmReads = warm.reads.n

  const cold = countingSource(data, 256)
  const cc = await open(cold.source)
  t.after(() => cc.close())
  await cc.get('arr', 40)
  const coldReads = cold.reads.n

  assert.ok(warmReads < coldReads, `warm backward get (${warmReads} reads) should be < cold (${coldReads})`)
})

test('warm_scattered_revisit_faults_fewer_reads_than_cold', async (t) => {
  // Visiting a scattered index set plants an array member at each; revisiting then
  // resumes from each array member rather than rescanning from the open.
  const data = flatArrayDoc(400)
  const idxs = [350, 50, 220, 120, 300, 80]
  const warm = countingSource(data, 256)
  const wc = await open(warm.source)
  t.after(() => wc.close())
  for (const i of idxs) await wc.get('arr', i)
  warm.reads.n = 0
  for (const i of idxs) await wc.get('arr', i)
  const warmReads = warm.reads.n

  const cold = countingSource(data, 256)
  const cc = await open(cold.source)
  t.after(() => cc.close())
  for (const i of idxs) await cc.get('arr', i)
  const coldReads = cold.reads.n

  assert.ok(warmReads < coldReads, `warm scattered revisit (${warmReads} reads) should be < cold (${coldReads})`)
})

test('repeat_count_issues_no_reads', async (t) => {
  const data = enc('{"xs":[' + Array.from({ length: 300 }, (_, i) => `${i}`).join(',') + ']}')
  const { source, reads } = countingSource(data, 256)
  const cursor = await open(source)
  t.after(() => cursor.close())
  assert.equal(await cursor.count('xs'), 300)
  assert.ok(reads.n > 0, 'the cold count must read')
  reads.n = 0
  assert.equal(await cursor.count('xs'), 300)
  assert.equal(reads.n, 0, 'a repeat count must be served from the cache with no reads')
})

test('cache_disabled_is_correct', async (t) => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2,"c":3}')), { objectMemberCap: 0, arrayIndexInterval: 0 })
  t.after(() => cursor.close())
  assert.equal(await cursor.get('c'), 3)
  assert.equal(await cursor.get('a'), 1)
  assert.equal(await cursor.get('missing'), undefined)
  assert.equal(await cursor.count(), 3)
})

test('cache_capped_object_members_stays_correct', async (t) => {
  // A tiny object cap tables only a dense prefix; members past the cap resume-scan
  // from the cap boundary, so every lookup must still resolve correctly.
  const cursor = await open(memorySource(enc(bigObject(500)), 256), { objectMemberCap: 4 })
  t.after(() => cursor.close())
  assert.equal(await cursor.get('k0001'), 1) // within the tabled prefix
  assert.equal(await cursor.get('k0400'), 400) // beyond the cap: resumes from the boundary
  assert.equal(await cursor.get('k0001'), 1)
  assert.equal(await cursor.has('k0499'), true) // last member, terminated by `}`
  assert.equal(await cursor.has('k9999'), false)
})

test('open_rejects_invalid_cache_knobs', async () => {
  await assert.rejects(() => open(memorySource(enc('{}')), { objectMemberCap: -1 }), RangeError)
  await assert.rejects(() => open(memorySource(enc('{}')), { objectMemberCap: 1.5 }), RangeError)
  await assert.rejects(() => open(memorySource(enc('{}')), { arrayIndexInterval: -1 }), RangeError)
  await assert.rejects(() => open(memorySource(enc('{}')), { arrayIndexInterval: Number.NaN }), RangeError)
})
