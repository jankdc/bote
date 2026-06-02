// Structural-index cache demonstrator: warm vs cold.
//
//   yarn workspace @botejs/bench cache                    # per-scenario table
//   yarn workspace @botejs/bench cache --reads-only       # skip the timing columns
//   yarn workspace @botejs/bench cache --sweep            # indexCacheEntries sweep (~200 MB doc)
//   yarn workspace @botejs/bench cache --sweep --mb 500   # sweep on a custom doc size
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
//
// The --sweep mode varies `indexCacheEntries` over a deep single-locality drill
// to show the budget's threshold-then-plateau effect on cold-vs-warm time: too
// few slots and the cache is effectively off (warm == cold); past a small
// threshold the warm drill collapses to a resume; more slots only raise the
// memory ceiling, they don't speed a single-locality drill further.

import { closeSync, mkdtempSync, openSync, readFileSync, rmSync, writeSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { fromBuffer, open, type Source, type SourceReader } from '@botejs/core'

import { arg, flag } from './cli.ts'
import { buildArrayDoc } from './fixtures.ts'
import { fmtBytes, fmtNs } from './format.ts'

const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

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
  /** Primes the cache: the query a real caller would have made earlier. */
  warm: (c: Cur) => Promise<unknown>
  /** The measured query - resolves to the same place warm or cold. */
  target: (c: Cur) => Promise<unknown>
}

type Cur = Awaited<ReturnType<typeof open>>

const ARRAY_ITEMS = 100_000
const arrayDoc = buildArrayDoc(ARRAY_ITEMS, 6)
// Kept under the default 1024-slot cache budget: a tabled object holds one slot
// per member, so a wider object would evict its own table and lose the resume.
const OBJ_MEMBERS = 900
const objDoc = nestedBigObject(OBJ_MEMBERS)
const CHUNK = 4096

const REC_COUNT = 5000
const REC_IDX = 4000 // deep in the array, so the cold drill-down scans most of it
const recDoc = recordsDoc(REC_COUNT)

// A realistic combined access: render one record's detail view, mixing all three
// ops the way an app would - a header `get`, a point `get`, a `walk` over the
// record's fields, and an `iter` over its tags. Cold, the first get pays the
// full scan to REC_IDX; warm, the array landmark and the record's container are
// already cached, so every step resumes near the target.
async function detailView(c: Cur): Promise<number> {
  let sink = 0
  sink += Number(await c.get('meta', 'version'))
  sink += String(await c.get('records', REC_IDX, 'name')).length
  for await (const _field of c.walk('records', REC_IDX)) sink += 1
  for await (const batch of c.iter('records', REC_IDX, 'tags')) sink += batch.length
  return sink
}

// A scattered, out-of-order index set over the big array. Each get plants a
// landmark at its index; a revisit (or any later nearby index) resumes from the
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
  const arr = Array.from({ length: items }, (_, i) => `{"id":${i},"name":"item-${String(i).padStart(6, '0')}"}`).join(',')
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
    name: 'array index resumed from landmark',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: (c) => c.get('items', ARRAY_ITEMS / 2, 'name'),
    target: (c) => c.get('items', ARRAY_ITEMS / 2 + 50, 'name'),
  },
  {
    // One deep get plants chunk-cadence landmarks across the array; a backward
    // re-get resumes from the nearest one. Flat zero before multi-landmarks.
    name: 'array backward index (prefix landmark)',
    doc: arrayDoc,
    chunkBytes: CHUNK,
    warm: (c) => c.get('items', 90_000, 'name'),
    target: (c) => c.get('items', 10_000, 'name'),
  },
  {
    // A scattered index set, revisited: each index resumes from its own landmark.
    name: 'array scattered revisit (multi-landmark)',
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
    warm: detailView,
    target: detailView,
  },
]

async function measureReads(s: Scenario): Promise<{ cold: number; warm: number }> {
  const cold = countingSource(s.doc, s.chunkBytes)
  const cc = await open(cold.source)
  cold.reads.n = 0
  await s.target(cc)
  const coldReads = cold.reads.n
  await cc.close()

  const warm = countingSource(s.doc, s.chunkBytes)
  const wc = await open(warm.source)
  await s.warm(wc)
  warm.reads.n = 0
  await s.target(wc)
  const warmReads = warm.reads.n
  await wc.close()

  return { cold: coldReads, warm: warmReads }
}

