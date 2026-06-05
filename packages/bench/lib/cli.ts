// Shared argv helpers for the bench scripts.

/** Value following `--name`, or null if absent / at end of argv. */
export function arg(name: string): string | null {
  const i = process.argv.indexOf(name)
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : null
}

/** True if `--name` is present. */
export function flag(name: string): boolean {
  return process.argv.includes(name)
}

/** Parse JSONL, skipping blank/malformed lines. */
export function parseJsonl<T>(text: string, onError?: (msg: string) => void): T[] {
  const out: T[] = []
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed) continue
    try {
      out.push(JSON.parse(trimmed) as T)
    } catch (e) {
      onError?.((e as Error).message)
    }
  }
  return out
}
