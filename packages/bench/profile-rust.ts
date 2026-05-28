// Heap profile of a full ingest of a large JSON doc.
//
// Requires `@botejs/native` built with `--features heap-profile`. Opens
// the source with default chunk size and default `maxResidentChunks`,
// then walks the whole doc to force every chunk through the cache +
// bitmap store. Writes a heap-profile file to CWD (override with
// `--out <path>`). The profile file format is whatever the native
// crate's `heap-profile` feature emits; today that's a dhat-rs JSON
// dump, viewable at https://nnethercote.github.io/dh_view.html - peak
// live bytes is the `t-gmax` field.
//
// Usage:
//   yarn workspace @botejs/bench profile:rust                       # synth ~400 MB doc
//   yarn workspace @botejs/bench profile:rust --file path/to.json   # real file
//   yarn workspace @botejs/bench profile:rust --items 7_000_000     # custom synth size
//   yarn workspace @botejs/bench profile:rust --out my-heap.json    # custom dump path

import { stat } from 'node:fs/promises'
import { resolve } from 'node:path'

import { heapProfilePeakBytes, heapProfileStart, heapProfileStop, open, type Cursor } from '@botejs/native'

import { arg } from './cli.ts'
import { fileSource, withTempDoc } from './fixtures.ts'
import { fmtBytes } from './format.ts'

const DEFAULT_SYNTH_ITEMS = 7_000_000 // ≈ 385 MB at padWidth 7
const PAD_WIDTH = 7

async function iterAll(cursor: Cursor): Promise<number> {
  let count = 0
  for await (const batch of cursor.iter('/items', { selectIr: JSON.stringify({ one: '/name' }) })) {
    count += batch.length
  }
  return count
}

async function profile(path: string, docBytes: number, outPath: string): Promise<void> {
  console.log(`Doc: ${path}  (${fmtBytes(docBytes)})`)
  console.log(`Defaults: chunkBytes = 64 KiB, maxResidentChunks = 512`)
  console.log(`Dumping heap profile to: ${outPath}`)

  const source = await fileSource(path) // no chunkBytes → native default (64 KiB)
  let peakBytes = 0
  try {
    heapProfileStart(outPath)
    try {
      const cursor = open(source) // no options → default maxResidentChunks (512)
      const seen = await iterAll(cursor)
      // Read peak before stopping; stop tears the profiler down.
      peakBytes = heapProfilePeakBytes()
      console.log(`Iterated ${seen.toLocaleString()} items.`)
    } finally {
      heapProfileStop()
    }
  } finally {
    await source.close?.()
  }

  console.log(`\nPeak Rust live bytes (t-gmax): ${fmtBytes(peakBytes)}`)
  console.log(`For per-call-stack attribution, view ${outPath} at https://nnethercote.github.io/dh_view.html`)
}

const outPath = resolve(arg('--out') ?? 'heap-profile.json')
const userFile = arg('--file')

if (userFile) {
  const path = resolve(userFile)
  const { size } = await stat(path)
  await profile(path, size, outPath)
} else {
  const items = Number(arg('--items') ?? DEFAULT_SYNTH_ITEMS)
  if (!Number.isFinite(items) || items <= 0) throw new Error(`--items must be a positive number, got ${arg('--items')}`)
  console.log(`Synthesizing array-of-objects doc (${items.toLocaleString()} items, padWidth ${PAD_WIDTH})…`)
  await withTempDoc(items, PAD_WIDTH, async (path, buf) => {
    await profile(path, buf.byteLength, outPath)
  })
}
