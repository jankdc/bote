import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, type Source } from '../src/index.ts'
import { memorySource, enc, bigObject } from './fixtures.ts'

// Chunk cache behavior: bounded resident occupancy, chunk-aligned fetches, shared
// stats across derived cursors, and correctness under a tight slot cap.

test('cache_stats_reports_bounded_occupancy', async (t) => {
  const cursor = await open(memorySource(enc(bigObject(2000)), 256), { maxResidentBytes: 16 * 256 })
  t.after(() => cursor.close())

  assert.equal(await cursor.get('k1500'), 1500)

  const stats = cursor.cacheStats()
  assert.ok(stats.ceilingBytes > 0, `ceilingBytes ${stats.ceilingBytes} should be positive`)
  assert.ok(stats.residentChunks >= 0 && Number.isFinite(stats.residentChunks))
  assert.ok(
    stats.residentBytes + stats.bitmapBytes <= stats.ceilingBytes,
    `resident ${stats.residentBytes} + bitmap ${stats.bitmapBytes} exceeded ceiling ${stats.ceilingBytes}`,
  )
})

test('cache_stats_is_shared_across_derived_cursors', async (t) => {
  const data = enc('{"users":[{"name":"Alice"},{"name":"Bob"}]}')
  const cursor = await open(memorySource(data))
  t.after(() => cursor.close())

  const rootCeiling = cursor.cacheStats().ceilingBytes
  for await (const user of cursor.walk('users')) {
    assert.equal(user.cacheStats().ceilingBytes, rootCeiling)
  }
})

test('cache_reads_are_chunk_aligned', async () => {
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

test('cache_large_doc_under_tight_slot_cap', async () => {
  // 30 KB object with 2000 keys; cap = 16 slots, chunk = 256 bytes.
  // The query must succeed under heavy fetching and eviction.
  const cursor = await open(memorySource(enc(bigObject(2000)), 256), { maxResidentBytes: 16 * 256 })
  assert.equal(await cursor.get('k1500'), 1500)
  assert.equal(await cursor.get('k0042'), 42)
  assert.equal(await cursor.has('k9999'), false)
})

test('cache_ceiling_is_twice_the_resident_byte_budget', async (t) => {
  // maxResidentBytes is the resident chunk-data budget; the enforced RSS
  // ceiling adds bitmap headroom (a 2x factor).
  const cursor = await open(memorySource(enc(bigObject(2000)), 256), { maxResidentBytes: 16 * 256 })
  t.after(() => cursor.close())
  assert.equal(cursor.cacheStats().ceilingBytes, 16 * 256 * 2)
})

test('cache_rejects_budget_not_a_multiple_of_chunk_bytes', async () => {
  // chunkBytes = 256; 300 is not a whole number of chunks.
  await assert.rejects(() => open(memorySource(enc(bigObject(10)), 256), { maxResidentBytes: 300 }), /multiple/)
})

test('cache_rejects_non_positive_budget', async () => {
  await assert.rejects(() => open(memorySource(enc(bigObject(10)), 256), { maxResidentBytes: 0 }), RangeError)
  await assert.rejects(() => open(memorySource(enc(bigObject(10)), 256), { maxResidentBytes: -256 }), RangeError)
})
