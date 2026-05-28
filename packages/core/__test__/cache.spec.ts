import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, type Source } from '../src/index.ts'
import { memorySource, enc, bigObject } from './fixtures.ts'

// Chunk cache behavior: bounded resident occupancy, chunk-aligned fetches, shared
// stats across derived cursors, and correctness under a tight slot cap.

test('cache_stats_reports_bounded_occupancy', async (t) => {
  const cursor = await open(memorySource(enc(bigObject(2000)), 256), { maxResidentChunks: 16 })
  t.after(() => cursor.close())

  assert.equal(await cursor.get('/k1500'), 1500)

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
  for await (const user of cursor.walk('/users')) {
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
  assert.equal(await cursor.get('/100'), 1)
  for (const r of reads) {
    assert.equal(r.offset % 64, 0, `unaligned offset ${r.offset}`)
    assert.equal(r.length, 64, `unexpected length ${r.length}`)
  }
})

test('cache_large_doc_under_tight_slot_cap', async () => {
  // 30 KB object with 2000 keys; cap = 16 slots, chunk = 256 bytes.
  // The query must succeed under heavy fetching and eviction.
  const cursor = await open(memorySource(enc(bigObject(2000)), 256), { maxResidentChunks: 16 })
  assert.equal(await cursor.get('/k1500'), 1500)
  assert.equal(await cursor.get('/k0042'), 42)
  assert.equal(await cursor.has('/k9999'), false)
})
