// Local matrix-based regression gate. Consumes matrix.ts JSONL, joins by
// `cell.id` against a baseline recorded on this machine, and FAILS
// (exit 1) on a regression or a cell that errored.
//
//   yarn bench                            run matrix + gate against baseline
//   yarn bench --current path/run.jsonl   gate a pre-recorded run
//   yarn bench --update                   refresh the local baseline
//
// `--baseline`/`--current` accept any two summarized runs, so A/B-ing two
// builds is just a matter of producing both with `bench:matrix`.

import { spawn } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'

import type { Cell, Result } from './cells.ts'
import { arg, flag } from './cli.ts'

const baselinePath = arg('--baseline') ?? new URL('./matrix-baseline.jsonl', import.meta.url).pathname
const currentPath = arg('--current')
const update = flag('--update')
const relSlack = Number.parseFloat(arg('--rel-slack') ?? '0.25')
const noiseK = Number.parseFloat(arg('--noise-k') ?? '3')

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

async function loadCurrent(): Promise<Result[]> {
  if (currentPath) return parseJsonl(readFileSync(currentPath, 'utf8'))
  console.error('running matrix…')
  return parseJsonl(await runMatrix())
}

interface BaselineEntry {
  cell: Cell
  ratio: number
  cv: number
}

function summarize(r: Result): BaselineEntry {
  return { cell: r.cell, ratio: r.reference?.ratio ?? 0, cv: r.timing.cv }
}

function writeBaseline(current: Result[]): void {
  const kept = current.filter((r) => !r.error && r.reference)
  const lines = kept.map((r) => JSON.stringify(summarize(r))).join('\n')
  writeFileSync(baselinePath, lines + '\n')
  console.error(`baseline written → ${baselinePath} (${kept.length} cell(s))`)
}

function loadBaseline(text: string): BaselineEntry[] {
  const out: BaselineEntry[] = []
  for (const r of parseJsonl(text)) {
    const obj = r as unknown as { ratio?: number; error?: string }
    if (obj.error) continue
    if (typeof obj.ratio === 'number') out.push(r as unknown as BaselineEntry)
    else if (r.reference && r.timing) out.push(summarize(r))
  }
  return out
}

interface Verdict {
  cell: string
  status: 'ok' | 'new' | 'gone' | 'error' | 'perf-regression'
  detail: string
}

function compareCell(cur: Result, base: BaselineEntry | undefined): Verdict {
  if (cur.error) return { cell: cur.cell.id, status: 'error', detail: cur.error }
  const curRatio = cur.reference?.ratio
  if (curRatio === undefined) return { cell: cur.cell.id, status: 'ok', detail: 'no reference (ungated)' }
  if (!base) return { cell: cur.cell.id, status: 'new', detail: `ratio ${curRatio.toFixed(3)}` }

  const drift = base.ratio > 0 ? curRatio / base.ratio : 1
  const noise = Math.max(cur.timing.cv, base.cv)
  const threshold = 1 + Math.max(relSlack, noiseK * noise)
  if (drift > threshold) {
    return {
      cell: cur.cell.id,
      status: 'perf-regression',
      detail: `ratio ${curRatio.toFixed(3)} vs ${base.ratio.toFixed(3)} (${drift.toFixed(2)}x > ${threshold.toFixed(2)}x; noise ${(noise * 100).toFixed(0)}%)`,
    }
  }
  return { cell: cur.cell.id, status: 'ok', detail: `${drift.toFixed(2)}x (noise ${(noise * 100).toFixed(0)}%)` }
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
const baseline = loadBaseline(readFileSync(baselinePath, 'utf8'))
const baseById = new Map<string, BaselineEntry>()
for (const entry of baseline) baseById.set(entry.cell.id, entry)

const verdicts: Verdict[] = current.map((r) => compareCell(r, baseById.get(r.cell.id)))

const seen = new Set(current.map((r) => r.cell.id))
const gone: Verdict[] = []
for (const entry of baseline) {
  if (!seen.has(entry.cell.id)) {
    gone.push({ cell: entry.cell.id, status: 'gone', detail: `was ratio=${entry.ratio.toFixed(3)}` })
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
  `summary: ${okCount} ok, ${regressed} regressed, ${newCount} new, ${goneCount} gone, ${errors.length} errored`,
)

if (regressed > 0 || errors.length > 0) {
  const parts: string[] = []
  if (regressed > 0) parts.push(`${regressed} perf regression(s)`)
  if (errors.length > 0) parts.push(`${errors.length} errored cell(s)`)
  console.error(`\nFAIL - ${parts.join(' and ')}.`)
  process.exit(1)
}
console.error('\nPASS - no regressions.')
process.exit(0)
