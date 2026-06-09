// Cold-cache get-by-index showdown: JSON.parse vs bote.
//
//   npm run showcase -w @botejs/bench                       # ~500 MiB synth doc
//   BYTES=1073741824 npm run showcase -w @botejs/bench      # custom size
//   SKIP_PURGE=1 npm run showcase -w @botejs/bench          # warm OS cache (fast iteration)
//
// Internally it re-execs itself as `showcase.ts run ...` once per cell.

import { execSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { performance } from 'node:perf_hooks'

import { fromFile, open } from '@botejs/core'

import { arg } from '#lib/cli.ts'
import { fmtNs } from '#lib/format.ts'
import { runNode } from '#lib/proc.ts'
import { ensureFixture } from '#lib/showcase-fixture.ts'
import { APPROACH_LABEL, APPROACHES } from '#lib/approaches.ts'
import { columnWidths, row, rule } from '#lib/table.ts'

interface RunResult {
  op: string
  approach: string
  index: number
  time_ns: number | null
  error: string | null
}

async function runOnce(approach: string, file: string, idx: number): Promise<number> {
  if (approach === 'json-parse') {
    const t0 = performance.now()
    const parsed = JSON.parse(readFileSync(file, 'utf8')) as unknown[]
    const item = parsed[idx]
    void item
    return (performance.now() - t0) * 1e6
  }
  if (approach === 'bote') {
    const t0 = performance.now()
    const cursor = await open(fromFile(file))
    try {
      const item = await cursor.get(idx)
      void item
      return (performance.now() - t0) * 1e6
    } finally {
      await cursor.close()
    }
  }
  throw new Error(`unknown approach: ${approach}`)
}

function fmtX(x: number): string {
  if (x >= 100) return `${Math.round(x)}×`
  return `${x.toFixed(x >= 10 ? 1 : 2)}×`
}

function renderTable(results: RunResult[], cold: boolean): void {
  const headers = ['operation', 'approach', 'time', 'vs JSON.parse']
  const ops: string[] = []
  const parseByOp = new Map<string, number>()
  for (const r of results) {
    if (!ops.includes(r.op)) ops.push(r.op)
    if (r.approach === 'json-parse' && r.time_ns) parseByOp.set(r.op, r.time_ns)
  }
  const find = (op: string, approach: string): RunResult | undefined =>
    results.find((r) => r.op === op && r.approach === approach)
  const timeCell = (r: RunResult | undefined): string =>
    r === undefined ? '-' : r.error !== null ? 'FAILED' : fmtNs(r.time_ns ?? 0)
  const ratioCell = (op: string, r: RunResult | undefined): string => {
    if (r?.approach === 'json-parse') return '1×'
    const parse = parseByOp.get(op)
    if (!parse || !r?.time_ns) return '-'
    return fmtX(parse / r.time_ns)
  }
  const data: string[][] = []
  for (const op of ops) {
    for (const approach of APPROACHES) {
      const r = find(op, approach)
      data.push([op, APPROACH_LABEL[approach], timeCell(r), ratioCell(op, r)])
    }
  }
  const widths = columnWidths(headers, data)
  console.log('')
  console.log(
    cold
      ? 'COLD start (OS page cache purged before each cell)'
      : 'WARM (OS cache left primed — NOT a cold-start result)',
  )
  console.log(row(headers, widths))
  console.log(rule(widths))
  for (const r of data) console.log(row(r, widths))
  console.log('')
}

// --- worker mode: one cold measurement, one JSON line ---
if (process.argv[2] === 'run') {
  const approach = arg('--approach')
  const file = arg('--file')
  const indexStr = arg('--index')
  const op = arg('--op')
  if (!approach || !file || indexStr === null || !op) {
    console.error('usage: showcase.ts run --approach <approach> --file <path> --index <N> --op <label>')
    process.exit(1)
  }
  const index = Number.parseInt(indexStr, 10)
  const result: RunResult = { op, approach, index, time_ns: null, error: null }
  try {
    result.time_ns = await runOnce(approach, file, index)
  } catch (e) {
    result.error = (e as Error).message
  }
  process.stdout.write(JSON.stringify(result) + '\n')
  process.exit(0)
}

// --- orchestrator mode (default) ---
const targetBytes = Number.parseInt(arg('--bytes') ?? process.env.BYTES ?? `${500 * 1024 * 1024}`, 10)
const skipPurge = Boolean(process.env.SKIP_PURGE)
const selfPath = new URL(import.meta.url).pathname

const { filePath, count } = ensureFixture(targetBytes)
console.error(`fixture: ${filePath} (${count.toLocaleString()} items)`)

if (!skipPurge) {
  try {
    execSync('command -v purge', { stdio: 'ignore' })
  } catch {
    console.error("error: 'purge' not found (needed to drop the OS page cache). Set SKIP_PURGE=1 to run warm.")
    process.exit(1)
  }
  console.error('[sudo] priming credentials (you may be prompted once)…')
  execSync('sudo -v', { stdio: 'inherit' })
}

function dropCaches(): void {
  if (skipPurge) {
    console.error('[skip-purge] OS page cache left warm')
    return
  }
  // Refresh the sudo timestamp each time so long cold reads between cells
  // don't let it lapse into a re-prompt, then purge non-interactively.
  execSync('sudo -v && sudo -n purge', { stdio: 'inherit' })
}

async function runCell(op: string, approach: string, index: number): Promise<RunResult> {
  const args = [selfPath, 'run', '--approach', approach, '--index', String(index), '--op', op, '--file', filePath]
  const { stdout } = await runNode(args)
  const line = stdout.trim().split('\n').pop() ?? ''
  try {
    return JSON.parse(line) as RunResult
  } catch {
    return { op, approach, index, time_ns: null, error: `no result (output: ${stdout.trim()})` }
  }
}

const cells: Array<{ op: string; index: number }> = [
  { op: 'first item', index: 0 },
  { op: `middle item (arr[${Math.floor(count / 2)}])`, index: Math.floor(count / 2) },
  { op: `last item (arr[${count - 1}])`, index: count - 1 },
]

const results: RunResult[] = []
for (const { op, index } of cells) {
  for (const approach of APPROACHES) {
    dropCaches()
    console.error(`[run] op='${op}' approach=${approach} index=${index}`)
    results.push(await runCell(op, approach, index))
  }
}

renderTable(results, !skipPurge)
process.exit(0)
