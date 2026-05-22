// Cold-cache JSON.parse vs bote showdown.
//
// This file is a multi-tool with three subcommands; the bash orchestrator
// in `showcase.sh` drives them so each measurement runs in its own node
// process with an empty bote chunk cache, an empty bitmap store, and (with
// `purge` between rows) a cold OS page cache.
//
//   showcase.ts fixture --bytes <N> [--file <path>]
//     Ensures a ~N-byte JSON-array fixture exists. Prints
//     `{ filePath, count, bytes }` to stdout.
//
//   showcase.ts run --approach <json-parse|bote> --file <path>
//                   --index <N> --op <label>
//     Performs one end-to-end measurement and prints a single JSON line:
//     `{ op, approach, index, time_ns, error }`.
//
//   showcase.ts render <jsonl-path>
//     Reads a results JSONL and prints a table to stdout.

import { closeSync, existsSync, openSync, readFileSync, statSync, writeFileSync, writeSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { performance } from 'node:perf_hooks'

import { heapProfilePeakBytes, heapProfileStart, heapProfileStop } from '@bote/native'
import { fromFile, open } from 'bote'

import { fmtBytes, fmtNs } from './format.ts'

function arg(name: string): string | null {
  const i = process.argv.indexOf(name)
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : null
}

interface FixtureMeta {
  filePath: string
  count: number
  bytes: number
}

interface RunResult {
  op: string
  approach: string
  index: number
  time_ns: number | null
  error: string | null
}

interface MemResult {
  op: string
  approach: string
  index: number
  js_heap_baseline_bytes: number
  js_heap_peak_delta_bytes: number | null
  js_heap_retained_delta_bytes: number | null
  rust_peak_bytes: number | null
  heap_profile_path: string | null
  error: string | null
}

function makeRecord(i: number): object {
  return {
    id: `evt_${i.toString(36)}`,
    type: ['PushEvent', 'PullRequestEvent', 'IssueCommentEvent', 'ForkEvent', 'WatchEvent'][i % 5],
    actor: {
      id: i * 31 + 7,
      login: `user_${i}`,
      display_login: `User ${i}`,
      url: `https://api.github.com/users/user_${i}`,
    },
    repo: {
      id: i * 17 + 3,
      name: `org_${i % 1000}/repo_${i % 5000}`,
      url: `https://api.github.com/repos/org_${i % 1000}/repo_${i % 5000}`,
    },
    payload: {
      ref: `refs/heads/branch_${i % 200}`,
      head: i.toString(16).padStart(40, '0'),
      size: (i % 7) + 1,
      message: `Commit ${i}: touch file_${i % 500}.ts and tidy a few things`,
    },
    public: true,
    created_at: new Date(1700000000000 + i * 1000).toISOString(),
  }
}

function generate(path: string, targetBytes: number): { count: number; bytes: number } {
  const fd = openSync(path, 'w')
  try {
    const chunks: Buffer[] = []
    let bufferedBytes = 0
    let writtenBytes = 0
    const FLUSH_AT = 4 * 1024 * 1024
    const flush = (): void => {
      if (bufferedBytes === 0) return
      const merged = Buffer.concat(chunks, bufferedBytes)
      writeSync(fd, merged)
      writtenBytes += merged.byteLength
      chunks.length = 0
      bufferedBytes = 0
    }
    const push = (s: string): void => {
      const b = Buffer.from(s, 'utf8')
      chunks.push(b)
      bufferedBytes += b.byteLength
      if (bufferedBytes >= FLUSH_AT) flush()
    }
    push('[')
    let count = 0
    while (writtenBytes + bufferedBytes < targetBytes - 2) {
      push((count === 0 ? '' : ',') + JSON.stringify(makeRecord(count)))
      count++
    }
    push(']')
    flush()
    return { count, bytes: writtenBytes }
  } finally {
    closeSync(fd)
  }
}

function ensureFixture(targetBytes: number, customFilePath: string | null): FixtureMeta {
  if (customFilePath) {
    if (!existsSync(customFilePath)) throw new Error(`--file does not exist: ${customFilePath}`)
    const userLast = arg('--last-index')
    if (userLast === null) {
      throw new Error('when passing --file, also pass --last-index <N> (the index of the final array item)')
    }
    const stat = statSync(customFilePath)
    return { filePath: customFilePath, count: Number.parseInt(userLast, 10) + 1, bytes: stat.size }
  }
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
  const t1 = performance.now()
  writeFileSync(sidecarPath, JSON.stringify({ count, bytes }))
  console.error(`wrote ${fmtBytes(bytes)} (${count.toLocaleString()} items) in ${fmtNs((t1 - t0) * 1e6)}`)
  return { filePath, count, bytes }
}

// Capture peak `heapUsed` two ways and keep whichever saw more:
//
//   1) explicit `sample()` calls between known steps (works even when the
//      event loop is blocked, as it is during `JSON.parse`); and
//   2) a 1 ms `setInterval` that fires across every async boundary in the
//      bote path, catching transient allocations that V8 might GC before
//      we reach the next checkpoint (so we don't undercount).
//
// We deliberately do not report RSS: it's a process-wide high-water mark
// driven by node's V8 arena sizing decisions, not by what we're holding
// live.
async function measureMem(
  approach: string,
  file: string,
  idx: number,
  heapProfilePath: string | null,
): Promise<{
  js_heap_baseline_bytes: number
  js_heap_peak_delta_bytes: number
  js_heap_retained_delta_bytes: number
  rust_peak_bytes: number | null
}> {
  const heapBaseline = process.memoryUsage().heapUsed
  let heapPeak = heapBaseline
  const sample = (): void => {
    const h = process.memoryUsage().heapUsed
    if (h > heapPeak) heapPeak = h
  }
  // 1 ms is the floor for setInterval in node; this still gives us
  // hundreds of samples across a multi-hundred-ms bote run.
  const poller = setInterval(sample, 1)
  // The poll callback is purely measurement - don't let it keep the
  // process alive past the work.
  poller.unref()

  let rustPeak: number | null = null
  // Hold the retrieved item in a ref that outlives sampling so we can
  // also report what's retained at end-of-work, not just the transient
  // peak. This is what V8 would still be holding if the caller stashed
  // the result somewhere.
  let retainedItem: unknown = null

  try {
    if (approach === 'json-parse') {
      const text = readFileSync(file, 'utf8')
      sample()
      const parsed = JSON.parse(text) as unknown[]
      sample()
      retainedItem = parsed[idx]
      sample()
    } else if (approach === 'bote') {
      if (heapProfilePath !== null) heapProfileStart(heapProfilePath)
      const cursor = await open(fromFile(file))
      sample()
      try {
        retainedItem = await cursor.get(`/${idx}`)
        sample()
        if (heapProfilePath !== null) rustPeak = heapProfilePeakBytes()
      } finally {
        await cursor.close()
        if (heapProfilePath !== null) heapProfileStop()
      }
    } else {
      throw new Error(`unknown approach: ${approach}`)
    }
  } finally {
    clearInterval(poller)
  }

  // Sample once more with the item still referenced, so the "retained"
  // figure isn't artificially low due to GC running between work and
  // measurement.
  sample()
  const heapRetained = process.memoryUsage().heapUsed
  // Touch the retained item so V8 can't optimize the binding away before
  // this point.
  void retainedItem
  return {
    js_heap_baseline_bytes: heapBaseline,
    js_heap_peak_delta_bytes: heapPeak - heapBaseline,
    js_heap_retained_delta_bytes: heapRetained - heapBaseline,
    rust_peak_bytes: rustPeak,
  }
}

async function runOnce(approach: string, file: string, idx: number): Promise<number> {
  if (approach === 'json-parse') {
    const t0 = performance.now()
    const text = readFileSync(file, 'utf8')
    const parsed = JSON.parse(text) as unknown[]
    const item = parsed[idx]
    const t1 = performance.now()
    void item
    return (t1 - t0) * 1e6
  }
  if (approach === 'bote') {
    const t0 = performance.now()
    const cursor = await open(fromFile(file))
    try {
      const item = await cursor.get(`/${idx}`)
      const t1 = performance.now()
      void item
      return (t1 - t0) * 1e6
    } finally {
      await cursor.close()
    }
  }
  throw new Error(`unknown approach: ${approach}`)
}

function renderMemTable(jsonlPath: string): void {
  const lines = readFileSync(jsonlPath, 'utf8')
    .split('\n')
    .map((s) => s.trim())
    .filter(Boolean)
  const results = lines.map((l) => JSON.parse(l) as MemResult)
  const headers = ['operation', 'approach', 'JS heap peak Δ', 'JS heap retained Δ', 'Rust peak']
  const data: string[][] = []
  for (const r of results) {
    if (r.error !== null) {
      data.push([r.op, r.approach, `FAILED - ${r.error}`, '-', '-'])
      continue
    }
    data.push([
      r.op,
      r.approach,
      fmtBytes(r.js_heap_peak_delta_bytes ?? 0),
      fmtBytes(r.js_heap_retained_delta_bytes ?? 0),
      r.rust_peak_bytes === null ? 'n/a' : fmtBytes(r.rust_peak_bytes),
    ])
  }
  const widths = headers.map((h, i) => Math.max(h.length, ...data.map((row) => row[i].length)))
  const pad = (row: string[]): string => row.map((c, i) => c.padEnd(widths[i])).join('  ')
  const sep = widths.map((w) => '─'.repeat(w)).join('  ')
  console.log('')
  console.log(pad(headers))
  console.log(sep)
  for (const row of data) console.log(pad(row))
  console.log('')
}

function renderTable(jsonlPath: string): void {
  const lines = readFileSync(jsonlPath, 'utf8')
    .split('\n')
    .map((s) => s.trim())
    .filter(Boolean)
  const results = lines.map((l) => JSON.parse(l) as RunResult)
  const headers = ['operation', 'JSON.parse', 'bote']
  const approachKeys = ['json-parse', 'bote']
  const byOp = new Map<string, Record<string, string>>()
  const order: string[] = []
  for (const r of results) {
    if (!byOp.has(r.op)) {
      byOp.set(r.op, {})
      order.push(r.op)
    }
    const row = byOp.get(r.op)!
    row[r.approach] = r.error !== null ? `FAILED - ${r.error}` : fmtNs(r.time_ns ?? 0)
  }
  const data = order.map((op) => [op, ...approachKeys.map((k) => byOp.get(op)![k] ?? '-')])
  const widths = headers.map((h, i) => Math.max(h.length, ...data.map((row) => row[i].length)))
  const pad = (row: string[]): string => row.map((c, i) => c.padEnd(widths[i])).join('  ')
  const sep = widths.map((w) => '─'.repeat(w)).join('  ')
  console.log('')
  console.log(pad(headers))
  console.log(sep)
  for (const row of data) console.log(pad(row))
  console.log('')
}

const sub = process.argv[2]

if (sub === 'fixture') {
  const targetBytes = Number.parseInt(arg('--bytes') ?? `${1024 ** 3}`, 10)
  const customFile = arg('--file')
  const meta = ensureFixture(targetBytes, customFile)
  process.stdout.write(JSON.stringify(meta) + '\n')
  process.exit(0)
}

if (sub === 'run') {
  const approach = arg('--approach')
  const file = arg('--file')
  const indexStr = arg('--index')
  const opLabel = arg('--op')
  if (!approach || !file || indexStr === null || !opLabel) {
    console.error('usage: showcase.ts run --approach <json-parse|bote> --file <path> --index <N> --op <label>')
    process.exit(1)
  }
  const idx = Number.parseInt(indexStr, 10)
  const result: RunResult = { op: opLabel, approach, index: idx, time_ns: null, error: null }
  try {
    result.time_ns = await runOnce(approach, file, idx)
  } catch (e) {
    result.error = (e as Error).message
  }
  process.stdout.write(JSON.stringify(result) + '\n')
  process.exit(0)
}

if (sub === 'mem') {
  const approach = arg('--approach')
  const file = arg('--file')
  const indexStr = arg('--index')
  const opLabel = arg('--op')
  const heapProfilePath = arg('--heap-profile-out')
  if (!approach || !file || indexStr === null || !opLabel) {
    console.error(
      'usage: showcase.ts mem --approach <json-parse|bote> --file <path> --index <N> --op <label> [--heap-profile-out <path>]',
    )
    process.exit(1)
  }
  const idx = Number.parseInt(indexStr, 10)
  // Rust heap profiling is only meaningful for the bote path; ignore --heap-profile-out for json-parse.
  const effectiveHeapProfile = approach === 'bote' ? heapProfilePath : null
  const result: MemResult = {
    op: opLabel,
    approach,
    index: idx,
    js_heap_baseline_bytes: 0,
    js_heap_peak_delta_bytes: null,
    js_heap_retained_delta_bytes: null,
    rust_peak_bytes: null,
    heap_profile_path: effectiveHeapProfile,
    error: null,
  }
  try {
    const m = await measureMem(approach, file, idx, effectiveHeapProfile)
    result.js_heap_baseline_bytes = m.js_heap_baseline_bytes
    result.js_heap_peak_delta_bytes = m.js_heap_peak_delta_bytes
    result.js_heap_retained_delta_bytes = m.js_heap_retained_delta_bytes
    result.rust_peak_bytes = m.rust_peak_bytes
  } catch (e) {
    result.error = (e as Error).message
  }
  process.stdout.write(JSON.stringify(result) + '\n')
  process.exit(0)
}

if (sub === 'render') {
  const path = process.argv[3]
  if (!path) {
    console.error('usage: showcase.ts render <jsonl-path>')
    process.exit(1)
  }
  renderTable(path)
  process.exit(0)
}

if (sub === 'render-mem') {
  const path = process.argv[3]
  if (!path) {
    console.error('usage: showcase.ts render-mem <jsonl-path>')
    process.exit(1)
  }
  renderMemTable(path)
  process.exit(0)
}

console.error('usage: showcase.ts <fixture|run|mem|render|render-mem> [args]')
console.error(
  '  (this is the underlying multi-tool; use ./showcase.sh or ./showcase-mem.sh to orchestrate cold-cache runs)',
)
process.exit(1)
