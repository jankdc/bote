// Matrix-based comparison report. Consumes matrix.ts JSONL, joins by
// `cell.id` against a committed baseline, and prints per-cell drift.
//
//   yarn bench                            run matrix + report drift
//   yarn bench --current path/run.jsonl   compare a pre-recorded run
//   yarn bench --update                   refresh the baseline

import { spawn } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'

import type { Cell, Result } from './cells.ts'
import { fmtNs } from './format.ts'

function arg(name: string): string | null {
  const i = process.argv.indexOf(name)
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : null
}
function flag(name: string): boolean {
  return process.argv.includes(name)
}

const baselinePath = arg('--baseline') ?? new URL('./matrix-baseline.jsonl', import.meta.url).pathname
const currentPath = arg('--current')
const update = flag('--update')
const perfSlack = Number.parseFloat(arg('--perf-slack') ?? '1.5')

function parseJsonl(text: string): Result[] {
  const out: Result[] = []
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed) continue
    try {
      out.push(JSON.parse(trimmed) as Result)
    } catch (e) {
      console.error(`skipping malformed JSONL line: ${(e as Error).message}`)
    }
  }
  return out
}

async function runMatrix(): Promise<string> {
  const matrixPath = new URL('./matrix.ts', import.meta.url).pathname
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      ['--experimental-strip-types', '--no-warnings=ExperimentalWarning', matrixPath],
      { stdio: ['ignore', 'pipe', 'inherit'] },
    )
    let buf = ''
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (d) => {
      buf += d
    })
    child.on('error', reject)
    child.on('close', (code) => {
      // The driver exits non-zero on failures, but those are still
      // meaningful comparisons. Only no-output is a hard failure.
      if (!buf.trim()) reject(new Error(`matrix produced no output (exit ${code})`))
      else resolve(buf)
    })
  })
}

async function loadCurrent(): Promise<Result[]> {
  if (currentPath) return parseJsonl(readFileSync(currentPath, 'utf8'))
  console.error('running matrix…')
  return parseJsonl(await runMatrix())
}

interface BaselineEntry {
  cell: Cell
  timing: { p50_ns: number; mean_ns: number; iters_per_sample: number; samples: number; items_per_invocation?: number }
  reference?: { parse_ns: number; ratio: number }
  meta?: { arch: string; platform: string; node: string }
}

// Drop everything that varies across machines or runs: timestamps,
// durations, sha, raw batch samples.
function summarize(r: Result): BaselineEntry {
  return {
    cell: r.cell,
    timing: {
      p50_ns: r.timing.p50_ns,
      mean_ns: r.timing.mean_ns,
      iters_per_sample: r.timing.iters_per_sample,
      samples: r.timing.samples,
      ...(r.timing.items_per_invocation !== undefined ? { items_per_invocation: r.timing.items_per_invocation } : {}),
    },
    ...(r.reference ? { reference: { parse_ns: r.reference.parse_ns, ratio: r.reference.ratio } } : {}),
    ...(r.meta ? { meta: { arch: r.meta.arch, platform: r.meta.platform, node: r.meta.node } } : {}),
  }
}

function writeBaseline(current: Result[]): void {
  const lines = current
    .filter((r) => !r.error)
    .map((r) => JSON.stringify(summarize(r)))
    .join('\n')
  writeFileSync(baselinePath, lines + '\n')
  console.error(`baseline written → ${baselinePath} (${current.length} cell(s))`)
}

interface Verdict {
  cell: string
  status: 'ok' | 'new' | 'gone' | 'error' | 'perf-regression'
  detail: string
}

function compareCell(cur: Result, base: BaselineEntry | undefined): Verdict {
  if (cur.error) return { cell: cur.cell.id, status: 'error', detail: cur.error }
  if (!base) return { cell: cur.cell.id, status: 'new', detail: `p50=${fmtNs(cur.timing.p50_ns)}` }

  const useRatio = cur.reference && base.reference
  const curPerf = useRatio ? cur.reference!.ratio : cur.timing.p50_ns
  const basePerf = useRatio ? base.reference!.ratio : base.timing.p50_ns
  const perfRatio = basePerf > 0 ? curPerf / basePerf : 1
  if (perfRatio > perfSlack) {
    const what = useRatio
      ? `ratio ${curPerf.toFixed(3)} vs ${basePerf.toFixed(3)}`
      : `p50 ${fmtNs(curPerf)} vs ${fmtNs(basePerf)}`
    return {
      cell: cur.cell.id,
      status: 'perf-regression',
      detail: `${what} (${perfRatio.toFixed(2)}× > ${perfSlack}×)`,
    }
  }

  const drift = useRatio ? `ratio ${perfRatio.toFixed(2)}×` : `p50 ${perfRatio.toFixed(2)}×`
  return { cell: cur.cell.id, status: 'ok', detail: drift }
}

const current = await loadCurrent()
if (current.length === 0) {
  console.error('no current results to compare')
  process.exit(1)
}

if (update) {
  writeBaseline(current)
  process.exit(0)
}

if (!existsSync(baselinePath)) {
  console.error(`no baseline at ${baselinePath}; run with --update first.`)
  process.exit(1)
}
const baseline = parseJsonl(readFileSync(baselinePath, 'utf8')) as unknown as BaselineEntry[]
const baseById = new Map<string, BaselineEntry>()
for (const entry of baseline) baseById.set(entry.cell.id, entry)

const verdicts: Verdict[] = current.map((r) => compareCell(r, baseById.get(r.cell.id)))

const seen = new Set(current.map((r) => r.cell.id))
const gone: Verdict[] = []
for (const entry of baseline) {
  if (!seen.has(entry.cell.id)) {
    gone.push({ cell: entry.cell.id, status: 'gone', detail: `was p50=${fmtNs(entry.timing.p50_ns)}` })
  }
}

const symbols: Record<Verdict['status'], string> = {
  ok: '✓',
  new: '+',
  gone: '-',
  error: '!',
  'perf-regression': '✗',
}

const order: Record<Verdict['status'], number> = {
  'perf-regression': 0,
  error: 1,
  new: 2,
  gone: 3,
  ok: 4,
}
const sortedVerdicts = [...verdicts, ...gone].sort(
  (a, b) => order[a.status] - order[b.status] || a.cell.localeCompare(b.cell),
)

for (const v of sortedVerdicts) {
  console.log(`${symbols[v.status]} ${v.status.padEnd(18)} ${v.cell.padEnd(48)} ${v.detail}`)
}

const errors = sortedVerdicts.filter((v) => v.status === 'error')
const regressed = sortedVerdicts.filter((v) => v.status === 'perf-regression').length
const okCount = sortedVerdicts.filter((v) => v.status === 'ok').length
const newCount = sortedVerdicts.filter((v) => v.status === 'new').length
const goneCount = sortedVerdicts.filter((v) => v.status === 'gone').length

console.log('')
console.log(
  `summary: ${okCount} ok, ${regressed} over ${perfSlack}× (informational), ${newCount} new, ${goneCount} gone, ${errors.length} errored`,
)

if (errors.length > 0) {
  console.error(`\n${errors.length} cell(s) errored (failed to run).`)
  process.exit(1)
}
process.exit(0)
