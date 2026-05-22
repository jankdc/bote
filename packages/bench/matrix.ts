// Matrix driver.
//
// Enumerates cells (optionally filtered), spawns one fresh node worker
// per cell, captures one structured Result per cell, and streams JSONL
// to stdout (or `--out <path>`). Process isolation is intentional: the
// native chunk cache and bitmap store carry state across invocations.
//
// CLI:
//   --out <path>     write JSONL to <path> instead of stdout
//   --filter <re>    only run cells whose id matches the regex
//   --limit <n>      stop after the first n matching cells
//   --dry-run        print the cell list to stderr and exit

import { execSync, spawn } from 'node:child_process'
import { createWriteStream } from 'node:fs'
import type { Writable } from 'node:stream'

import { defaultCells, type Cell, type Result } from './cells.ts'
import { fmtNs } from './format.ts'

function flag(name: string): boolean {
  return process.argv.includes(name)
}
function arg(name: string): string | null {
  const i = process.argv.indexOf(name)
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : null
}

const outPath = arg('--out')
const filterRe = arg('--filter') ? new RegExp(arg('--filter')!) : null
const limit = arg('--limit') ? Number.parseInt(arg('--limit')!, 10) : Number.POSITIVE_INFINITY
const dryRun = flag('--dry-run')

const workerPath = new URL('./matrix-worker.ts', import.meta.url).pathname

function gitSha(): string {
  try {
    return execSync('git rev-parse HEAD', { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim()
  } catch {
    return process.env.GIT_SHA ?? 'unknown'
  }
}

const meta = { sha: gitSha(), arch: process.arch, platform: process.platform, node: process.version }

let cells: Cell[] = defaultCells()
if (filterRe) cells = cells.filter((c) => filterRe.test(c.id))
if (Number.isFinite(limit)) cells = cells.slice(0, limit)

if (dryRun) {
  for (const c of cells) console.error(c.id)
  console.error(`\n${cells.length} cell(s) would run.`)
  process.exit(0)
}

const sink: Writable = outPath ? createWriteStream(outPath) : process.stdout

interface SpawnOutcome {
  result: Result | null
  raw: string
  exitCode: number | null
  spawnError?: Error
}

async function runCell(cell: Cell): Promise<SpawnOutcome> {
  return new Promise((resolve) => {
    const nodeArgs = ['--experimental-strip-types', '--no-warnings=ExperimentalWarning', workerPath]
    const child = spawn(process.execPath, nodeArgs, { stdio: ['pipe', 'pipe', 'inherit'] })
    let out = ''
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (d) => {
      out += d
    })
    child.on('error', (err) => resolve({ result: null, raw: out, exitCode: null, spawnError: err }))
    child.on('close', (code) => {
      const line = out.trim().split('\n').pop() ?? ''
      let result: Result | null = null
      try {
        if (line) {
          const parsed = JSON.parse(line) as Result
          if (parsed && typeof parsed === 'object') result = parsed
        }
      } catch {
        // fall through with raw output for the driver to log
      }
      resolve({ result, raw: out, exitCode: code })
    })
    child.stdin.write(JSON.stringify(cell))
    child.stdin.end()
  })
}

console.error(`running ${cells.length} cell(s); meta=${JSON.stringify(meta)}`)

let failed = 0
const startedAt = Date.now()

for (const cell of cells) {
  const cellStartedAt = Date.now()
  const outcome = await runCell(cell)
  const durationMs = Date.now() - cellStartedAt

  if (!outcome.result) {
    failed += 1
    const reason = outcome.spawnError
      ? outcome.spawnError.message
      : `exit ${outcome.exitCode}; output: ${outcome.raw.trim()}`
    sink.write(
      JSON.stringify({ cell, meta: { ...meta, date: new Date().toISOString(), durationMs }, error: reason }) + '\n',
    )
    console.error(`✗ ${cell.id}  worker failed: ${reason}`)
    continue
  }

  const result = outcome.result
  result.meta = { ...meta, date: new Date().toISOString(), durationMs }
  sink.write(JSON.stringify(result) + '\n')

  if (result.error) {
    failed += 1
    console.error(`✗ ${cell.id}  ${result.error}`)
    continue
  }
  const ratio = result.reference ? `  ratio=${result.reference.ratio.toFixed(2)}` : ''
  console.error(`✓ ${cell.id}  p50=${fmtNs(result.timing.p50_ns)}${ratio}  (${durationMs} ms)`)
}

if (sink !== process.stdout) (sink as ReturnType<typeof createWriteStream>).end()

const totalMs = Date.now() - startedAt
console.error(
  `\ndone: ${cells.length} cell(s) in ${(totalMs / 1000).toFixed(1)} s` + (failed ? `; ${failed} failed` : ''),
)
process.exit(failed > 0 ? 1 : 0)
