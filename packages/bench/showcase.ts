// Cold-cache JSON.parse vs bote showdown
//
//   yarn workspace @botejs/bench showcase                  # ~500 MiB synth doc
//   BYTES=1073741824 yarn workspace @botejs/bench showcase # custom size
//   SKIP_PURGE=1 yarn workspace @botejs/bench showcase      # warm OS cache (fast iteration)
//
// Internally it re-execs itself as `showcase.ts run ...` once per cell.

import { execSync, spawn } from 'node:child_process'
import { closeSync, existsSync, openSync, readFileSync, statSync, writeFileSync, writeSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { performance } from 'node:perf_hooks'

import { fromFile, open } from '@botejs/core'

import { arg } from './cli.ts'
import { fmtBytes, fmtNs } from './format.ts'

interface RunResult {
  op: string
  approach: string
  index: number
  time_ns: number | null
  error: string | null
}

// A reasonably rich, realistic record so JSON.parse has honest work to do
function makeRecord(i: number): object {
  return {
    id: `evt_${i.toString(36)}`,
    type: ['PushEvent', 'PullRequestEvent', 'IssueCommentEvent', 'ForkEvent', 'WatchEvent'][i % 5],
    actor: { id: i * 31 + 7, login: `user_${i}`, url: `https://api.github.com/users/user_${i}` },
    repo: { id: i * 17 + 3, name: `org_${i % 1000}/repo_${i % 5000}` },
    payload: {
      ref: `refs/heads/branch_${i % 200}`,
      size: (i % 7) + 1,
      message: `Commit ${i}: tidy file_${i % 500}.ts`,
    },
    public: true,
    created_at: new Date(1700000000000 + i * 1000).toISOString(),
  }
}

// Stream a ~targetBytes JSON array to disk. Streaming (not an in-memory
// builder) is the point: the doc can be far larger than the V8 heap.
function generate(path: string, targetBytes: number): { count: number; bytes: number } {
  const fd = openSync(path, 'w')
  try {
    const chunks: Buffer[] = []
    let buffered = 0
    let written = 0
    const flush = (): void => {
      if (buffered === 0) return
      const merged = Buffer.concat(chunks, buffered)
      writeSync(fd, merged)
      written += merged.byteLength
      chunks.length = 0
      buffered = 0
    }
    const push = (s: string): void => {
      const b = Buffer.from(s, 'utf8')
      chunks.push(b)
      buffered += b.byteLength
      if (buffered >= 4 * 1024 * 1024) flush()
    }
    push('[')
    let count = 0
    while (written + buffered < targetBytes - 2) {
      push((count === 0 ? '' : ',') + JSON.stringify(makeRecord(count)))
      count++
    }
    push(']')
    flush()
    return { count, bytes: written }
  } finally {
    closeSync(fd)
  }
}

// Synth fixture with a sidecar so repeat runs reuse it instead of
// regenerating hundreds of MB each time.
function ensureFixture(targetBytes: number): { filePath: string; count: number; bytes: number } {
  const filePath = join(tmpdir(), `bote-showcase-${targetBytes}.json`)
  const sidecarPath = `${filePath}.meta.json`
  if (existsSync(filePath) && existsSync(sidecarPath)) {
    const stat = statSync(filePath)
    const meta = JSON.parse(readFileSync(sidecarPath, 'utf8')) as { count: number; bytes: number }
    if (stat.size === meta.bytes) {
      console.error(`reusing fixture: ${filePath} (${fmtBytes(stat.size)}, ${meta.count.toLocaleString()} items)`)
      return { filePath, count: meta.count, bytes: meta.bytes }
    }
  }
  console.error(`generating fixture ~${fmtBytes(targetBytes)} at ${filePath}…`)
  const t0 = performance.now()
  const { count, bytes } = generate(filePath, targetBytes)
  writeFileSync(sidecarPath, JSON.stringify({ count, bytes }))
  console.error(
    `wrote ${fmtBytes(bytes)} (${count.toLocaleString()} items) in ${fmtNs((performance.now() - t0) * 1e6)}`,
  )
  return { filePath, count, bytes }
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

function renderTable(results: RunResult[], cold: boolean): void {
  const headers = ['operation', 'JSON.parse', 'bote', 'bote speedup']
  const byOp = new Map<string, Map<string, RunResult>>()
  const order: string[] = []
  for (const r of results) {
    if (!byOp.has(r.op)) {
      byOp.set(r.op, new Map())
      order.push(r.op)
    }
    byOp.get(r.op)!.set(r.approach, r)
  }
  const cell = (r: RunResult | undefined): string =>
    r === undefined ? '-' : r.error !== null ? `FAILED - ${r.error}` : fmtNs(r.time_ns ?? 0)
  const speedup = (op: string): string => {
    const parse = byOp.get(op)!.get('json-parse')
    const bote = byOp.get(op)!.get('bote')
    if (!parse?.time_ns || !bote?.time_ns) return '-'
    return `${(parse.time_ns / bote.time_ns).toFixed(1)}×`
  }
  const data = order.map((op) => [op, cell(byOp.get(op)!.get('json-parse')), cell(byOp.get(op)!.get('bote')), speedup(op)])
  const widths = headers.map((h, i) => Math.max(h.length, ...data.map((row) => row[i].length)))
  const pad = (row: string[]): string => row.map((c, i) => c.padEnd(widths[i])).join('  ')
  console.log('')
  console.log(cold ? 'COLD start (OS page cache purged before each cell)' : 'WARM (OS cache left primed — NOT a cold-start result)')
  console.log(pad(headers))
  console.log(widths.map((w) => '─'.repeat(w)).join('  '))
  for (const row of data) console.log(pad(row))
  console.log('')
}

// --- worker mode: one cold measurement, one JSON line ---
if (process.argv[2] === 'run') {
  const approach = arg('--approach')
  const file = arg('--file')
  const indexStr = arg('--index')
  const op = arg('--op')
  if (!approach || !file || indexStr === null || !op) {
    console.error('usage: showcase.ts run --approach <json-parse|bote> --file <path> --index <N> --op <label>')
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

function runCell(op: string, approach: string, index: number): Promise<RunResult> {
  return new Promise((resolve) => {
    const args = ['--experimental-strip-types', '--no-warnings=ExperimentalWarning', selfPath, 'run']
    args.push('--approach', approach, '--index', String(index), '--op', op, '--file', filePath)
    const child = spawn(process.execPath, args, { stdio: ['ignore', 'pipe', 'inherit'] })
    let out = ''
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (d) => {
      out += d
    })
    child.on('close', () => {
      const line = out.trim().split('\n').pop() ?? ''
      try {
        resolve(JSON.parse(line) as RunResult)
      } catch {
        resolve({ op, approach, index, time_ns: null, error: `no result (output: ${out.trim()})` })
      }
    })
  })
}

const cells: Array<{ op: string; index: number }> = [
  { op: 'first item', index: 0 },
  { op: `middle item (arr[${Math.floor(count / 2)}])`, index: Math.floor(count / 2) },
  { op: `last item (arr[${count - 1}])`, index: count - 1 },
]

const results: RunResult[] = []
for (const { op, index } of cells) {
  for (const approach of ['json-parse', 'bote']) {
    dropCaches()
    console.error(`[run] op='${op}' approach=${approach} index=${index}`)
    results.push(await runCell(op, approach, index))
  }
}

renderTable(results, !skipPurge)
process.exit(0)
