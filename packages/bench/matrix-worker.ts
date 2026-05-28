// Matrix-cell worker. Reads one Cell JSON from stdin, runs its
// measurement, writes one Result JSON line to stdout, exits 0. On
// failure writes `{cell, error}` and exits 1.
// The driver spawns us fresh so the native chunk cache,
// bitmap store, and V8 heap all start cold.

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { open, type Cursor } from '@botejs/native'

import type { Cell, Reference, Result, Timing } from './cells.ts'
import { buildFixture, fileSource, memorySource, type DocFixture, type Source } from './fixtures.ts'

async function readStdin(): Promise<string> {
  const chunks: Buffer[] = []
  for await (const chunk of process.stdin) chunks.push(chunk as Buffer)
  return Buffer.concat(chunks).toString('utf8')
}

interface SourceHandle {
  source: Source
  cleanup: () => Promise<void>
}

async function makeSource(cell: Cell, buf: Uint8Array): Promise<SourceHandle> {
  if (cell.source === 'memory') {
    return { source: memorySource(buf, cell.chunkBytes), cleanup: async () => {} }
  }
  const dir = mkdtempSync(join(tmpdir(), 'bote-matrix-'))
  const path = join(dir, 'doc.json')
  writeFileSync(path, buf)
  const source = await fileSource(path, cell.chunkBytes)
  return {
    source,
    cleanup: async () => {
      await source.close?.()
      rmSync(dir, { recursive: true, force: true })
    },
  }
}

// Returns items consumed (1 for get/has, iterated count for walk/iter).
// `walk-get-name` is a walk that also fetches `/name` on every child -
// closer to a realistic streaming-traversal workload.
async function invokeOnce(cursor: Cursor, cell: Cell, pointer: string): Promise<number> {
  switch (cell.op) {
    case 'get':
      await cursor.get(pointer)
      return 1
    case 'has':
      await cursor.has(pointer)
      return 1
    case 'walk': {
      let n = 0
      if (cell.accessPattern === 'walk-get-name') {
        for await (const child of cursor.walk(pointer)) {
          await child.get('/name')
          n += 1
        }
      } else if (cell.accessPattern === 'walk-first') {
        // Stop after the first child: this times time-to-first-child, not a
        // full traversal.
        for await (const _child of cursor.walk(pointer)) {
          n = 1
          break
        }
      } else {
        for await (const _child of cursor.walk(pointer)) n += 1
      }
      return n
    }
    case 'iter': {
      // `.iter` always yields batches; count items, not yields.
      let n = 0
      for await (const batch of cursor.iter(pointer)) n += batch.length
      return n
    }
  }
}

function percentile(sorted: number[], q: number): number {
  if (sorted.length === 0) return 0
  if (sorted.length === 1) return sorted[0]
  const idx = (sorted.length - 1) * q
  const lo = Math.floor(idx)
  const hi = Math.ceil(idx)
  if (lo === hi) return sorted[lo]
  return sorted[lo] * (1 - (idx - lo)) + sorted[hi] * (idx - lo)
}

async function measureCell(cell: Cell): Promise<Result> {
  const fixture: DocFixture = buildFixture(cell.docShape, cell.docSize, cell.padWidth)
  const pointer = fixture.pointers[cell.accessPattern]
  if (pointer === null) {
    throw new Error(`cell ${cell.id}: shape ${cell.docShape} does not support access pattern ${cell.accessPattern}`)
  }
  const { source, cleanup } = await makeSource(cell, fixture.buf)
  try {
    const cursor = open(source, { maxResidentChunks: cell.maxResidentChunks })

    // Let's warm this up it.
    let itemsPerInvocation = 0
    const warmupDeadline = process.hrtime.bigint() + 50_000_000n // 50 ms
    do {
      itemsPerInvocation = await invokeOnce(cursor, cell, pointer)
    } while (process.hrtime.bigint() < warmupDeadline)

    const batchMeans: number[] = []
    for (let s = 0; s < cell.samples; s++) {
      const t0 = process.hrtime.bigint()
      for (let i = 0; i < cell.iterations; i++) await invokeOnce(cursor, cell, pointer)
      const t1 = process.hrtime.bigint()
      batchMeans.push(Number(t1 - t0) / cell.iterations)
    }

    const timing = summarizeTiming(batchMeans, cell, itemsPerInvocation)
    const reference = measureParseReference(fixture.buf, cell, pointer, timing.min_ns)
    return { cell, timing, reference }
  } finally {
    await cleanup()
  }
}

