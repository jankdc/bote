// Cell + Result schema and the default cell set the matrix driver runs.
//
// A cell is one fully-specified measurement; the driver spawns a fresh
// node process per cell so the native chunk cache and bitmap store start
// cold (required for honest peak-memory readings).

import type { DocShape, FixturePattern } from './fixtures.ts'

export type Op = 'get' | 'has' | 'walk' | 'iter'
export type AccessPattern = FixturePattern
export type SourceKind = 'memory' | 'file'
export { type DocShape }

export interface Cell {
  /** Stable identifier; join key for baseline comparison. */
  id: string
  op: Op
  source: SourceKind
  docShape: DocShape
  /** Shape-specific scale: items / depth / key count. */
  docSize: number
  padWidth: number
  /** Source `chunkBytes` (bytes, multiple of 64). */
  chunkBytes: number
  maxResidentChunks: number
  accessPattern: AccessPattern
  /** Batches; each batch's mean is one sample for the median/p95 stats. */
  samples: number
  /** Op invocations per batch. For walk/iter one invocation is a full
   *  iteration, so this should usually be 1. */
  iters: number
}

export interface Timing {
  p50_ns: number
  p95_ns: number
  mean_ns: number
  iters_per_sample: number
  samples: number
  batch_means_ns: number[]
  /** For walk/iter: items yielded per invocation. */
  items_per_invocation?: number
}

export interface Reference {
  /** Median ns/op for `JSON.parse(...) + property lookup` on the same
   *  pointer, measured in the same process. */
  parse_ns: number
  /** `timing.p50_ns / reference.parse_ns`. */
  ratio: number
}

export interface Result {
  cell: Cell
  meta?: {
    sha: string
    arch: string
    platform: string
    node: string
    date: string
    durationMs: number
  }
  timing: Timing
  reference?: Reference
  error?: string
}

function mk(c: Omit<Cell, 'id'>): Cell {
  const id =
    `${c.op}.${c.accessPattern}.${c.docShape}.${c.source}` +
    `.n${c.docSize}.cap${c.maxResidentChunks}.cs${c.chunkBytes}`
  return { ...c, id }
}

// Tuned so each batch lands in the 50–500 ms window. Point-access
// micro-times scale with depth, so deep cells get fewer iters than shallow.
function iterCount(scale: number, ap: AccessPattern): number {
  if (ap === 'shallow') return 1000
  if (ap === 'mid') return 100
  if (ap === 'deep') return scale >= 100_000 ? 5 : scale >= 10_000 ? 50 : 100
  return 1
}

const CHUNK_SIZE = 64 * 1024
const PAD_WIDTH = 6

/** The default cell set the driver runs without `--filter`. */
export function defaultCells(): Cell[] {
  const cells: Cell[] = []
  const base = { chunkBytes: CHUNK_SIZE, padWidth: PAD_WIDTH }

  // array-of-objects, memory source: full sweep - the workhorse path.
  for (const docSize of [10_000, 100_000]) {
    for (const cap of [16, 256]) {
      for (const op of ['get', 'has'] as Op[]) {
        for (const ap of ['shallow', 'deep'] as AccessPattern[]) {
          cells.push(mk({
            ...base, op, source: 'memory', docShape: 'array-of-objects', docSize,
            maxResidentChunks: cap, accessPattern: ap,
            samples: 5, iters: iterCount(docSize, ap),
          }))
        }
      }
      // Traversal: walk-all (count children), iter-all (materialize values),
      // walk-get-name (walk + per-child get; closer to real usage).
      for (const [op, ap] of [['walk', 'walk-all'], ['iter', 'iter-all'], ['walk', 'walk-get-name']] as Array<[Op, AccessPattern]>) {
        cells.push(mk({
          ...base, op, source: 'memory', docShape: 'array-of-objects', docSize,
          maxResidentChunks: cap, accessPattern: ap,
          samples: 3, iters: 1,
        }))
      }
    }
  }

  // File source: largest doc, both caps, patterns that drive distinct
  // chunk-load behavior (cold-shallow, cold-deep, full traversal).
  for (const cap of [16, 256]) {
    for (const ap of ['shallow', 'deep'] as AccessPattern[]) {
      cells.push(mk({
        ...base, op: 'get', source: 'file', docShape: 'array-of-objects', docSize: 100_000,
        maxResidentChunks: cap, accessPattern: ap,
        samples: 5, iters: iterCount(100_000, ap),
      }))
    }
    cells.push(mk({
      ...base, op: 'walk', source: 'file', docShape: 'array-of-objects', docSize: 100_000,
      maxResidentChunks: cap, accessPattern: 'walk-get-name',
      samples: 3, iters: 1,
    }))
  }

  // Deep-nested: depth 500 stresses pointer-walking overhead. Point
  // access only - walk/iter aren't meaningful here.
  for (const cap of [16, 256]) {
    for (const ap of ['shallow', 'mid', 'deep'] as AccessPattern[]) {
      cells.push(mk({
        ...base, op: 'get', source: 'memory', docShape: 'deep-nested', docSize: 500,
        maxResidentChunks: cap, accessPattern: ap,
        samples: 5, iters: ap === 'shallow' ? 1000 : ap === 'mid' ? 200 : 100,
      }))
    }
  }

  // Wide-flat: 100k top-level keys (~2.5 MB doc, ~40 chunks at 64 KB).
  // Forces eviction at cap=16, fits resident at cap=256.
  for (const cap of [16, 256]) {
    for (const ap of ['shallow', 'deep'] as AccessPattern[]) {
      cells.push(mk({
        ...base, op: 'get', source: 'memory', docShape: 'wide-flat', docSize: 100_000,
        maxResidentChunks: cap, accessPattern: ap,
        samples: 5, iters: iterCount(100_000, ap),
      }))
    }
    cells.push(mk({
      ...base, op: 'walk', source: 'memory', docShape: 'wide-flat', docSize: 100_000,
      maxResidentChunks: cap, accessPattern: 'walk-all',
      samples: 3, iters: 1,
    }))
  }

  return cells
}
