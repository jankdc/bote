// Bounded-resident-memory check
//
// bote's promise is that the native memory held for source data stays at or
// below a fixed ceiling (~ maxResidentChunks x chunkBytes), regardless of
// document size. We iterate array-of-objects docs of increasing size under a
// tight cap, sample peak resident (chunk + bitmap) bytes via the native
// `cacheStats()` API, and assert that peak (a) stays under the cache's own
// derived ceiling and (b) stays ~flat as the doc grows.
//
//   yarn workspace @botejs/bench profile:resident
//   yarn workspace @botejs/bench profile:resident --items 250000,1000000,2000000

import { DEFAULT_ITER_BATCH } from '@botejs/core'
import { open, type Cursor } from '@botejs/native'

import { arg } from './cli.ts'
import { fileSource, withTempDoc } from './fixtures.ts'
import { fmtBytes } from './format.ts'

const CHUNK_BYTES = 64 * 1024
const MAX_RESIDENT_CHUNKS = 16
const PAD_WIDTH = 7
const SAMPLE_EVERY = 25_000
// Sizes span ~8x; all far exceed the ~2 MB ceiling at this cap, so eviction
// is in force throughout (bounding is only meaningful above the cap).
const DEFAULT_ITEMS = [250_000, 1_000_000, 2_000_000]
// A flat ceiling means peak should barely move across an 8x size increase.
const GROWTH_TOLERANCE = 1.5

interface Reading {
  items: number
  docBytes: number
  peakResident: number
  peakBitmap: number
  peakTotal: number
  peakChunks: number
  ceiling: number
}

async function iterSampling(cursor: Cursor, items: number): Promise<Reading> {
  let peakResident = 0
  let peakBitmap = 0
  let peakTotal = 0
  let peakChunks = 0
  let ceiling = 0
  const sample = (): void => {
    const s = cursor.cacheStats()
    const total = s.residentBytes + s.bitmapBytes
    if (total > peakTotal) peakTotal = total
    if (s.residentBytes > peakResident) peakResident = s.residentBytes
    if (s.bitmapBytes > peakBitmap) peakBitmap = s.bitmapBytes
    if (s.residentChunks > peakChunks) peakChunks = s.residentChunks
    ceiling = s.ceilingBytes
  }

  let seen = 0
  for await (const batch of cursor.iter(['items'], {
    selectIr: JSON.stringify({ one: ['name'] }),
    batch: DEFAULT_ITER_BATCH,
  })) {
    for (let i = 0; i < batch.length; i++) {
      seen += 1
      if (seen % SAMPLE_EVERY === 0) sample()
    }
  }
  sample()
  if (seen !== items) throw new Error(`iterated ${seen} of ${items} items`)
  return { items, docBytes: 0, peakResident, peakBitmap, peakTotal, peakChunks, ceiling }
}

async function measure(items: number): Promise<Reading> {
  return withTempDoc(items, PAD_WIDTH, async (path, buf) => {
    const source = await fileSource(path, CHUNK_BYTES)
    try {
      const cursor = open(source, { maxResidentChunks: MAX_RESIDENT_CHUNKS })
      const r = await iterSampling(cursor, items)
      r.docBytes = buf.byteLength
      return r
    } finally {
      await source.close?.()
    }
  })
}

const itemsArg = arg('--items')
const sizes = itemsArg ? itemsArg.split(',').map((s) => Number.parseInt(s.trim(), 10)) : DEFAULT_ITEMS
if (sizes.some((n) => !Number.isFinite(n) || n <= 0))
  throw new Error(`--items must be positive integers, got ${itemsArg}`)

console.log(`Bounded-resident check: cap ${MAX_RESIDENT_CHUNKS} chunks x ${fmtBytes(CHUNK_BYTES)}`)

const readings: Reading[] = []
for (const items of sizes) {
  console.log(`  iterating ${items.toLocaleString()} items…`)
  readings.push(await measure(items))
}

const rows = [
  ['doc size', 'items', 'peak resident', 'peak bitmap', 'peak total', 'chunks', 'ceiling'],
  ...readings.map((r) => [
    fmtBytes(r.docBytes),
    r.items.toLocaleString(),
    fmtBytes(r.peakResident),
    fmtBytes(r.peakBitmap),
    fmtBytes(r.peakTotal),
    String(r.peakChunks),
    fmtBytes(r.ceiling),
  ]),
]
const widths = rows[0].map((_, i) => Math.max(...rows.map((row) => row[i].length)))
console.log('')
for (const row of rows) console.log(row.map((c, i) => c.padStart(widths[i])).join('  '))
console.log('')

const verdicts: string[] = []
let failed = false

const overCeiling = readings.filter((r) => r.peakTotal > r.ceiling)
if (overCeiling.length === 0) {
  verdicts.push(`PASS  peak total stayed within the cache ceiling for all ${readings.length} sizes`)
} else {
  failed = true
  for (const r of overCeiling) {
    verdicts.push(
      `FAIL  ${r.items.toLocaleString()} items: peak ${fmtBytes(r.peakTotal)} exceeded ceiling ${fmtBytes(r.ceiling)}`,
    )
  }
}

const smallest = readings[0].peakTotal
const largest = readings[readings.length - 1].peakTotal
const growth = smallest > 0 ? largest / smallest : 1
const sizeGrowth = readings[readings.length - 1].docBytes / Math.max(1, readings[0].docBytes)
if (growth <= GROWTH_TOLERANCE) {
  verdicts.push(
    `PASS  peak total grew ${growth.toFixed(2)}x across a ${sizeGrowth.toFixed(1)}x doc-size increase (<= ${GROWTH_TOLERANCE}x)`,
  )
} else {
  failed = true
  verdicts.push(
    `FAIL  peak total grew ${growth.toFixed(2)}x across a ${sizeGrowth.toFixed(1)}x doc-size increase - resident memory is scaling with doc size`,
  )
}

for (const v of verdicts) console.log(v)
process.exit(failed ? 1 : 0)
