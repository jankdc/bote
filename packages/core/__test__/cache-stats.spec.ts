import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, fromBuffer, type Source } from '../src/index.ts'

function memorySource(data: Uint8Array, chunkBytes?: number): Source {
  return fromBuffer(data, chunkBytes === undefined ? undefined : { chunkBytes })
}

test('cache_stats_reports_bounded_occupancy', async (t) => {
  const parts = ['{']
  for (let i = 0; i < 2000; i++) {
    if (i > 0) parts.push(',')
    parts.push(`"k${String(i).padStart(4, '0')}":${i}`)
  }
  parts.push('}')
  const data = new TextEncoder().encode(parts.join(''))
  const cursor = await open(memorySource(data, 256), { maxResidentChunks: 16 })
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
  const data = new TextEncoder().encode('{"users":[{"name":"Alice"},{"name":"Bob"}]}')
  const cursor = await open(memorySource(data))
  t.after(() => cursor.close())

  const rootCeiling = cursor.cacheStats().ceilingBytes
  for await (const user of cursor.walk('/users')) {
    assert.equal(user.cacheStats().ceilingBytes, rootCeiling)
  }
})
