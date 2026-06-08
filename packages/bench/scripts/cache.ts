// Structural-index cache demonstrator: warm vs cold.
//
//   npm run bench:cache -w @botejs/bench                                 # per-scenario table
//   npm run bench:cache -w @botejs/bench -- --reads-only                 # skip the timing columns
//   npm run bench:cache -w @botejs/bench -- --index-cache-entries 3072   # custom slot budget (default 1024; 0 disables)
//   npm run bench:cache -w @botejs/bench -- --mb 200                     # also run the demo on a freshly-built ~200 MB doc
//
// `--index-cache-entries <n>` sets the structural-index cache slot budget for
// every cursor opened by this run (both modes), so you can see how a larger or
// smaller budget shifts warm reads — a budget too small to hold a scenario's
// containers evicts them and warm climbs back toward cold.
//
// `--mb <n>` builds a fresh records-shaped JSON of about n megabytes and appends a
// combined-access scenario over its deepest record, so the warm/cold read gap can
// be watched widening as the document grows past the small built-in fixtures.
//
// The cache restores cross-query warmth - a query that lands in a container an
// earlier query already walked starts its scan near the target instead of from
// the container's open. Each scenario runs the same target query two ways:
//   cold - on a fresh cursor (scans from scratch)
//   warm - on a cursor a prior query already primed
//
// Headline metric is chunks faulted (reads), measured through a counting
// Source: deterministic and machine-independent - a fresh scan cannot
// out-read itself, so a drop is forgery-proof proof the cache did work. The
// wall-clock columns are indicative color only (hardware-dependent).

import { closeSync, openSync, readFileSync, writeSync } from 'node:fs'
import { join } from 'node:path'

import { fromBuffer, open, type Source, type SourceReader } from '@botejs/core'

import { arg, flag } from '#lib/cli.ts'
import { buildArrayDoc } from '#lib/fixtures.ts'
import { fmtBytes, fmtNs } from '#lib/format.ts'
import { median, sample, timeNs } from '#lib/timings.ts'
import { createTempDir } from '#lib/tmp.ts'

const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

// Optional slot-budget override applied to every cursor this run opens. Accepts
// kebab and camel spellings; validated here so a typo fails fast rather than
// deep inside `open`.
const indexCacheEntriesArg = arg('--index-cache-entries') ?? arg('--indexCacheEntries')
if (
  indexCacheEntriesArg != null &&
  (!Number.isInteger(Number(indexCacheEntriesArg)) || Number(indexCacheEntriesArg) < 0)
) {
  throw new Error(`--index-cache-entries must be a non-negative integer (0 disables), got ${indexCacheEntriesArg}`)
}
const indexCacheEntries = indexCacheEntriesArg == null ? undefined : Number(indexCacheEntriesArg)
/** Base open options for this run (the budget override, or {} for the native default). */
const baseOpts: { indexCacheEntries?: number } = indexCacheEntries === undefined ? {} : { indexCacheEntries }
/** Header suffix naming the active budget, so a run's output is self-describing. */
const budgetNote = (): string =>
  indexCacheEntries === undefined
    ? '  (indexCacheEntries=native default)'
    : `  (indexCacheEntries=${indexCacheEntries})`

/** A `Source` that counts `read` calls so cache-driven fault reduction is
 *  observable from the facade (there is no `cacheStats()`). */
function countingSource(data: Uint8Array, chunkBytes: number): { source: Source; reads: { n: number } } {
  const reads = { n: 0 }
  const reader: SourceReader = {
    size: data.length,
    chunkBytes,
    read: (offset, length) => {
      reads.n++
      return Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length)))
    },
  }
  return { source: { open: () => Promise.resolve(reader) }, reads }
}

