import { open as openNative, type CacheStats, type Cursor as NativeCursor } from '@botejs/native'

import type { Source, SourceReader } from './sources.ts'
import { runStandardSchema, validateItem, type Path, type Segment, type StandardSchemaV1 } from './validate.ts'

export interface SessionOptions {
  /**
   * Maximum number of source chunks held resident at once. Each slot
   * accounts for one chunk's bytes plus its bitmaps; the cache also
   * enforces a derived byte ceiling at roughly `maxResidentChunks x
   * source.chunkBytes x 2` to bound total native memory.
   *
   * Defaults to 512 chunks.
   */
  maxResidentChunks?: number
}

type InferOutput<Sch> = Sch extends StandardSchemaV1<unknown, infer O> ? O : never

type SelectMapShape<S> = { -readonly [K in keyof S]: unknown }

/** Zero-based index of an array element. */
export type IterIndex = number

/** Default batch size for `.iter()`. Each yield is an array of up to this many
 *  items; the final batch may be smaller. Sized to amortize the per-yield
 *  FFI/promise overhead across enough items that compute, not protocol, sets
 *  the cost. Override via `IterOptions.batch`. */
export const DEFAULT_ITER_BATCH = 1000

export interface IterOptions {
  select?: Segment | Path | Record<string, Segment | Path>
  /** Override the default batch size of {@link DEFAULT_ITER_BATCH}. Must be a
   *  positive integer. Larger amortizes FFI overhead further at the cost of
   *  per-yield latency and transient JS-heap residency. */
  batch?: number
  /** Validate each yielded item against this schema (after `select`). */
  schema?: StandardSchemaV1
  /** Policy for items failing `schema`. Default `'throw'`; `'skip'` drops them, turning the schema into a filter. */
  onInvalid?: 'throw' | 'skip'
  /** Yield `[index, value]` tuples instead of bare values, where `index` is
   *  the zero-based position of the element in the source array. Useful when
   *  a `schema` with `onInvalid: 'skip'` has dropped items and the caller
   *  needs the original index. */
  withIndex?: boolean
}

type VariadicPathArgs<TTail> = [...Segment[]] | [...Segment[], TTail]

export interface Cursor {
  /** Object-member key or array-element index that this cursor was yielded under by `walk`. `null` on the root cursor. */
  readonly key: string | number | null

  has(...path: Segment[]): Promise<boolean>
  has(...args: [...Segment[], StandardSchemaV1]): Promise<boolean>

  get(...path: Segment[]): Promise<unknown>
  get<Sch extends StandardSchemaV1>(...args: [...Segment[], Sch]): Promise<InferOutput<Sch>>

  count(...path: Segment[]): Promise<number>

  iter(...path: Segment[]): AsyncIterable<unknown[]>
  iter<Sch extends StandardSchemaV1>(...args: [...Segment[], Sch]): AsyncIterable<InferOutput<Sch>[]>
  iter<Sch extends StandardSchemaV1>(
    ...args: [...Segment[], IterOptions & { withIndex: true; schema: Sch }]
  ): AsyncIterable<[IterIndex, InferOutput<Sch>][]>
  iter<Sch extends StandardSchemaV1>(
    ...args: [...Segment[], IterOptions & { schema: Sch }]
  ): AsyncIterable<InferOutput<Sch>[]>
  iter<S extends Record<string, Segment | Path>>(
    ...args: [...Segment[], IterOptions & { withIndex: true; select: S }]
  ): AsyncIterable<[IterIndex, SelectMapShape<S>][]>
  iter<S extends Record<string, Segment | Path>>(
    ...args: [...Segment[], IterOptions & { select: S }]
  ): AsyncIterable<SelectMapShape<S>[]>
  iter(...args: [...Segment[], IterOptions & { withIndex: true }]): AsyncIterable<[IterIndex, unknown][]>
  iter(...args: [...Segment[], IterOptions]): AsyncIterable<unknown[]>

  /** Stream child positions as cursors. */
  walk(...path: Segment[]): AsyncIterable<Cursor>

