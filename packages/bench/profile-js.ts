// JS-layer retention profile for the `bote` facade.
//
// Verifies two properties of the JS layer, in two independent phases so
// each phase's bookkeeping can't pollute the other's signal:
//
//   1. heap-plateau phase - iterates N items (the common consumption path),
//      samples `process.memoryUsage().heapUsed` (after a forced GC) at
//      regular intervals, keeps no per-item state. If the facade retained
//      resolved values or async-iterator state, heap would climb linearly
//      with N. A flat tail is the pass condition.
//
//   2. weakref phase - walks a smaller sample, records a WeakRef per
//      yielded Cursor wrapper, drops the strong reference at the loop
//      boundary, forces GC, asserts all refs deref to undefined. This
//      phase keeps the refs array around, so it intentionally inflates
//      heapUsed - which is why it doesn't share a run with phase 1. Stays
//      on `walk` because the property under test is specifically that
//      walked Cursor wrappers are reclaimable.
//
// Must be run with `--expose-gc`; the harness asserts gc is available
// and exits otherwise.

import { open, fromFile } from '@botejs/core'

import { withTempDoc, withTempObjectDoc } from './fixtures.ts'
import { fmtBytes } from './format.ts'

const gc = (globalThis as { gc?: () => void }).gc
if (!gc) {
  console.error('profile-js requires --expose-gc. Run via `yarn workspace @botejs/bench profile:js`.')
  process.exit(1)
}

// Drain microtasks and run GC a couple of times. V8 occasionally needs a
// second pass to collect through async iterator continuations.
async function collect(): Promise<void> {
  await new Promise<void>((r) => setImmediate(r))
  gc!()
  await new Promise<void>((r) => setImmediate(r))
  gc!()
}

const HEAP_ITEMS = 500_000
const HEAP_SAMPLE_EVERY = 50_000
const WEAKREF_ITEMS = 50_000
const PAD_WIDTH = 7

interface HeapSample {
  itemsSeen: number
  heapDelta: number
}

async function heapPlateauPhase(path: string): Promise<HeapSample[]> {
  await using cursor = await open(fromFile(path))
  await collect()
  const baseline = process.memoryUsage().heapUsed
  const samples: HeapSample[] = []
  let seen = 0
  outer: for await (const batch of cursor.iter('items', { select: ['name'] })) {
    for (let i = 0; i < batch.length; i++) {
      seen += 1
      if (seen % HEAP_SAMPLE_EVERY === 0) {
        await collect()
        samples.push({ itemsSeen: seen, heapDelta: process.memoryUsage().heapUsed - baseline })
      }
      if (seen >= HEAP_ITEMS) break outer
    }
  }
  await collect()
  samples.push({ itemsSeen: seen, heapDelta: process.memoryUsage().heapUsed - baseline })
  return samples
}

async function weakRefPhase(path: string): Promise<{ total: number; alive: number }> {
  await using cursor = await open(fromFile(path))
  const refs: WeakRef<object>[] = []
  let seen = 0
  for await (const [, child] of cursor.walk('items')) {
    await child.get('name')
    refs.push(new WeakRef(child))
    seen += 1
    if (seen >= WEAKREF_ITEMS) break
  }
  await collect()
  let alive = 0
  for (const r of refs) if (r.deref() !== undefined) alive += 1
  return { total: refs.length, alive }
}

const verdicts: string[] = []
let failed = false

console.log(`Building doc (${HEAP_ITEMS.toLocaleString()} items, padWidth ${PAD_WIDTH})…`)
await withTempDoc(HEAP_ITEMS, PAD_WIDTH, async (path, buf) => {
  console.log(`Doc size: ${fmtBytes(buf.byteLength)}`)

  console.log(
    `\n[phase 1] heap plateau - iterating ${HEAP_ITEMS.toLocaleString()} items, sample every ${HEAP_SAMPLE_EVERY.toLocaleString()}\n`,
  )
  const samples = await heapPlateauPhase(path)
  for (const s of samples) {
    console.log(`  after ${s.itemsSeen.toLocaleString().padStart(8)} items : ${fmtBytes(s.heapDelta).padStart(10)}`)
  }
  // Compare tail to midpoint. Real retention is linear in items walked;
  // anything under ~4 B/item is V8/GC noise (compilation caches, hidden
  // class allocations, etc., none of which scale with iteration count).
  const mid = samples[Math.floor(samples.length / 2)]
  const last = samples[samples.length - 1]
  const tailGrowth = last.heapDelta - mid.heapDelta
  const tailItems = last.itemsSeen - mid.itemsSeen
  const perItem = tailGrowth / Math.max(1, tailItems)
  if (perItem < 4) {
    verdicts.push(
      `PASS  heap plateau (tail ${fmtBytes(tailGrowth)} over ${tailItems.toLocaleString()} items, ${perItem.toFixed(2)} B/item)`,
    )
  } else {
    verdicts.push(
      `FAIL  heap grew ${fmtBytes(tailGrowth)} over last ${tailItems.toLocaleString()} items ` +
        `(${perItem.toFixed(2)} B/item) - JS layer is retaining per-item state`,
    )
    failed = true
  }

  console.log(
    `\n[phase 2] WeakRef liveness - collecting ${WEAKREF_ITEMS.toLocaleString()} refs, forcing GC, checking deref\n`,
  )
  const wr = await withTempObjectDoc(WEAKREF_ITEMS, PAD_WIDTH, (objPath) => weakRefPhase(objPath))
  console.log(
    `  ${(wr.total - wr.alive).toLocaleString()} of ${wr.total.toLocaleString()} collected, ${wr.alive.toLocaleString()} still alive`,
  )
  // Tolerate a tiny number alive: the final iteration's Cursor can
  // survive in an async generator slot until the loop fully unwinds.
  const tolerance = Math.max(2, Math.floor(wr.total * 0.001))
  if (wr.alive <= tolerance) {
    verdicts.push(`PASS  WeakRefs collected (${wr.alive} alive ≤ tolerance ${tolerance})`)
  } else {
    verdicts.push(
      `FAIL  ${wr.alive} WeakRefs still alive (tolerance ${tolerance}) - facade is retaining yielded Cursors`,
    )
    failed = true
  }

  console.log('')
  for (const v of verdicts) console.log(v)
  if (failed) process.exit(1)
})
