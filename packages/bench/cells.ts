// Cell + Result schema and the default cell set the matrix driver runs.
//
// A cell is one fully-specified measurement; the driver spawns a fresh
// node process per cell so the native chunk cache and bitmap store start
// cold.

import type { DocShape, FixturePattern } from './fixtures.ts'

export type Operation = 'get' | 'has' | 'walk' | 'iter'
export type AccessPattern = FixturePattern
export type SourceKind = 'memory' | 'file'
export { type DocShape }

export interface Cell {
  id: string
  accessPattern: AccessPattern
  chunkBytes: number
  docShape: DocShape
  docSize: number
  iterations: number
  maxResidentBytes: number
  op: Operation
  padWidth: number
  samples: number
  source: SourceKind
}

export interface Timing {
  min_ns: number
  p50_ns: number
  mean_ns: number
  samples: number
  iters_per_sample: number
  /** For walk/iter: `min_ns / items_per_invocation` (lower is better). */
  ns_per_item?: number
  items_per_invocation?: number
  /** For the `walk-first` pattern: ns to yield the *first* child (min over
   *  samples). Guards against an entry path that scans the whole container
   *  before the first element - that cost is O(doc) here, ~flat when lazy. */
  first_item_ns?: number
  /** Coefficient of variation (stddev / mean) across batch means to avoid jitter. */
  cv: number
}

export interface Reference {
  /** Fastest ns/op for the op-equivalent `JSON.parse(...)` work (parse +
   *  the same logical lookup/traversal) on the same source.
   */
  parse_ns: number
  /** `timing.min_ns / reference.parse_ns`. */
  ratio: number
}

export interface Result {
  cell: Cell
  timing: Timing
  reference?: Reference
  error?: string
  meta?: {
    sha: string
    arch: string
    platform: string
    node: string
    date: string
    durationMs: number
  }
}

// TODO: need to document this
function mk(c: Omit<Cell, 'id'>): Cell {
  const id =
    `${c.op}.${c.accessPattern}.${c.docShape}.${c.source}` +
    `.n${c.docSize}.cap${c.maxResidentBytes}.cs${c.chunkBytes}`
  return { ...c, id }
}

// Tuned so each batch lands in the 50–500 ms window. Point-access
// micro-times scale with depth, so deep cells get fewer iters than shallow.
function iterCount(scale: number, ap: AccessPattern): number {
  switch (ap) {
    case 'shallow':
      return 1000
    case 'mid':
      return 100
    case 'deep':
      return scale >= 100_000 ? 5 : scale >= 10_000 ? 50 : 100
    default:
      return 1
  }
}

const CHUNK_SIZE = 64 * 1024
const PAD_WIDTH = 6

