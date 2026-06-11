// Memory footprint of a single cold get-by-index: JSON.parse vs bote.
//
// The companion to `showcase` (which measures time). It reuses the exact
// same ~500 MB array-of-records fixture and the same first/middle/last
// access pattern, but instead of wall-clock it reports two memory columns:
//
//   - js heap peak delta : peak `heapUsed` over a clean (post-GC) baseline.
//       For JSON.parse this is the whole parsed tree + source string;
//       for bote it's just the tiny facade/cursor state.
//   - rust heap peak : peak live bytes in the native crate (dhat t-gmax),
//       i.e. the transient streaming-scan window. `n/a` for JSON.parse
//       (it has no native heap).
//
//   npm run footprint -w @botejs/bench                   # ~500 MiB synth doc
//   BYTES=1073741824 npm run footprint -w @botejs/bench  # custom size

import { readFileSync } from 'node:fs';

import { fromFile, open } from '@botejs/core';

import { arg } from '#lib/cli.ts';
import { fmtBytes } from '#lib/format.ts';
import { runNode } from '#lib/proc.ts';
import { ensureFixture } from '#lib/showcase-fixture.ts';
import { APPROACH_LABEL, APPROACHES } from '#lib/approaches.ts';
import { columnWidths, row, rule } from '#lib/table.ts';

interface RunResult {
  op: string;
  approach: string;
  index: number;
  js_heap_delta: number | null;
  rust_peak: number | null;
  rust_available: boolean;
  error: string | null;
}

const gc = (globalThis as { gc?: () => void }).gc;

// Drain microtasks and force a GC pass so the baseline excludes startup
// garbage. V8 sometimes needs a second pass through async continuations.
async function collect(): Promise<void> {
  await new Promise<void>((r) => setImmediate(r));
  gc!();
  await new Promise<void>((r) => setImmediate(r));
  gc!();
}

// Native heap-profile hooks only exist when the crate is built with
// `--features heap-profile`; resolve them lazily so the no-feature build
// still runs (rust column just reports n/a).
async function loadHeapProfile(): Promise<{
  start: (path?: string) => void;
  stop: () => void;
  peak: () => number;
} | null> {
  try {
    const native = (await import('@botejs/native')) as {
      heapProfileStart?: (path?: string) => void;
      heapProfileStop?: () => void;
      heapProfilePeakBytes?: () => number;
    };
    if (!native.heapProfileStart || !native.heapProfileStop || !native.heapProfilePeakBytes) {
      return null;
    }
    return { start: native.heapProfileStart, stop: native.heapProfileStop, peak: native.heapProfilePeakBytes };
  } catch {
    return null;
  }
}

async function measure(approach: string, file: string, idx: number): Promise<Omit<RunResult, 'op' | 'index'>> {
  if (approach === 'json-parse') {
    await collect();
    const baseline = process.memoryUsage().heapUsed;
    const parsed = JSON.parse(readFileSync(file, 'utf8')) as unknown[];
    // Peak is right here: source string + full object tree both resident.
    const jsDelta = process.memoryUsage().heapUsed - baseline;
    void parsed[idx];
    return { approach, js_heap_delta: jsDelta, rust_peak: null, rust_available: false, error: null };
  }
  if (approach === 'bote') {
    let hp = await loadHeapProfile();
    await collect();
    const baseline = process.memoryUsage().heapUsed;
    try {
      hp?.start();
    } catch {
      // Hooks present but the crate was built without `--features
      // heap-profile`; carry on and just skip the rust column.
      hp = null;
    }
    let peakHeap = baseline;
    const sampler = setInterval(() => {
      const u = process.memoryUsage().heapUsed;
      if (u > peakHeap) {
        peakHeap = u;
      }
    }, 1);
    const cursor = await open(fromFile(file));
    try {
      const item = await cursor.get(idx);
      void item;
      const rustPeak = hp ? hp.peak() : null;
      const u = process.memoryUsage().heapUsed;
      if (u > peakHeap) {
        peakHeap = u;
      }
      return {
        approach,
        js_heap_delta: peakHeap - baseline,
        rust_peak: rustPeak,
        rust_available: hp !== null,
        error: null,
      };
    } finally {
      clearInterval(sampler);
      hp?.stop();
      await cursor.close();
    }
  }
  throw new Error(`unknown approach: ${approach}`);
}

