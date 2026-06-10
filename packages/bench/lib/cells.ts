// Cell + Result schema and the default cell set the matrix driver runs.
//
// A cell is one fully-specified measurement; the driver spawns a fresh node
// process per cell so the streaming walk starts cold (it stores no chunk or
// bitmap cache across queries).

import type { DocShape, FixturePattern } from './fixtures.ts'

export type Operation = 'get' | 'has' | 'iter'
export type AccessPattern = FixturePattern
export { type DocShape }

export interface Cell {
  id: string
  accessPattern: AccessPattern
  batch?: number
  chunkBytes: number
  docShape: DocShape
  docSize: number
  iterations: number
  op: Operation
  padWidth: number
  samples: number
}

export interface Timing {
  min_ns: number
  p50_ns: number
  mean_ns: number
  samples: number
  iters_per_sample: number
  /** For the `obj-iter-first` pattern: ns to yield the *first* child (min over
   *  samples). Guards against an entry path that scans the whole container
   *  before the first element - that cost is O(doc) here, ~flat when lazy. */
  first_item_ns?: number
  /** Coefficient of variation (stddev / mean) across batch means to avoid jitter. */
  cv: number
}

export interface Result {
  cell: Cell
  timing: Timing
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

function mk(c: Omit<Cell, 'id'>): Cell {
  const batch = c.batch !== undefined ? `.b${c.batch}` : ''
  const id = `${c.op}.${c.accessPattern}.${c.docShape}.n${c.docSize}.cs${c.chunkBytes}${batch}`
  return { ...c, id }
}

function iterCount(scale: number, ap: AccessPattern): number {
  switch (ap) {
    case 'shallow':
      return 1000
    case 'deep':
      return scale >= 100_000 ? 5 : scale >= 10_000 ? 50 : 100
    default:
      return 1
  }
}

const CHUNK_SIZE = 64 * 1024
const PAD_WIDTH = 6

const WALK = 10_000
const ITER = 100_000
const SMALL = 10_000
const LARGE = 1_000_000
const ITER_BATCHES = [1, 10, 100, 1_000, 10_000, 100_000]

export function defaultCells(): Cell[] {
  const cells: Cell[] = []
  const base = { chunkBytes: CHUNK_SIZE, padWidth: PAD_WIDTH }
  const point = (docShape: DocShape, ap: AccessPattern, docSize: number, op: Operation = 'get'): void => {
    cells.push(
      mk({ ...base, op, docShape, docSize, accessPattern: ap, samples: 8, iterations: iterCount(docSize, ap) }),
    )
  }
  const traverse = (docShape: DocShape, op: Operation, ap: AccessPattern, docSize: number): void => {
    cells.push(mk({ ...base, op, docShape, docSize, accessPattern: ap, samples: 8, iterations: 1 }))
  }
  const iterBatch = (docShape: DocShape, docSize: number, batch: number): void => {
    cells.push(
      mk({ ...base, op: 'iter', docShape, docSize, accessPattern: 'iter-all', samples: 8, iterations: 1, batch }),
    )
  }

  // array-of-objects (the workhorse): O(1) entry once at LARGE; deep scan-to-last
  // at both magnitudes (the deep LARGE scan faults a long run of chunks); array
  point('array-of-objects', 'shallow', LARGE)
  point('array-of-objects', 'deep', SMALL)
  point('array-of-objects', 'deep', LARGE)
  point('array-of-objects', 'deep', LARGE, 'has') // has parallels get; one scale to confirm it stays gated

  // iter throughput swept across batch sizes (same doc, batch is the only variable
  // - exposes how crossing overhead amortizes vs per-yield array cost).
  for (const batch of ITER_BATCHES) {
    iterBatch('array-of-objects', ITER, batch)
  }

  // object-of-objects: the object-iteration workhorse. Plain member iteration
  // and member iteration projecting one field, each over WALK members.
  traverse('object-of-objects', 'iter', 'obj-iter', WALK)
  traverse('object-of-objects', 'iter', 'obj-iter-name', WALK)

  // deep-nested: depth (not count) stresses pointer-walking. shallow vs deep
  // brackets the per-level cost; no middle point.
  point('deep-nested', 'shallow', 500)
  cells.push(
    mk({
      ...base,
      op: 'get',
      docShape: 'deep-nested',
      docSize: 500,
      accessPattern: 'deep',
      samples: 8,
      iterations: 100,
    }),
  )

  // wide-flat: worst-case linear key scan to the last member (the class PR#11
  // regressed) plus object iteration over a wide root.
  point('wide-flat', 'deep', LARGE)
  traverse('wide-flat', 'iter', 'obj-iter', WALK)

  // First-yield latency guard: obj-iter-first must yield the first member without
  // resolving the whole container (O(1), not O(doc))- a regression there
  // balloons first_item_ns. The 500k doc exceeds the resident window so the
  // container can't fully reside.
  cells.push(
    mk({
      ...base,
      op: 'iter',
      docShape: 'object-of-objects',
      docSize: 500_000,
      accessPattern: 'obj-iter-first',
      samples: 8,
      iterations: 1,
    }),
  )

  return cells
}
