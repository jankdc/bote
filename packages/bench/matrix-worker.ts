// Matrix-cell worker. Reads one Cell JSON from stdin, runs its
// measurement, writes one Result JSON line to stdout, exits 0. On
// failure writes `{cell, error}` and exits 1.
// The driver spawns us fresh so the native chunk cache,
// bitmap store, and V8 heap all start cold.

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { DEFAULT_ITER_BATCH } from '@botejs/core'
import { open, type Cursor } from '@botejs/native'

import type { Cell, Result, Timing } from './cells.ts'
import { buildFixture, fileSource, type DocFixture, type Path, type Source } from './fixtures.ts'

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
// `walk-get-name` is a walk that also fetches `name` on every child -
// closer to a realistic streaming-traversal workload.
//
// The native binding takes paths as a single `Array<string|number>` arg, not
// variadic; the variadic surface lives one layer up in `@botejs/core`. The
// bench measures the native layer directly, so we pass the array as-is.
async function invokeOnce(cursor: Cursor, cell: Cell, path: Path): Promise<number> {
  const p = path as (string | number)[]
  switch (cell.op) {
    case 'get':
      await cursor.get(p)
      return 1
    case 'has':
      await cursor.has(p)
      return 1
    case 'walk': {
      let n = 0
      if (cell.accessPattern === 'walk-get-name') {
        for await (const [, child] of cursor.walk(p)) {
          await child.get(['name'])
          n += 1
        }
      } else if (cell.accessPattern === 'walk-first') {
        // Stop after the first child: this times time-to-first-child, not a
        // full traversal.
        for await (const _entry of cursor.walk(p)) {
          n = 1
          break
        }
      } else {
        for await (const _entry of cursor.walk(p)) n += 1
      }
      return n
    }
    case 'iter': {
      // `.iter` always yields batches; count items, not yields.
      let n = 0
      for await (const batch of cursor.iter(p, { batch: DEFAULT_ITER_BATCH })) n += batch.length
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
  const path = fixture.paths[cell.accessPattern]
  if (path === null) {
    throw new Error(`cell ${cell.id}: shape ${cell.docShape} does not support access pattern ${cell.accessPattern}`)
  }
  const { source, cleanup } = await makeSource(cell, fixture.buf)
  try {
    const cursor = open(source)

    // Warm the JIT and fault the cell's chunks before timing.
    const warmupDeadline = process.hrtime.bigint() + 50_000_000n // 50 ms
    do {
      await invokeOnce(cursor, cell, path)
    } while (process.hrtime.bigint() < warmupDeadline)

    const batchMeans: number[] = []
    for (let s = 0; s < cell.samples; s++) {
      const t0 = process.hrtime.bigint()
      for (let i = 0; i < cell.iterations; i++) await invokeOnce(cursor, cell, path)
      const t1 = process.hrtime.bigint()
      batchMeans.push(Number(t1 - t0) / cell.iterations)
    }

    return { cell, timing: summarizeTiming(batchMeans, cell) }
  } finally {
    await cleanup()
  }
}

function summarizeTiming(batchMeans: number[], cell: Cell): Timing {
  const sorted = [...batchMeans].sort((a, b) => a - b)
  const mean = batchMeans.reduce((a, b) => a + b, 0) / batchMeans.length
  const variance = batchMeans.reduce((a, b) => a + (b - mean) ** 2, 0) / batchMeans.length
  const min_ns = sorted[0]
  const firstItem = cell.op === 'walk' && cell.accessPattern === 'walk-first'
  return {
    min_ns,
    p50_ns: percentile(sorted, 0.5),
    mean_ns: mean,
    cv: mean > 0 ? Math.sqrt(variance) / mean : 0,
    iters_per_sample: cell.iterations,
    samples: cell.samples,
    ...(firstItem ? { first_item_ns: min_ns } : {}),
  }
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