// A deep object whose target container `b` holds many members, so a cold scan
// to any one faults a long run of chunks. Warming on one member tables the rest.
function nestedBigObject(members: number): Uint8Array {
  const fields: string[] = []
  for (let i = 0; i < members; i++) fields.push(`"f${i}":${i}`)
  fields.push('"c":1', '"d":2')
  return enc(`{"a":{"b":{${fields.join(',')}}}}`)
}

// An API-response-shaped doc: a small `meta` header plus a long `records` array
// of nested objects. Stands in for the kind of payload a real caller drills into.
function recordsDoc(n: number): Uint8Array {
  const parts: string[] = [`{"meta":{"version":3,"count":${n}},"records":[`]
  for (let i = 0; i < n; i++) {
    if (i) parts.push(',')
    const id = String(i).padStart(6, '0')
    parts.push(`{"id":${i},"name":"rec-${id}","detail":{"email":"u${i}@x.io","score":${i % 100}},"tags":["a","b","c"]}`)
  }
  parts.push(']}')
  return enc(parts.join(''))
}

interface Scenario {
  name: string
  doc: Uint8Array
  chunkBytes: number
  /** Override the cold-timing sample count (a big doc scans too much to time 120×). */
  coldIters?: number
  /** Primes the cache: the query a real caller would have made earlier. */
  warm: (c: Cur) => Promise<unknown>
  /** The measured query - resolves to the same place warm or cold. */
  target: (c: Cur) => Promise<unknown>
}

type Cur = Awaited<ReturnType<typeof open>>

const ARRAY_ITEMS = 100_000
const arrayDoc = buildArrayDoc(ARRAY_ITEMS, 6)
// With the default unbounded object cap the whole object tables on one scan, so
// warming on a later member tables every earlier one and the sibling resume is free.
const OBJ_MEMBERS = 900
const objDoc = nestedBigObject(OBJ_MEMBERS)
const CHUNK = 4096

const REC_COUNT = 5000
const REC_IDX = 4000 // deep in the array, so the cold drill-down scans most of it
const recDoc = recordsDoc(REC_COUNT)

// A realistic combined access: render one record's detail view, mixing all three
// ops the way an app would - a header `get`, a point `get`, a `walk` over the
// record's fields, and an `iter` over its tags. Cold, the first get pays the
// full scan to the record; warm, the array member and the record's container
// are already cached, so every step resumes near the target.
function detailViewAt(idx: number): (c: Cur) => Promise<number> {
  return async (c) => {
    let sink = 0
    sink += Number(await c.get('meta', 'version'))
    sink += String(await c.get('records', idx, 'name')).length
    for await (const _entry of c.walk('records', idx)) sink += 1
    for await (const batch of c.iter('records', idx, 'tags')) sink += batch.length
    return sink
  }
}

// A scattered, out-of-order index set over the big array. Each get plants a
// array member at its index; a revisit (or any later nearby index) resumes from the
// nearest one rather than rescanning from the array open.
const SCATTER_IDXS = [90_000, 5_000, 60_000, 25_000, 80_000, 15_000, 45_000, 70_000]
async function scatterGets(c: Cur): Promise<number> {
  let sink = 0
  for (const i of SCATTER_IDXS) sink += String(await c.get('items', i, 'name')).length
  return sink
}

// A header object whose array member `items` sits AFTER a bulk of large string
// members `p0..pN`, with a small `tail` member last. Iterating `items` cold scans
// the whole prefix of pads to locate it; warming on the LATER `tail` member tables
// every earlier member (including `items`) as a free side effect of the object
// scan, so the warm iter entry hops straight to `items` and skips the prefix scan.
function siblingArrayDoc(pads: number, padBytes: number, items: number): Uint8Array {
  const big = 'x'.repeat(padBytes)
  const padMembers = Array.from({ length: pads }, (_, i) => `"p${i}":"${big}"`).join(',')
  const arr = Array.from({ length: items }, (_, i) => `{"id":${i},"name":"item-${String(i).padStart(6, '0')}"}`).join(
    ',',
  )
  return enc(`{${padMembers},"items":[${arr}],"tail":{"version":3}}`)
}
const siblingDoc = siblingArrayDoc(50, 4000, 200)