  /** Live snapshot of the shared chunk-cache occupancy - the bounded-memory contract, observable from JS. */
  cacheStats(): CacheStats
}

/**
 * The cursor returned by `open()`. Owns the underlying `Source` and exposes
 * both an explicit `close()` and `Symbol.asyncDispose` so callers can choose
 * between manual cleanup and `await using` scoping.
 */
export interface RootCursor extends Cursor, AsyncDisposable {
  /** Close the underlying source. Idempotent. */
  close(): Promise<void>
}

/**
 * Open a cursor over a seekable source.
 *
 * Calls `source.open()` to acquire a reader, then constructs the native cursor
 * over it. The reader's `read(offset, buf)` is invoked with chunk-aligned
 * `offset` and a `buf` whose `byteLength` equals the configured chunk size;
 * the reader fills `buf` and resolves with `bytesRead`. `buf` is a view over
 * native-owned memory and **MUST** not be retained past the returned promise.
 *
 * The returned `RootCursor` owns the reader: `close()` (or `await using`)
 * drives the reader's own `close()` exactly once.
 */
export async function open(source: Source, options?: SessionOptions): Promise<RootCursor> {
  const reader = await source.open()
  let native: NativeCursor
  try {
    native = openNative(
      {
        size: reader.size,
        chunkBytes: reader.chunkBytes,
        read: async ({ offset, length }: { offset: number; length: number }) => reader.read(offset, length),
      },
      {
        maxResidentChunks: options?.maxResidentChunks,
      },
    )
  } catch (err) {
    await closeReader(reader)
    throw err
  }
  let closed = false
  const close = async (): Promise<void> => {
    if (closed) return
    closed = true
    await closeReader(reader)
  }
  return Object.assign(wrap(native), {
    close,
    [Symbol.asyncDispose]: close,
  }) as RootCursor
}

async function closeReader(reader: SourceReader): Promise<void> {
  if (reader.close) await reader.close()
}

/** Upper bound on numeric segments (napi takes them as `u32`). 2^32 - 1
 *  comfortably covers any in-memory JSON array. */
const MAX_ARRAY_INDEX = 0xffffffff

function validatePath(path: readonly unknown[]): asserts path is readonly Segment[] {
  for (let i = 0; i < path.length; i++) {
    const s = path[i]
    if (typeof s === 'string') continue
    if (typeof s === 'number' && Number.isInteger(s) && s >= 0 && s <= MAX_ARRAY_INDEX) continue
    throw new TypeError(
      `path segment ${i}: expected string or non-negative integer (<= ${MAX_ARRAY_INDEX}), got ${describeBadSegment(s)}`,
    )
  }
}

function describeBadSegment(s: unknown): string {
  if (typeof s === 'number') return `${s}`
  if (s === null) return 'null'
  return typeof s
}

function splitArgs<TTail>(args: VariadicPathArgs<TTail>): { path: Segment[]; tail: TTail | undefined } {
  let pathArgs: unknown[]
  let tail: TTail | undefined
  if (args.length === 0) {
    pathArgs = []
    tail = undefined
  } else {
    const last = args[args.length - 1]
    if (last !== null && typeof last === 'object' && !Array.isArray(last)) {
      pathArgs = args.slice(0, -1)
      tail = last as TTail
    } else {
      pathArgs = args as unknown[]
      tail = undefined
    }
  }
  validatePath(pathArgs)
  return { path: pathArgs as Segment[], tail }
}

function isSchema(value: unknown): value is StandardSchemaV1 {
  return typeof value === 'object' && value !== null && '~standard' in value
}

function normalizeIterTail(tail: StandardSchemaV1 | IterOptions | undefined): {
  schema?: StandardSchemaV1
  select?: Segment | Path | Record<string, Segment | Path>
  batch?: number
  onInvalid?: 'throw' | 'skip'
  withIndex?: boolean
} {
  if (!tail) return {}
  if (isSchema(tail)) return { schema: tail }
  return {
    schema: tail.schema,
    select: tail.select,
    batch: tail.batch,
    onInvalid: tail.onInvalid,
    withIndex: tail.withIndex,
  }
}

