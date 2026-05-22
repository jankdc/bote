// V8-memory-constraint stress test.
//
// Asks one question: does the library survive a multi-MB walk when V8
// is given a tight `--max-old-space-size` cap? The library's contract
// is that source bytes live in native Rust memory, not the V8 heap -
// so a 100 MB doc should walk cleanly under e.g. 32 MB of V8 old-space.
//
// Spawns a fresh node child per cap (so V8 state from previous runs
// can't help). Each child runs `stress-worker.ts` against a temp doc;
// the parent collects exit codes. Any OOM crash = test failure.

import { spawn } from 'node:child_process'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { withTempDoc } from './fixtures.ts'
import { fmtBytes } from './format.ts'

const ITEMS = 2_000_000 // ≈ 110 MB at padWidth 7
const PAD_WIDTH = 7
const WORKER = join(dirname(fileURLToPath(import.meta.url)), 'stress-worker.ts')

// V8 needs some headroom for code, semi-space, and parser state; under
// ~24 MB it OOMs before user code runs even on a trivial program. 32 MB
// is the realistic floor for "tight but functional."
const CAPS_MB = [32, 64, 128, 256]

interface Result {
  capMb: number
  exitCode: number | null
  signal: NodeJS.Signals | null
  durationMs: number
  stderrTail: string
}

function runChild(capMb: number, docPath: string): Promise<Result> {
  return new Promise((resolve) => {
    const args = [
      `--max-old-space-size=${capMb}`,
      '--experimental-strip-types',
      '--no-warnings=ExperimentalWarning',
      WORKER,
      docPath,
    ]
    const start = Date.now()
    const child = spawn(process.execPath, args, { stdio: ['ignore', 'inherit', 'pipe'] })
    let stderr = ''
    child.stderr?.on('data', (chunk: Buffer) => {
      stderr += chunk.toString()
    })
    child.on('exit', (code, signal) => {
      resolve({
        capMb,
        exitCode: code,
        signal,
        durationMs: Date.now() - start,
        stderrTail: stderr.trim().split('\n').slice(-3).join(' | '),
      })
    })
  })
}

console.log(`Building doc (${ITEMS.toLocaleString()} items, padWidth ${PAD_WIDTH})…`)
await withTempDoc(ITEMS, PAD_WIDTH, async (path, buf) => {
  console.log(`Doc size: ${fmtBytes(buf.byteLength)}`)
  console.log(`Each child walks every item end-to-end under its --max-old-space-size cap.\n`)

  let failed = false
  for (const capMb of CAPS_MB) {
    const r = await runChild(capMb, path)
    const status = r.exitCode === 0 ? 'PASS' : 'FAIL'
    if (r.exitCode !== 0) failed = true
    const exit = r.exitCode !== null ? `exit=${r.exitCode}` : `signal=${r.signal}`
    console.log(
      `${status}  --max-old-space-size=${String(capMb).padStart(4)} MB  ${exit.padEnd(12)}  ${(r.durationMs / 1000).toFixed(2)}s` +
        (r.stderrTail ? `\n      ${r.stderrTail}` : ''),
    )
  }

  if (failed) {
    console.log(`\nFAIL - at least one cap triggered an OOM. Native bytes are leaking into the V8 heap.`)
    process.exit(1)
  } else {
    console.log(`\nPASS - every cap survived. Source bytes live in native memory, as advertised.`)
  }
})
