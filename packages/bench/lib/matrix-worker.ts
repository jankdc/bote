// Matrix-cell worker. Reads one Cell JSON from stdin, runs its
// measurement, writes one Result JSON line to stdout, exits 0. On
// failure writes `{cell, error}` and exits 1.
// The driver spawns us fresh so the native chunk cache,
// bitmap store, and V8 heap all start cold.

import { writeFileSync } from 'node:fs'
import { join } from 'node:path'

import { DEFAULT_ITER_BATCH, fromFile, open, type Path, type Cursor, type IterStream } from '@botejs/core'

import type { Cell, Consume, Result, Timing } from './cells.ts'
import { buildFixture, type DocFixture } from './fixtures.ts'
import { cv, mean, percentile, sample, warmup } from './timings.ts'
import { createTempDir } from './tmp.ts'

async function readStdin(): Promise<string> {
  const chunks: Buffer[] = []
  for await (const chunk of process.stdin) chunks.push(chunk as Buffer)
  return Buffer.concat(chunks).toString('utf8')
}

// Writes the cell's doc to a temp file the cursor faults chunk-by-chunk (the
// real-world streaming path). Returns the path plus a `cleanup` that removes
// the temp dir; the cursor's own `close()` releases the file handle.
function makeDoc(buf: Uint8Array): { path: string; cleanup: () => void } {
  const { dir, cleanup } = createTempDir('bote-matrix-')
  const path = join(dir, 'doc.json')
  writeFileSync(path, buf)
  return { path, cleanup }
}

async function invokeOnce(cursor: Cursor, cell: Cell, path: Path): Promise<number> {
  switch (cell.op) {
    case 'get':
      await cursor.get(...path)
      return 1
    case 'has':
      await cursor.has(...path)
      return 1
    case 'iter': {
      if (cell.accessPattern === 'obj-iter-first') {
        // Time to the first item through the default item iterator (the path a
        // caller actually writes); `batch: 1` makes the fetch yield it eagerly.
        for await (const _item of cursor.iter(...path, { batch: 1 })) return 1
        return 0
      }
      const batch = cell.batch ?? DEFAULT_ITER_BATCH
      const opts = cell.accessPattern === 'obj-iter-name' ? { batch, select: 'name' } : { batch }
      if (cell.consume) return consumeStream(cursor.iter(...path, opts), cell.consume, cell.docSize)
      let n = 0
      for await (const _item of cursor.iter(...path, opts)) n += 1
      return n
    }
  }
}

// Drain one `IterStream` by the cell's consumption mode. Each returns an
// item-ish count purely so the work isn't dead-code-eliminated; the timing is
// what matters. Short-circuiting terminals use predicates that never decide, so
// they traverse the whole stream rather than stopping early.
async function consumeStream(stream: IterStream<unknown>, consume: Consume, size: number): Promise<number> {
  switch (consume) {
    case 'raw': {
      let n = 0
      for await (const b of stream.raw()) n += b.length
      return n
    }
    case 'toArray':
      return (await stream.toArray()).length
    case 'forEach': {
      let n = 0
      await stream.forEach(() => {
        n += 1
      })
      return n
    }
    case 'reduce':
      return stream.reduce((a) => a + 1, 0)
    case 'find':
      await stream.find(() => false)
      return 0
    case 'some':
      await stream.some(() => false)
      return 0
    case 'every':
      await stream.every(() => true)
      return 0
    case 'map':
      return (await stream.map((x) => x).toArray()).length
    case 'filter':
      return (await stream.filter(() => true).toArray()).length
    case 'take':
      return (await stream.take(size).toArray()).length
    case 'drop':
      return (await stream.drop(1).toArray()).length
  }
}

async function measureCell(cell: Cell): Promise<Result> {
  const fixture: DocFixture = buildFixture(cell.docShape, cell.docSize, cell.padWidth)
  const path = fixture.paths[cell.accessPattern]
  if (path === null) {
    throw new Error(`cell ${cell.id}: shape ${cell.docShape} does not support access pattern ${cell.accessPattern}`)
  }
  const { path: docPath, cleanup } = makeDoc(fixture.buf)
  try {
    const cursor = await open(fromFile(docPath, { chunkBytes: cell.chunkBytes }))
    try {
      // Warm the JIT and fault the cell's chunks before timing.
      await warmup(() => invokeOnce(cursor, cell, path), 50)

      const batchMeans = (
        await sample(async () => {
          for (let i = 0; i < cell.iterations; i++) await invokeOnce(cursor, cell, path)
        }, cell.samples)
      ).map((ns) => ns / cell.iterations)

      return { cell, timing: summarizeTiming(batchMeans, cell) }
    } finally {
      await cursor.close()
    }
  } finally {
    cleanup()
  }
}

function summarizeTiming(batchMeans: number[], cell: Cell): Timing {
  const sorted = [...batchMeans].sort((a, b) => a - b)
  const min_ns = sorted[0]
  const firstItem = cell.op === 'iter' && cell.accessPattern === 'obj-iter-first'
  return {
    min_ns,
    p50_ns: percentile(sorted, 0.5),
    mean_ns: mean(batchMeans),
    cv: cv(batchMeans),
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
