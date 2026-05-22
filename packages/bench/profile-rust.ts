// Heap profile of a full ingest of a large JSON doc.
//
// Requires `@bote/native` built with `--features heap-profile`. Opens
// the source with default chunk size and default `maxResidentChunks`,
// then walks the whole doc to force every chunk through the cache +
// bitmap store. Writes a heap-profile file to CWD (override with
// `--out <path>`). The profile file format is whatever the native
// crate's `heap-profile` feature emits; today that's a dhat-rs JSON
// dump, viewable at https://nnethercote.github.io/dh_view.html - peak
// live bytes is the `t-gmax` field.
//
// Usage:
//   yarn workspace @bote/bench profile:rust                       # synth ~400 MB doc
//   yarn workspace @bote/bench profile:rust --file path/to.json   # real file
//   yarn workspace @bote/bench profile:rust --items 7_000_000     # custom synth size
//   yarn workspace @bote/bench profile:rust --out my-heap.json    # custom dump path

import { stat } from 'node:fs/promises'
import { resolve } from 'node:path'

import { heapProfileStart, heapProfileStop, open, type Cursor } from '@bote/native'

import { fileSource, withTempDoc } from './fixtures.ts'
import { fmtBytes } from './format.ts'

function argValue(flag: string): string | undefined {
  const i = process.argv.indexOf(flag)
  return i >= 0 ? process.argv[i + 1] : undefined
}

const DEFAULT_SYNTH_ITEMS = 7_000_000 // ≈ 385 MB at padWidth 7
const PAD_WIDTH = 7

async function walkAll(cursor: Cursor): Promise<number> {
  let count = 0
  for await (const child of cursor.walk('/items')) {
    await child.get('/name')
    count += 1
  }
  return count
}

async function profile(path: string, docBytes: number, outPath: string): Promise<void> {
  console.log(`Doc: ${path}  (${fmtBytes(docBytes)})`)
  console.log(`Defaults: chunkBytes = 64 KiB, maxResidentChunks = 256`)
  console.log(`Dumping heap profile to: ${outPath}`)

  const source = await fileSource(path) // no chunkBytes → native default (64 KiB)
  try {
    heapProfileStart(outPath)
    try {
      const cursor = open(source) // no options → default maxResidentChunks (256)
      const seen = await walkAll(cursor)
      console.log(`Walked ${seen.toLocaleString()} items.`)
    } finally {
      heapProfileStop()
    }
  } finally {
    await source.close?.()
  }

  console.log(`\nDone. View ${outPath} at https://nnethercote.github.io/dh_view.html`)
  console.log(`Look for "t-gmax" (peak live bytes) in the viewer header - that's the answer.`)
}

const outPath = resolve(argValue('--out') ?? 'heap-profile.json')
const userFile = argValue('--file')

if (userFile) {
  const path = resolve(userFile)
  const { size } = await stat(path)
  await profile(path, size, outPath)
} else {
  const items = Number(argValue('--items') ?? DEFAULT_SYNTH_ITEMS)
  if (!Number.isFinite(items) || items <= 0) throw new Error(`--items must be a positive number, got ${argValue('--items')}`)
  console.log(`Synthesizing array-of-objects doc (${items.toLocaleString()} items, padWidth ${PAD_WIDTH})…`)
  await withTempDoc(items, PAD_WIDTH, async (path, buf) => {
    await profile(path, buf.byteLength, outPath)
  })
}
