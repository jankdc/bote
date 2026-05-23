// Matrix-cell worker. Reads one Cell JSON from stdin, runs its
// measurement, writes one Result JSON line to stdout, exits 0. On
// failure writes `{cell, error}` and exits 1. One process == one cell,
// by design - the driver spawns us fresh so the native chunk cache,
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
      } else {
        for await (const _child of cursor.walk(pointer)) n += 1
      }
      return n
    }
    case 'iter': {
      let n = 0
      for await (const _value of cursor.iter(pointer)) n += 1
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

    // Warmup: pays for bitmap construction + first chunk loads. Captures
    // items-per-invocation for walk/iter (fixed for a given pointer).
    const itemsPerInvocation = await invokeOnce(cursor, cell, pointer)

    const batchMeans: number[] = []
    for (let s = 0; s < cell.samples; s++) {
      const t0 = process.hrtime.bigint()
      for (let i = 0; i < cell.iters; i++) await invokeOnce(cursor, cell, pointer)
      const t1 = process.hrtime.bigint()
      batchMeans.push(Number(t1 - t0) / cell.iters)
    }

    const sorted = [...batchMeans].sort((a, b) => a - b)
    const timing: Timing = {
      p50_ns: percentile(sorted, 0.5),
      p95_ns: percentile(sorted, 0.95),
      mean_ns: batchMeans.reduce((a, b) => a + b, 0) / batchMeans.length,
      iters_per_sample: cell.iters,
      samples: cell.samples,
      batch_means_ns: batchMeans,
      ...(cell.op === 'walk' || cell.op === 'iter' ? { items_per_invocation: itemsPerInvocation } : {}),
    }

    let reference: Reference | undefined
    if (
      cell.op === 'get' &&
      (cell.accessPattern === 'shallow' || cell.accessPattern === 'mid' || cell.accessPattern === 'deep')
    ) {
      reference = measureParseReference(fixture.buf, cell, pointer, timing.p50_ns)
    }

    return { cell, timing, reference }
  } finally {
    await cleanup()
  }
}

// Walk a parsed JS value by JSON pointer (RFC 6901, minus the root `''`
// special-case the callers don't hit). Used inside the JSON.parse
// reference so the comparison runs the same logical lookup as bote.get.
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

// Same-process JSON.parse + property lookup baseline. Lets the
// regression layer compare ratios (which survive CI noise) instead of
// absolute timings.
//
// Reference iter count is adaptive - a 5 MB JSON.parse takes ~15 ms, so
// running 1000× would burn a minute per cell to no purpose. Warm up
// once, time it, pick the smallest iter count that puts each reference
// batch in the 100–200 ms window, capped at `cell.iters`.
function measureParseReference(buf: Uint8Array, cell: Cell, pointer: string, boteNs: number): Reference {
  const decoder = new TextDecoder()
  const parseOnce = (): void => {
    const parsed = JSON.parse(decoder.decode(buf))
    evalPointer(parsed, pointer)
  }
  const warmupT0 = process.hrtime.bigint()
  parseOnce()
  const warmupNs = Number(process.hrtime.bigint() - warmupT0)
  const targetBatchNs = 200_000_000
  const refIters = Math.max(1, Math.min(cell.iters, Math.floor(targetBatchNs / Math.max(warmupNs, 1))))
  const batchMeans: number[] = []
  for (let s = 0; s < cell.samples; s++) {
    const t0 = process.hrtime.bigint()
    for (let i = 0; i < refIters; i++) parseOnce()
    const t1 = process.hrtime.bigint()
    batchMeans.push(Number(t1 - t0) / refIters)
  }
  batchMeans.sort((a, b) => a - b)
  const parse_ns = batchMeans[Math.floor(batchMeans.length / 2)]
  return { parse_ns, ratio: boteNs / parse_ns }
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
