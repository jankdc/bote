import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, type Source } from '../src/index.ts'
import { memorySource, enc, bigObject } from './fixtures.ts'

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