async function iterItems(c: Cur): Promise<number> {
  let n = 0
  for await (const batch of c.iter('items')) n += batch.length
  return n
}

const scenarios: Scenario[] = [
  {
    // Finer chunks than the array cases: at 4 KB the burst coalescing swallows
    // the whole object in a handful of reads, leaving nothing for resume to save.
    name: 'object sibling member (resume frontier)',
    doc: objDoc,
    chunkBytes: 256,
    warm: (c) => c.get('a', 'b', 'c'),
    target: (c) => c.get('a', 'b', 'd'),
  },
  {
    // Iterate an array member that sits early in the object, after getting a
    // sibling that appears LATER in the source: the get's object scan tables every
    // member up to it (including the array), so the warm iter entry hops straight
    // to the array instead of rescanning the long prefix to locate it.
    name: 'iter array sibling after later-sibling get',
    doc: siblingDoc,
    chunkBytes: 256,
    warm: (c) => c.get('tail'),
    target: iterItems,
  },
  {
    name: 'array index resumed from array member',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: (c) => c.get('items', ARRAY_ITEMS / 2, 'name'),
    target: (c) => c.get('items', ARRAY_ITEMS / 2 + 50, 'name'),
  },
  {
    // One deep get plants chunk-cadence array members across the array; a backward
    // re-get resumes from the nearest one. Flat zero before multi-members.
    name: 'array backward index (prefix array member)',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: (c) => c.get('items', 90_000, 'name'),
    target: (c) => c.get('items', 10_000, 'name'),
  },
  {
    // A scattered index set, revisited: each index resumes from its own array member.
    name: 'array scattered revisit (multi-member)',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: scatterGets,
    target: scatterGets,
  },
  {
    name: 'repeated identical get (full hit)',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: (c) => c.get('items', ARRAY_ITEMS - 1, 'name'),
    target: (c) => c.get('items', ARRAY_ITEMS - 1, 'name'),
  },
  {
    name: 'repeated count (full hit)',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: (c) => c.count('items'),
    target: (c) => c.count('items'),
  },
  {
    name: 'detail view: get + walk + iter (combined)',
    doc: recDoc,
    chunkBytes: CHUNK,
    warm: detailViewAt(REC_IDX),
    target: detailViewAt(REC_IDX),
  },
]

// `--mb <n>`: build a fresh records-shaped doc of about n megabytes and append a
// combined detail view over its deepest record. Cold, the first get scans the
// whole array to reach it; warm, the array-member grid and the record's container are
// cached, so the same access resumes near the target — a gap that widens with size.
const docMbArg = arg('--mb')
if (docMbArg != null && (!Number.isFinite(Number(docMbArg)) || Number(docMbArg) <= 0)) {
  throw new Error(`--mb must be a positive number of megabytes, got ${docMbArg}`)
}
if (docMbArg != null) {
  const mb = Number(docMbArg)
  console.log(`building ~${mb} MB records doc…`)
  const { buf, count } = buildRecordsBuffer(mb * 1024 * 1024)
  const view = detailViewAt(count - 1)
  scenarios.push({
    name: `big detail view (${fmtBytes(buf.length)}, ${count.toLocaleString()} recs)`,
    doc: buf,
    chunkBytes: CHUNK,
    coldIters: 8,
    warm: view,
    target: view,
  })
}

async function measureReads(s: Scenario): Promise<{ cold: number; warm: number }> {
  const cold = countingSource(s.doc, s.chunkBytes)
  const cc = await open(cold.source, baseOpts)
  cold.reads.n = 0
  await s.target(cc)
  const coldReads = cold.reads.n
  await cc.close()

  const warm = countingSource(s.doc, s.chunkBytes)
  const wc = await open(warm.source, baseOpts)
  await s.warm(wc)
  warm.reads.n = 0
  await s.target(wc)
  const warmReads = warm.reads.n
  await wc.close()

  return { cold: coldReads, warm: warmReads }
}

