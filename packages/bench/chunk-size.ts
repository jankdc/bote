// Read perf vs chunk size. Cap is held large enough that no eviction
// happens, so this isolates per-chunk overhead from cache-churn effects.

import { open } from '@bote/native'
import { fileSource, withTempDoc, type Pattern } from './fixtures.ts'
import { fmtBytes, fmtNs } from './format.ts'

interface Cell {
  cold: number
  warm: number
}

async function timeOnce(fn: () => Promise<unknown>): Promise<number> {
  const t0 = process.hrtime.bigint()
  await fn()
  return Number(process.hrtime.bigint() - t0)
}

async function timeMedian(iters: number, fn: () => Promise<unknown>): Promise<number> {
  const samples: number[] = []
  for (let i = 0; i < iters; i++) samples.push(await timeOnce(fn))
  samples.sort((a, b) => a - b)
  return samples[Math.floor(samples.length / 2)]
}

async function measure(path: string, chunkBytes: number, maxResidentChunks: number, pattern: Pattern): Promise<Cell> {
  const source = await fileSource(path, chunkBytes)
  try {
    const cursor = open(source, { maxResidentChunks })
    const cold = await timeOnce(() => cursor.get(pattern.pointer))
    const warm = await timeMedian(pattern.iters, () => cursor.get(pattern.pointer))
    return { cold, warm }
  } finally {
    await source.close?.()
  }
}

function printTable(
  title: string,
  rowHeader: string,
  columns: string[],
  rows: Array<{ label: string; cells: string[] }>,
): void {
  console.log(`\n== ${title} ==`)
  const header = [rowHeader].concat(columns).join(' | ')
  console.log(header)
  console.log('-'.repeat(header.length))
  for (const row of rows) console.log(`${row.label} | ${row.cells.join(' | ')}`)
}

const N = 2_000_000
console.log(`Building doc (${N.toLocaleString()} items)…`)
await withTempDoc(N, 7, async (path, buf) => {
  console.log(`Doc size: ${fmtBytes(buf.byteLength)}\n`)

  // Each chunk size must be a non-zero multiple of 64.
  const chunkBytess = [4 * 1024, 16 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024]
  const patterns: Pattern[] = [
    { name: 'shallow', pointer: '/items/0/name', iters: 10 },
    { name: 'mid', pointer: `/items/${Math.floor(N / 2)}/name`, iters: 5 },
    { name: 'deep', pointer: `/items/${N - 1}/name`, iters: 3 },
  ]

  const results = new Map<number, Map<string, Cell>>()
  for (const chunkBytes of chunkBytess) {
    // Cap large enough to fit the full doc, so per-chunk overhead is
    // isolated from eviction churn.
    const cap = Math.ceil(buf.byteLength / chunkBytes)
    const row = new Map<string, Cell>()
    for (const pat of patterns) {
      const cell = await measure(path, chunkBytes, cap, pat)
      row.set(pat.name, cell)
      console.log(
        `  chunk ${fmtBytes(chunkBytes).padStart(7)}  ${pat.name.padEnd(7)}  ` +
          `cold ${fmtNs(cell.cold).padStart(8)}   warm ${fmtNs(cell.warm).padStart(8)}`,
      )
    }
    results.set(chunkBytes, row)
  }

  const columns = patterns.map((p) => p.name.padStart(10))
  const rowsFor = (project: (c: Cell) => string) =>
    chunkBytess.map((chunkBytes) => ({
      label: fmtBytes(chunkBytes).padStart(9),
      cells: patterns.map((p) => project(results.get(chunkBytes)!.get(p.name)!)),
    }))

  printTable(
    'Warm-cursor median query time vs chunk size (cap = full doc, no eviction)',
    'Chunk    ',
    columns,
    rowsFor((c) => fmtNs(c.warm).padStart(10)),
  )
  printTable(
    'Cold (first call) query time vs chunk size - includes bitmap construction',
    'Chunk    ',
    columns,
    rowsFor((c) => fmtNs(c.cold).padStart(10)),
  )
})
process.exit(0)
