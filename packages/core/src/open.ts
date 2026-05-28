import { open as openNative, type CacheStats, type Cursor as NativeCursor } from '@botejs/native'

import type { PointerLiteral, Pointer } from './pointer.ts'
import type { Source, SourceReader } from './sources.ts'
import { runStandardSchema, validateItem, type StandardSchemaV1 } from './validate.ts'

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

/** Zero-based index of an array element. */
export type IterIndex = number

/** Default batch size for `.iter()`. Each yield is an array of up to this many
 *  items; the final batch may be smaller. Sized to amortize the per-yield
 *  FFI/promise overhead across enough items that compute, not protocol, sets
 *  the cost. Override via `IterOptions.batch`. */
export const DEFAULT_ITER_BATCH = 1000

export interface IterOptions {
  /** Project each child before it crosses: a sub-pointer yields the bare value;
   *  a map yields an object of those sub-values. */
  select?: string | Record<string, string>
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

export interface Cursor {
  /** Object-member key or array-element index that this cursor was yielded under by `walk`. `null` on the root cursor. */
  readonly key: string | number | null

  has<S extends string>(pointer: PointerLiteral<S> | Pointer): Promise<boolean>
  has<S extends string>(pointer: PointerLiteral<S> | Pointer, schema: StandardSchemaV1): Promise<boolean>

  get<S extends string>(pointer: PointerLiteral<S> | Pointer): Promise<unknown>
  get<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    schema: Sch,
  ): Promise<InferOutput<Sch>>

  count<S extends string>(pointer: PointerLiteral<S> | Pointer): Promise<number>

  iter<S extends string>(pointer: PointerLiteral<S> | Pointer): AsyncIterable<unknown[]>
  iter<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    schema: Sch,
  ): AsyncIterable<InferOutput<Sch>[]>
  // withKey overloads precede the non-withKey ones so TS resolves the
  // tuple-yielding signatures first.
  iter<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    options: IterOptions & { withKey: true; schema: Sch },
  ): AsyncIterable<[IterIndex, InferOutput<Sch>][]>
  iter<S extends string>(
    pointer: PointerLiteral<S> | Pointer,
    options: IterOptions & { withKey: true },
  ): AsyncIterable<[IterIndex, unknown][]>
  iter<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    options: IterOptions & { schema: Sch },
  ): AsyncIterable<InferOutput<Sch>[]>
  iter<S extends string>(pointer: PointerLiteral<S> | Pointer, options: IterOptions): AsyncIterable<unknown[]>

  /** Stream child positions as cursors. */
  walk<S extends string>(pointer: PointerLiteral<S> | Pointer): AsyncIterable<Cursor>

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

function normalizeIterArgs(arg?: StandardSchemaV1 | IterOptions): {
  schema?: StandardSchemaV1
  select?: string | Record<string, string>
  batch?: number
  onInvalid?: 'throw' | 'skip'
  withKey?: boolean
} {
  if (!arg) return {}
  if ('~standard' in arg) return { schema: arg as StandardSchemaV1 }
  const options = arg as IterOptions
  return {
    schema: options.schema,
    select: options.select,
    batch: options.batch,
    onInvalid: options.onInvalid,
    withKey: options.withIndex,
  }
}

function serializeSelect(select: string | Record<string, string>): string {
  if (typeof select === 'string') return JSON.stringify({ one: select })
  const entries = Object.entries(select)
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
    async has(pointer: string, schema?: StandardSchemaV1): Promise<boolean> {
      if (!schema) return native.has(pointer)
      if (!(await native.has(pointer))) return false
      const result = await schema['~standard'].validate(await native.get(pointer))
      return result.issues === undefined
    },
    async get(pointer: string, schema?: StandardSchemaV1): Promise<unknown> {
      const value = await native.get(pointer)
      return schema ? runStandardSchema(schema, value, pointer) : value
    },
    count(pointer: string): Promise<number> {
      return native.count(pointer)
    },
    iter(pointer: string, optionsOrSchema?: StandardSchemaV1 | IterOptions): AsyncIterable<unknown[]> {
      const { schema, select, batch, onInvalid, withKey } = normalizeIterArgs(optionsOrSchema)
      if (batch !== undefined && (!Number.isInteger(batch) || batch <= 0)) {
        throw new RangeError(`iter: batch must be a positive integer, got ${batch}`)
      }
      const resolvedBatch = batch ?? DEFAULT_ITER_BATCH
      const selectIr = select !== undefined ? serializeSelect(select) : undefined
      const inner = native.iter(pointer, { selectIr, batch: resolvedBatch, withKey })
      if (!schema) return inner as AsyncIterable<unknown[]>
      const policy = onInvalid ?? 'throw'

      // The native side has already shaped each item inside the batch:
      // `value` when `!withKey`, `[key, value]` when `withKey`. Schema
      // validation only ever runs against the value half; the key passes
      // through unchanged. With `onInvalid: 'skip'` a batch may shrink or
      // come back empty.
      return {
        async *[Symbol.asyncIterator]() {
          let i = 0
          for await (const b of inner) {
            const out: unknown[] = []
            for (const v of b as unknown[]) {
              const value = withKey ? (v as [IterIndex, unknown])[1] : v
              const result = await validateItem(schema, value, `${pointer}/${i++}`, policy)
              if ('skip' in result) continue
              out.push(withKey ? [(v as [IterIndex, unknown])[0], result.value] : result.value)
            }
            yield out
          }
        },
      }
    },
    walk(pointer: string) {
      return {
        async *[Symbol.asyncIterator]() {
          for await (const child of native.walk(pointer)) {
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