const COLD_ITERS = 120
const WARM_ITERS = 3000

async function measureTime(s: Scenario): Promise<{ cold: number; warm: number }> {
  const source = fromBuffer(s.doc, { chunkBytes: s.chunkBytes })
  const coldIters = s.coldIters ?? COLD_ITERS

  const coldSamples: number[] = []
  for (let i = 0; i < coldIters; i++) {
    const c = await open(source, baseOpts)
    coldSamples.push(await timeNs(() => s.target(c)))
    await c.close()
  }

  const wc = await open(source, baseOpts)
  await s.warm(wc)
  for (let i = 0; i < 100; i++) await s.target(wc) // warm up the hit path
  const warmSamples = await sample(() => s.target(wc), WARM_ITERS)
  await wc.close()

  return { cold: median(coldSamples), warm: median(warmSamples) }
}

function pct(cold: number, warm: number): string {
  if (cold === 0) return '—'
  return `${(((cold - warm) / cold) * 100).toFixed(1)}%`
}

function speedup(cold: number, warm: number): string {
  if (warm === 0) return '∞'
  return `${(cold / warm).toFixed(1)}×`
}

async function runScenarios(): Promise<void> {
  const skipTime = flag('--reads-only')
  console.log(`bote structural-index cache — warm vs cold${budgetNote()}\n`)
  const header = skipTime
    ? ['scenario'.padEnd(42), 'cold reads'.padStart(11), 'warm reads'.padStart(11), 'saved'.padStart(8)]
    : [
        'scenario'.padEnd(42),
        'cold reads'.padStart(11),
        'warm reads'.padStart(11),
        'saved'.padStart(8),
        'cold time'.padStart(11),
        'warm time'.padStart(11),
        'speedup'.padStart(9),
      ]
  console.log(header.join('  '))
  console.log('-'.repeat(header.join('  ').length))

  for (const s of scenarios) {
    const reads = await measureReads(s)
    const cols = [
      s.name.padEnd(42),
      String(reads.cold).padStart(11),
      String(reads.warm).padStart(11),
      pct(reads.cold, reads.warm).padStart(8),
    ]
    if (!skipTime) {
      const time = await measureTime(s)
      cols.push(fmtNs(time.cold).padStart(11), fmtNs(time.warm).padStart(11), speedup(time.cold, time.warm).padStart(9))
    }
    console.log(cols.join('  '))
  }

  console.log(
    `\nReads are deterministic and machine-independent — the headline signal.` +
      (skipTime ? '' : ` Times are indicative (this box only).`),
  )
}

// Stream a records-shaped doc of ~targetBytes to a temp file, then read it back
// into one buffer. Streaming keeps peak memory near the doc size instead of
// holding a giant intermediate string + array of parts.
function buildRecordsBuffer(targetBytes: number): { buf: Uint8Array; count: number } {
  const { dir, cleanup } = createTempDir('bote-cache-')
  const path = join(dir, 'doc.json')
  const fd = openSync(path, 'w')
  try {
    writeSync(fd, '{"meta":{"version":3},"records":[')
    let i = 0
    let bytes = 0
    const BATCH = 5000
    while (bytes < targetBytes) {
      let s = ''
      for (let k = 0; k < BATCH; k++, i++) {
        const id = String(i).padStart(9, '0')
        s += `${i ? ',' : ''}{"id":${i},"name":"rec-${id}","detail":{"email":"u${i}@x.io","score":${i % 100}},"tags":["a","b","c"]}`
      }
      bytes += Buffer.byteLength(s)
      writeSync(fd, s)
    }
    writeSync(fd, ']}')
    return { buf: readFileSync(path), count: i }
  } finally {
    closeSync(fd)
    cleanup()
  }
}

await runScenarios()