function summarizeTiming(batchMeans: number[], cell: Cell, itemsPerInvocation: number): Timing {
  const sorted = [...batchMeans].sort((a, b) => a - b)
  const mean = batchMeans.reduce((a, b) => a + b, 0) / batchMeans.length
  const variance = batchMeans.reduce((a, b) => a + (b - mean) ** 2, 0) / batchMeans.length
  const min_ns = sorted[0]
  const firstItem = cell.op === 'walk' && cell.accessPattern === 'walk-first'
  const streaming = (cell.op === 'walk' || cell.op === 'iter') && !firstItem
  return {
    min_ns,
    p50_ns: percentile(sorted, 0.5),
    mean_ns: mean,
    cv: mean > 0 ? Math.sqrt(variance) / mean : 0,
    iters_per_sample: cell.iterations,
    samples: cell.samples,
    ...(firstItem ? { first_item_ns: min_ns } : {}),
    ...(streaming
      ? { items_per_invocation: itemsPerInvocation, ns_per_item: min_ns / Math.max(1, itemsPerInvocation) }
      : {}),
  }
}

// Walk a parsed JS value by JSON pointer (RFC 6901). The empty pointer ``
// resolves to the root, which `walk`/`iter` cells on a wide-flat doc rely
// on. Used inside the JSON.parse reference so the comparison runs the same
// logical lookup as the bote op.
function evalPointer(obj: unknown, pointer: string): unknown {
  const parts = pointer.split('/').slice(1)
  let cur: unknown = obj
  for (const part of parts) {
    const key = part.replace(/~1/g, '/').replace(/~0/g, '~')
    if (Array.isArray(cur)) cur = cur[Number.parseInt(key, 10)]
    else if (cur && typeof cur === 'object') cur = (cur as Record<string, unknown>)[key]
    else return undefined
  }
  return cur
}

// The op-equivalent work to do against a parsed JS value: the same logical
// lookup or traversal the bote op performs. Returns a count derived from
// the actual work so V8 can't elide it.
function referenceWork(parsed: unknown, cell: Cell, pointer: string): number {
  const target = evalPointer(parsed, pointer)
  if (cell.op === 'get' || cell.op === 'has') return target === undefined ? 0 : 1
  // walk / iter: traverse the resolved container's children.
  if (target === null || typeof target !== 'object') return 0
  const values = Array.isArray(target) ? target : Object.values(target as Record<string, unknown>)
  // walk-first only needs the first child; JSON.parse still has to parse the
  // whole doc to reach it, which is exactly the asymmetry the cell exposes.
  if (cell.accessPattern === 'walk-first') return values.length > 0 ? 1 : 0
  if (cell.accessPattern === 'walk-get-name') {
    let named = 0
    for (const el of values) {
      if (el !== null && typeof el === 'object' && (el as Record<string, unknown>).name !== undefined) named += 1
    }
    return named
  }
  return values.length
}

// Co-located JSON.parse baseline, computed for every op. The regression
// layer compares the bote/parse ratio.
//
// Reference iter count is adaptive: a 5 MB JSON.parse takes ~15 ms, so
// running 1000x would burn a minute per cell to no purpose. Warm up once,
// time it, pick the smallest iter count that puts each reference batch in
// the ~200 ms window, capped at `cell.iters`. Report the min (matching
// bote's min_ns) so the ratio compares like for like.
function measureParseReference(buf: Uint8Array, cell: Cell, pointer: string, boteMinNs: number): Reference {
  const decoder = new TextDecoder()
  let sink = 0
  const parseOnce = (): void => {
    sink += referenceWork(JSON.parse(decoder.decode(buf)), cell, pointer)
  }
  const warmupT0 = process.hrtime.bigint()
  parseOnce()
  const warmupNs = Number(process.hrtime.bigint() - warmupT0)
  const targetBatchNs = 200_000_000
  const refIters = Math.max(1, Math.min(cell.iterations, Math.floor(targetBatchNs / Math.max(warmupNs, 1))))
  const batchMeans: number[] = []
  for (let s = 0; s < cell.samples; s++) {
    const t0 = process.hrtime.bigint()
    for (let i = 0; i < refIters; i++) parseOnce()
    const t1 = process.hrtime.bigint()
    batchMeans.push(Number(t1 - t0) / refIters)
  }
  if (!Number.isFinite(sink)) throw new Error('reference work produced a non-finite count')
  batchMeans.sort((a, b) => a - b)
  const parse_ns = batchMeans[0]
  return { parse_ns, ratio: boteMinNs / parse_ns }
}

const text = await readStdin()
let cell: Cell
try {
  cell = JSON.parse(text) as Cell
} catch (e) {
  process.stdout.write(JSON.stringify({ error: `worker: invalid cell JSON: ${(e as Error).message}` }) + '\n')
  process.exit(1)
}

try {
  const result = await measureCell(cell)
  process.stdout.write(JSON.stringify(result) + '\n')
  process.exit(0)
} catch (e) {
  const message = e instanceof Error ? e.message : String(e)
  process.stdout.write(JSON.stringify({ cell, error: message }) + '\n')
  process.exit(1)
}