export function defaultCells(): Cell[] {
  const cells: Cell[] = []
  const base = { chunkBytes: CHUNK_SIZE, padWidth: PAD_WIDTH }

  // array-of-objects, memory source: full sweep - the workhorse path.
  for (const docSize of [10_000, 100_000]) {
    for (const cap of [16, 256]) {
      for (const op of ['get', 'has'] as Operation[]) {
        for (const ap of ['shallow', 'deep'] as AccessPattern[]) {
          cells.push(
            mk({
              ...base,
              op,
              source: 'memory',
              docShape: 'array-of-objects',
              docSize,
              maxResidentBytes: cap * CHUNK_SIZE,
              accessPattern: ap,
              samples: 8,
              iterations: iterCount(docSize, ap),
            }),
          )
        }
      }
      // Traversal: walk-all (count children), iter-all (materialize values),
      // walk-get-name (walk + per-child get; closer to real usage).
      for (const [op, ap] of [
        ['walk', 'walk-all'],
        ['iter', 'iter-all'],
        ['walk', 'walk-get-name'],
      ] as Array<[Operation, AccessPattern]>) {
        cells.push(
          mk({
            ...base,
            op,
            source: 'memory',
            docShape: 'array-of-objects',
            docSize,
            maxResidentBytes: cap * CHUNK_SIZE,
            accessPattern: ap,
            samples: 5,
            iterations: 1,
          }),
        )
      }
    }
  }

  // File source: largest doc, both caps, patterns that drive distinct
  // chunk-load behavior (cold-shallow, cold-deep, full traversal).
  for (const cap of [16, 256]) {
    for (const ap of ['shallow', 'deep'] as AccessPattern[]) {
      cells.push(
        mk({
          ...base,
          op: 'get',
          source: 'file',
          docShape: 'array-of-objects',
          docSize: 100_000,
          maxResidentBytes: cap * CHUNK_SIZE,
          accessPattern: ap,
          samples: 8,
          iterations: iterCount(100_000, ap),
        }),
      )
    }
    cells.push(
      mk({
        ...base,
        op: 'walk',
        source: 'file',
        docShape: 'array-of-objects',
        docSize: 100_000,
        maxResidentBytes: cap * CHUNK_SIZE,
        accessPattern: 'walk-get-name',
        samples: 3,
        iterations: 1,
      }),
    )
  }

  // First-child latency guard. Walks `/items` and stops after one element,
  // on a doc far larger than the cache ceiling (cap x chunkBytes) so the
  // array can't be fully resident. Time-to-first-child must stay ~flat: a
  // regression that resolves the container's full extent before yielding the
  // first child would make this O(doc) and balloon min_ns. File source so
  // chunks fault through the cache like real usage.
  for (const cap of [16, 256]) {
    cells.push(
      mk({
        ...base,
        op: 'walk',
        source: 'file',
        docShape: 'array-of-objects',
        docSize: 500_000,
        maxResidentBytes: cap * CHUNK_SIZE,
        accessPattern: 'walk-first',
        samples: 8,
        iterations: 1,
      }),
    )
  }

  // Deep-nested: depth 500 stresses pointer-walking overhead. Point
  // access only - walk/iter aren't meaningful here.
  for (const cap of [16, 256]) {
    for (const ap of ['shallow', 'mid', 'deep'] as AccessPattern[]) {
      cells.push(
        mk({
          ...base,
          op: 'get',
          source: 'memory',
          docShape: 'deep-nested',
          docSize: 500,
          maxResidentBytes: cap * CHUNK_SIZE,
          accessPattern: ap,
          samples: 8,
          iterations: ap === 'shallow' ? 1000 : ap === 'mid' ? 200 : 100,
        }),
      )
    }
  }

  // Wide-flat: 100k top-level keys (~2.5 MB doc, ~40 chunks at 64 KB).
  // Forces eviction at cap=16, fits resident at cap=256.
  for (const cap of [16, 256]) {
    for (const ap of ['shallow', 'deep'] as AccessPattern[]) {
      cells.push(
        mk({
          ...base,
          op: 'get',
          source: 'memory',
          docShape: 'wide-flat',
          docSize: 100_000,
          maxResidentBytes: cap * CHUNK_SIZE,
          accessPattern: ap,
          samples: 8,
          iterations: iterCount(100_000, ap),
        }),
      )
    }
    cells.push(
      mk({
        ...base,
        op: 'walk',
        source: 'memory',
        docShape: 'wide-flat',
        docSize: 100_000,
        maxResidentBytes: cap * CHUNK_SIZE,
        accessPattern: 'walk-all',
        samples: 3,
        iterations: 1,
      }),
    )
  }

  return cells
}

/** A small, stable subset for the CI PR report */
export function commonCells(): Cell[] {
  const base = { chunkBytes: CHUNK_SIZE, padWidth: PAD_WIDTH }
  const shared = {
    ...base,
    source: 'memory' as SourceKind,
    docShape: 'array-of-objects' as DocShape,
    docSize: 100_000,
    maxResidentBytes: 256 * CHUNK_SIZE,
  }
  const cells: Cell[] = []
  for (const [op, ap] of [
    ['get', 'shallow'],
    ['get', 'deep'],
    ['has', 'shallow'],
  ] as Array<[Operation, AccessPattern]>) {
    cells.push(mk({ ...shared, op, accessPattern: ap, samples: 8, iterations: iterCount(shared.docSize, ap) }))
  }
  for (const [op, ap] of [
    ['walk', 'walk-all'],
    ['iter', 'iter-all'],
    ['walk', 'walk-get-name'],
  ] as Array<[Operation, AccessPattern]>) {
    cells.push(mk({ ...shared, op, accessPattern: ap, samples: 5, iterations: 1 }))
  }
  return cells
}