function renderTable(results: RunResult[]): void {
  const headers = ['operation', 'approach', 'js heap peak delta', 'rust heap peak'];
  const anyRust = results.some((r) => r.approach === 'bote' && !r.rust_available);
  const jsCell = (r: RunResult): string => (r.error !== null ? `FAILED` : fmtBytes(r.js_heap_delta ?? 0));
  const rustCell = (r: RunResult): string => {
    if (r.approach !== 'bote') {
      return 'n/a';
    }
    if (!r.rust_available) {
      return 'n/a*';
    }
    return fmtBytes(r.rust_peak ?? 0);
  };
  // Grouped by approach (all ops for one parser together), JSON.parse first.
  const data: string[][] = [];
  for (const approach of APPROACHES) {
    for (const r of results.filter((x) => x.approach === approach)) {
      data.push([r.op, APPROACH_LABEL[approach], jsCell(r), rustCell(r)]);
    }
  }
  const widths = columnWidths(headers, data);
  console.log('');
  console.log(row(headers, widths));
  console.log(rule(widths));
  for (const r of data) {
    console.log(row(r, widths));
  }
  if (anyRust) {
    console.log('\n* rust heap peak needs `--features heap-profile`; rebuild native and re-run.');
  }
  console.log('');
}

if (process.argv[2] === 'run') {
  if (!gc) {
    console.error('footprint worker requires --expose-gc');
    process.exit(1);
  }
  const approach = arg('--approach');
  const file = arg('--file');
  const indexStr = arg('--index');
  const op = arg('--op');
  if (!approach || !file || indexStr === null || !op) {
    console.error('usage: footprint.ts run --approach <approach> --file <path> --index <N> --op <label>');
    process.exit(1);
  }
  const index = Number.parseInt(indexStr, 10);
  const result: RunResult = {
    op,
    approach,
    index,
    js_heap_delta: null,
    rust_peak: null,
    rust_available: false,
    error: null,
  };
  try {
    Object.assign(result, await measure(approach, file, index));
  } catch (e) {
    result.error = (e as Error).message;
  }
  process.stdout.write(JSON.stringify(result) + '\n');
  process.exit(0);
}

// --- orchestrator mode (default) ---
const targetBytes = Number.parseInt(arg('--bytes') ?? process.env.BYTES ?? `${500 * 1024 * 1024}`, 10);
const selfPath = new URL(import.meta.url).pathname;

const { filePath, count } = ensureFixture(targetBytes);
console.error(`fixture: ${filePath} (${count.toLocaleString()} items)`);

async function runCell(op: string, approach: string, index: number): Promise<RunResult> {
  const args = [
    '--expose-gc',
    selfPath,
    'run',
    '--approach',
    approach,
    '--index',
    String(index),
    '--op',
    op,
    '--file',
    filePath,
  ];
  const { stdout } = await runNode(args);
  const line = stdout.trim().split('\n').pop() ?? '';
  try {
    return JSON.parse(line) as RunResult;
  } catch {
    return {
      op,
      approach,
      index,
      js_heap_delta: null,
      rust_peak: null,
      rust_available: false,
      error: `no result (output: ${stdout.trim()})`,
    };
  }
}

const cells: Array<{ op: string; index: number }> = [
  { op: 'first item', index: 0 },
  { op: `middle item (arr[${Math.floor(count / 2)}])`, index: Math.floor(count / 2) },
  { op: `last item (arr[${count - 1}])`, index: count - 1 },
];

const results: RunResult[] = [];
for (const { op, index } of cells) {
  for (const approach of APPROACHES) {
    console.error(`[run] op='${op}' approach=${approach} index=${index}`);
    results.push(await runCell(op, approach, index));
  }
}

renderTable(results);
process.exit(0);
