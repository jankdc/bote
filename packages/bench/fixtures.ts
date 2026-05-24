// Doc builders, napi-shape Source adapters, temp-doc lifecycle, and the
// shape/pattern table used by the matrix worker.

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

export interface Source {
  size: number
  /** Preferred read granularity (non-zero multiple of 64). Defaults to
   *  64 KiB inside the native binding when omitted. */
  chunkBytes?: number
  /** Fill `args.buf` starting at `args.offset` and resolve with bytesRead.
   *  `args.buf` is native-owned and must not be retained past the promise. */
  read(args: { offset: number; buf: Uint8Array }): Promise<number>
  /** Release resources owned by the source (e.g. an open file handle).
   *  Call after the consuming cursor is no longer in use. */
  close?(): Promise<void>
}

export function memorySource(data: Uint8Array, chunkBytes?: number): Source {
  return {
    size: data.length,
    chunkBytes,
    read: ({ offset, buf }) => {
      const end = Math.min(offset + buf.byteLength, data.length)
      const n = Math.max(0, end - offset)
      if (n > 0) buf.set(data.subarray(offset, end))
      return Promise.resolve(n)
    },
  }
}

export async function fileSource(path: string, chunkBytes?: number): Promise<Source> {
  const { open: fsOpen } = await import('node:fs/promises')
  const handle = await fsOpen(path, 'r')
  const stat = await handle.stat()
  return {
    size: stat.size,
    chunkBytes,
    read: async ({ offset, buf }) => {
      const { bytesRead } = await handle.read(buf, 0, buf.byteLength, offset)
      return bytesRead
    },
    close: () => handle.close(),
  }
}

// `padWidth` zero-pads the per-item `name` field, shifting every item's
// byte size by exactly one character. Small benches use 6 (≈54 B/item);
// the 100 MB scaling bench uses 7 so 2M items lands on a round ~110 MB.
// Don't change a script's padWidth without refreshing its baseline.
export function buildArrayDoc(n: number, padWidth: number): Uint8Array {
  const parts: string[] = ['{"items":[']
  for (let i = 0; i < n; i++) {
    if (i > 0) parts.push(',')
    parts.push(`{"id":${i},"name":"item-${String(i).padStart(padWidth, '0')}","tags":["a","b"]}`)
  }
  parts.push(']}')
  return new TextEncoder().encode(parts.join(''))
}

export async function withTempDoc<T>(
  items: number,
  padWidth: number,
  fn: (path: string, buf: Uint8Array) => Promise<T>,
): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), 'bote-bench-'))
  try {
    const buf = buildArrayDoc(items, padWidth)
    const path = join(dir, 'doc.json')
    writeFileSync(path, buf)
    return await fn(path, buf)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
}

export interface Pattern {
  name: string
  pointer: string
  /** Iterations to median; tune so total measurement fits a few seconds. */
  iters: number
}

export type DocShape = 'array-of-objects' | 'deep-nested' | 'wide-flat'
export type FixturePattern = 'shallow' | 'mid' | 'deep' | 'walk-all' | 'iter-all' | 'walk-get-name' | 'walk-first'

export interface DocFixture {
  shape: DocShape
  buf: Uint8Array
  /** Pointer per access pattern; `null` means the shape doesn't support
   *  that pattern (e.g. `walk-all` on a deep-nested doc). */
  pointers: Record<FixturePattern, string | null>
}

function buildArrayOfObjects(items: number, padWidth: number): DocFixture {
  return {
    shape: 'array-of-objects',
    buf: buildArrayDoc(items, padWidth),
    pointers: {
      shallow: '/items/0/name',
      mid: `/items/${Math.floor(items / 2)}/name`,
      deep: `/items/${items - 1}/name`,
      'walk-all': '/items',
      'iter-all': '/items',
      'walk-get-name': '/items',
      'walk-first': '/items',
    },
  }
}

// `{"a":{"a":...{"name":"leaf-N"},"name":"leaf-N-1"}...,"name":"leaf-0"}`.
// Every level carries a sibling `name` so every access pattern resolves
// to a leaf string (not a sub-object). Walk/iter aren't meaningful here:
// each level has one child, one key path.
function buildDeepNested(depth: number, padWidth: number): DocFixture {
  let body = `"name":"leaf-${String(depth).padStart(padWidth, '0')}"`
  for (let i = depth - 1; i >= 0; i--) {
    body = `"a":{${body}},"name":"leaf-${String(i).padStart(padWidth, '0')}"`
  }
  const ptr = (d: number): string => '/a'.repeat(d) + '/name'
  return {
    shape: 'deep-nested',
    buf: new TextEncoder().encode(`{${body}}`),
    pointers: {
      shallow: ptr(0),
      mid: ptr(Math.floor(depth / 2)),
      deep: ptr(depth),
      'walk-all': null,
      'iter-all': null,
      'walk-get-name': null,
      'walk-first': null,
    },
  }
}

// `{"k_000000":"v_000000",...}` - wide fanout off the root, no nesting.
// Children are leaf strings, so `walk-get-name` doesn't apply.
function buildWideFlat(keys: number, padWidth: number): DocFixture {
  const parts: string[] = ['{']
  for (let i = 0; i < keys; i++) {
    if (i > 0) parts.push(',')
    const k = String(i).padStart(padWidth, '0')
    parts.push(`"k_${k}":"v_${k}"`)
  }
  parts.push('}')
  const key = (i: number): string => `/k_${String(i).padStart(padWidth, '0')}`
  return {
    shape: 'wide-flat',
    buf: new TextEncoder().encode(parts.join('')),
    pointers: {
      shallow: key(0),
      mid: key(Math.floor(keys / 2)),
      deep: key(keys - 1),
      'walk-all': '',
      'iter-all': '',
      'walk-get-name': null,
      'walk-first': '',
    },
  }
}

export function buildFixture(shape: DocShape, scale: number, padWidth: number): DocFixture {
  switch (shape) {
    case 'array-of-objects':
      return buildArrayOfObjects(scale, padWidth)
    case 'deep-nested':
      return buildDeepNested(scale, padWidth)
    case 'wide-flat':
      return buildWideFlat(scale, padWidth)
  }
}
