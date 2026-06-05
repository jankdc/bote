
export async function timeNs(fn: () => Promise<unknown>): Promise<number> {
  const t0 = process.hrtime.bigint()
  await fn()
  return Number(process.hrtime.bigint() - t0)
}

export async function warmup(fn: () => Promise<unknown>, ms: number): Promise<void> {
  const deadline = process.hrtime.bigint() + BigInt(Math.round(ms * 1e6))
  do {
    await fn()
  } while (process.hrtime.bigint() < deadline)
}

export async function sample(fn: () => Promise<unknown>, count: number): Promise<number[]> {
  const out: number[] = []
  for (let i = 0; i < count; i++) out.push(await timeNs(fn))
  return out
}

export function percentile(sorted: number[], q: number): number {
  if (sorted.length === 0) return 0
  if (sorted.length === 1) return sorted[0]
  const idx = (sorted.length - 1) * q
  const lo = Math.floor(idx)
  const hi = Math.ceil(idx)
  if (lo === hi) return sorted[lo]
  return sorted[lo] * (1 - (idx - lo)) + sorted[hi] * (idx - lo)
}

export function median(xs: number[]): number {
  return percentile([...xs].sort((a, b) => a - b), 0.5)
}

/** Coefficient of variation (population stddev ÷ mean); 0 when the mean is 0. */
export function cv(xs: number[]): number {
  const m = mean(xs)
  if (m === 0) return 0
  const variance = xs.reduce((a, b) => a + (b - m) ** 2, 0) / xs.length
  return Math.sqrt(variance) / m
}

export function mean(xs: number[]): number {
  return xs.reduce((a, b) => a + b, 0) / xs.length
}