function serializeSelect(select: Segment | Path | Record<string, Segment | Path>): string {
  if (typeof select === 'string' || typeof select === 'number') {
    const one = [select]
    validatePath(one)
    return JSON.stringify({ one })
  }
  if (Array.isArray(select)) {
    validatePath(select)
    if (select.length === 0) {
      throw new RangeError('iter: select sub-path must have at least one segment')
    }
    return JSON.stringify({ one: select })
  }
  const entries = Object.entries(select).map(([k, sub]) => {
    const path = typeof sub === 'string' || typeof sub === 'number' ? [sub] : sub
    validatePath(path)
    if (path.length === 0) {
      throw new RangeError(`iter: select field ${JSON.stringify(k)} sub-path must have at least one segment`)
    }
    return [k, path] as const
  })
  if (entries.length === 0) {
    throw new RangeError('iter: select must have at least one field')
  }
  return JSON.stringify({ map: entries })
}

function wrap(native: NativeCursor): Cursor {
  const cursor = {
    get key() {
      return native.key
    },
    async has(...args: VariadicPathArgs<StandardSchemaV1>): Promise<boolean> {
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args)
      if (!schema) return native.has(path)
      if (!(await native.has(path))) return false
      const result = await validateItem(schema, await native.get(path), path, 'skip')
      return !('skip' in result)
    },
    async get(...args: VariadicPathArgs<StandardSchemaV1>): Promise<unknown> {
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args)
      const value = await native.get(path)
      // A missing path resolves to `undefined` (JSON values are never
      // `undefined`, so this is unambiguous). There is nothing to validate, so
      // return `undefined` rather than running the schema against it - matches
      // the no-schema `get` and avoids a spurious ValidationError / silent pass.
      if (!schema || value === undefined) return value
      return runStandardSchema(schema, value, path)
    },
    count(...path: Segment[]): Promise<number> {
      validatePath(path)
      return native.count(path)
    },
    iter(...args: VariadicPathArgs<StandardSchemaV1 | IterOptions>): AsyncIterable<unknown[]> {
      const { path, tail } = splitArgs<StandardSchemaV1 | IterOptions>(args)
      const { schema, select, batch, onInvalid, withIndex } = normalizeIterTail(tail)
      if (batch !== undefined && (!Number.isInteger(batch) || batch <= 0)) {
        throw new RangeError(`iter: batch must be a positive integer, got ${batch}`)
      }
      const resolvedBatch = batch ?? DEFAULT_ITER_BATCH
      const selectIr = select !== undefined ? serializeSelect(select) : undefined
      const inner = native.iter(path, { selectIr, batch: resolvedBatch, withKey: withIndex })
      if (!schema) return inner as AsyncIterable<unknown[]>
      const policy = onInvalid ?? 'throw'

      // The native side has already shaped each item inside the batch:
      // `value` when `!withIndex`, `[index, value]` when `withIndex`. Schema
      // validation only ever runs against the value half; the index passes
      // through unchanged. With `onInvalid: 'skip'` a batch may shrink or
      // come back empty.
      return {
        async *[Symbol.asyncIterator]() {
          let i = 0
          for await (const b of inner) {
            const out: unknown[] = []
            for (const v of b as unknown[]) {
              const value = withIndex ? (v as [IterIndex, unknown])[1] : v
              const result = await validateItem(schema, value, [...path, i++], policy)
              if ('skip' in result) continue
              out.push(withIndex ? [(v as [IterIndex, unknown])[0], result.value] : result.value)
            }
            yield out
          }
        },
      }
    },
    walk(...path: Segment[]) {
      validatePath(path)
      return {
        async *[Symbol.asyncIterator]() {
          for await (const child of native.walk(path)) {
            yield wrap(child)
          }
        },
      }
    },
    cacheStats() {
      return native.cacheStats()
    },
  }

  return cursor as Cursor
}