function median(xs: number[]): number {
  const s = [...xs].sort((a, b) => a - b)
  const m = Math.floor(s.length / 2)
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2
}

const COLD_ITERS = 120
const WARM_ITERS = 3000

async function measureTime(s: Scenario): Promise<{ cold: number; warm: number }> {
  const source = fromBuffer(s.doc, { chunkBytes: s.chunkBytes })

  const coldSamples: number[] = []
  for (let i = 0; i < COLD_ITERS; i++) {
    const c = await open(source)
    const t0 = process.hrtime.bigint()
    await s.target(c)
    coldSamples.push(Number(process.hrtime.bigint() - t0))
    await c.close()
  }

  const wc = await open(source)
  await s.warm(wc)
  for (let i = 0; i < 100; i++) await s.target(wc) // warm up the hit path
  const warmSamples: number[] = []
  for (let i = 0; i < WARM_ITERS; i++) {
    const t0 = process.hrtime.bigint()
    await s.target(wc)
    warmSamples.push(Number(process.hrtime.bigint() - t0))
  }
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

function pad(s: string, w: number): string {
  return s.length >= w ? s : s + ' '.repeat(w - s.length)
}

function padL(s: string, w: number): string {
  return s.length >= w ? s : ' '.repeat(w - s.length) + s
}

async function runScenarios(): Promise<void> {
  const skipTime = flag('--reads-only')
  console.log(`bote structural-index cache — warm vs cold\n`)
  const header = skipTime
    ? [pad('scenario', 42), padL('cold reads', 11), padL('warm reads', 11), padL('saved', 8)]
    : [
        pad('scenario', 42),
        padL('cold reads', 11),
        padL('warm reads', 11),
        padL('saved', 8),
        padL('cold time', 11),
        padL('warm time', 11),
        padL('speedup', 9),
      ]
  console.log(header.join('  '))
  console.log('-'.repeat(header.join('  ').length))

  for (const s of scenarios) {
    const reads = await measureReads(s)
    const cols = [
      pad(s.name, 42),
      padL(String(reads.cold), 11),
      padL(String(reads.warm), 11),
      padL(pct(reads.cold, reads.warm), 8),
    ]
    if (!skipTime) {
      const time = await measureTime(s)
      cols.push(padL(fmtNs(time.cold), 11), padL(fmtNs(time.warm), 11), padL(speedup(time.cold, time.warm), 9))
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
  const dir = mkdtempSync(join(tmpdir(), 'bote-cache-'))
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
    rmSync(dir, { recursive: true, force: true })
  }
}

// A deep single-locality drill, the access pattern the cache actually
// accelerates: a header `get`, a point `get` into a record near the end of the
// doc, a `walk` over that record's fields, and an `iter` of its tags - all in
// one locality. Cold, the first step scans the whole doc to reach the record;
// warm, the cursor resumes from the frontier the prior pass parked there.
function makeDrillView(count: number): (c: Cur) => Promise<number> {
  const idx = count - 2 // a record near the very end, so the cold scan is long
  return async (c) => {
    let sink = 0
    sink += Number(await c.get('meta', 'version'))
    sink += String(await c.get('records', idx, 'name')).length
    sink += Number(await c.get('records', idx, 'detail', 'score'))
    for await (const _f of c.walk('records', idx)) sink += 1
    for await (const batch of c.iter('records', idx, 'tags')) sink += batch.length
    return sink
  }
}

// A scattered, out-of-order set of indices spread across the whole array - the
// access pattern multi-landmarks unlock. With too small a budget the per-array
// landmark set is coarse (few survive), so the warm revisit stays near cold;
// past a threshold every visited index keeps its own landmark and the revisit
// collapses to a per-index resume. This is the payoff the single forward-only
// landmark could never give (it parked at the furthest index).
function makeScatterView(count: number): (c: Cur) => Promise<number> {
  const k = 24
  const idxs = Array.from({ length: k }, (_, j) => Math.floor(((j + 1) * count) / (k + 1)))
  for (let i = idxs.length - 1; i > 0; i--) {
    const j = (i * 7 + 3) % (i + 1) // deterministic shuffle: access out of source order
    ;[idxs[i], idxs[j]] = [idxs[j], idxs[i]]
  }
  return async (c) => {
    let sink = 0
    sink += Number(await c.get('meta', 'version'))
    for (const i of idxs) sink += String(await c.get('records', i, 'name')).length
    return sink
  }
}

async function sweepBudget(
  buf: Uint8Array,
  workflow: (c: Cur) => Promise<number>,
  entries: number,
  coldIters: number,
  warmIters: number,
): Promise<{ coldReads: number; warmReads: number; coldNs: number; warmNs: number }> {
  const opt = { indexCacheEntries: entries }

  const coldR = countingSource(buf, CHUNK)
  const ccR = await open(coldR.source, opt)
  coldR.reads.n = 0
  await workflow(ccR)
  const coldReads = coldR.reads.n
  await ccR.close()

  const warmR = countingSource(buf, CHUNK)
  const wcR = await open(warmR.source, opt)
  await workflow(wcR)
  warmR.reads.n = 0
  await workflow(wcR)
  const warmReads = warmR.reads.n
  await wcR.close()

  const source = fromBuffer(buf, { chunkBytes: CHUNK })
  const cold: number[] = []
  for (let i = 0; i < coldIters; i++) {
    const c = await open(source, opt)
    const t0 = process.hrtime.bigint()
    await workflow(c)
    cold.push(Number(process.hrtime.bigint() - t0))
    await c.close()
  }
  const wc = await open(source, opt)
  await workflow(wc)
  for (let i = 0; i < 3; i++) await workflow(wc)
  const warm: number[] = []
  for (let i = 0; i < warmIters; i++) {
    const t0 = process.hrtime.bigint()
    await workflow(wc)
    warm.push(Number(process.hrtime.bigint() - t0))
  }
  await wc.close()

  return { coldReads, warmReads, coldNs: median(cold), warmNs: median(warm) }
}

async function runSweep(): Promise<void> {
  const mb = Number(arg('--mb') ?? 200)
  const scatter = flag('--scatter')
  console.log(`building ~${mb} MB records doc…`)
  const { buf, count } = buildRecordsBuffer(mb * 1024 * 1024)
  const workload = scatter
    ? `scattered (24 out-of-order gets spread across the array, revisited warm)`
    : `deep drill (get + walk + iter on one record near the end)`
  console.log(
    `bote structural-index cache — indexCacheEntries sweep\n` +
      `doc ${fmtBytes(buf.length)}, ${count.toLocaleString()} records, ${CHUNK} B chunks\n` +
      `workload: ${workload}\n`,
  )
  const workflow = scatter ? makeScatterView(count) : makeDrillView(count)

  const header = [
    pad('indexCacheEntries', 18),
    padL('cold time', 11),
    padL('warm time', 11),
    padL('speedup', 9),
    padL('cold reads', 11),
    padL('warm reads', 11),
  ]
  console.log(header.join('  '))
  console.log('-'.repeat(header.join('  ').length))

  // Span the threshold: 0 disables the cache; a few entries can't hold the
  // container's frontier; tens are enough; more only raises memory.
  for (const entries of [0, 8, 64, 1024, 16384, 262144]) {
    const r = await sweepBudget(buf, workflow, entries, 4, 40)
    console.log(
      [
        pad(entries === 0 ? '0 (off)' : String(entries), 18),
        padL(fmtNs(r.coldNs), 11),
        padL(fmtNs(r.warmNs), 11),
        padL(speedup(r.coldNs, r.warmNs), 9),
        padL(String(r.coldReads), 11),
        padL(String(r.warmReads), 11),
      ].join('  '),
    )
  }

  console.log(
    scatter
      ? `\nWarm reads slope down with the budget: each visited index keeps its own resume\n` +
          `landmark only while the per-array set can hold it, so more entries means more\n` +
          `scattered targets resume near their index instead of from the array open.\n` +
          `Reads are deterministic; times indicative.`
      : `\nWarm time steps down once the budget can hold the drilled container's frontier,\n` +
          `then plateaus - extra entries don't speed a single-locality drill, they only let\n` +
          `more distinct containers stay warm at once. Reads are deterministic; times indicative.`,
  )
}

if (flag('--sweep')) await runSweep()
else await runScenarios()
