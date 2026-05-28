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

/** Member name for object children, zero-based index for array elements. */
export type ScanKey = string | number

export interface ScanOptions {
  /** Project each child before it crosses: a sub-pointer yields the bare value;
   *  a map yields an object of those sub-values. */
  select?: string | Record<string, string>
  /** Yield arrays of up to `batch` items instead of one at a time.. */
  batch?: number
  /** Validate each yielded item against this schema (after `select`). */
  schema?: StandardSchemaV1
  /** Policy for items failing `schema`. Default `'throw'`; `'skip'` drops them, turning the schema into a filter. */
  onInvalid?: 'throw' | 'skip'
  /** Yield `[key, value]` tuples instead of bare values. Key is the member
   *  name for object children, or the zero-based index for array elements. */
  withKey?: boolean
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

  scan<S extends string>(pointer: PointerLiteral<S> | Pointer): AsyncIterable<unknown>
  scan<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    schema: Sch,
  ): AsyncIterable<InferOutput<Sch>>
  // withKey: true × (schema, batch) — must precede the non-withKey overloads
  // below so TS resolves the tuple-yielding signatures first.
  scan<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { withKey: true; schema: Sch; batch: number },
  ): AsyncIterable<[ScanKey, InferOutput<Sch>][]>
  scan<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { withKey: true; schema: Sch },
  ): AsyncIterable<[ScanKey, InferOutput<Sch>]>
  scan<S extends string>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { withKey: true; batch: number },
  ): AsyncIterable<[ScanKey, unknown][]>
  scan<S extends string>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { withKey: true },
  ): AsyncIterable<[ScanKey, unknown]>
  scan<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { schema: Sch; batch: number },
  ): AsyncIterable<InferOutput<Sch>[]>
  scan<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { schema: Sch },
  ): AsyncIterable<InferOutput<Sch>>
  scan<S extends string>(
    pointer: PointerLiteral<S> | Pointer,
    options: ScanOptions & { batch: number },
  ): AsyncIterable<unknown[]>
  scan<S extends string>(pointer: PointerLiteral<S> | Pointer, options: ScanOptions): AsyncIterable<unknown>

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

function normalizeScanArgs(arg?: StandardSchemaV1 | ScanOptions): {
  schema?: StandardSchemaV1
  select?: string | Record<string, string>
  batch?: number
  onInvalid?: 'throw' | 'skip'
  withKey?: boolean
} {
  if (!arg) return {}
  if ('~standard' in arg) return { schema: arg as StandardSchemaV1 }
  const options = arg as ScanOptions
  return {
    schema: options.schema,
    select: options.select,
    batch: options.batch,
    onInvalid: options.onInvalid,
    withKey: options.withKey,
  }
}

function serializeSelect(select: string | Record<string, string>): string {
  if (typeof select === 'string') return JSON.stringify({ one: select })
  const entries = Object.entries(select)
  if (entries.length === 0) {
    throw new RangeError('scan: select must have at least one field')
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
    scan(pointer: string, optionsOrSchema?: StandardSchemaV1 | ScanOptions): AsyncIterable<unknown> {
      const { schema, select, batch, onInvalid, withKey } = normalizeScanArgs(optionsOrSchema)
      if (batch !== undefined && (!Number.isInteger(batch) || batch <= 0)) {
        throw new RangeError(`scan: batch must be a positive integer, got ${batch}`)
      }
      const selectIr = select !== undefined ? serializeSelect(select) : undefined
      const hasArgs = selectIr !== undefined || batch !== undefined || withKey === true
      const inner = native.scan(pointer, hasArgs ? { selectIr, batch, withKey } : undefined)
      if (!schema) return inner
      const policy = onInvalid ?? 'throw'

      // The native side has already shaped each item: `value` when `!withKey`,
      // `[key, value]` when `withKey`. Schema validation only ever runs against
      // the value half; the key is passed through unchanged.
      if (batch === undefined) {
        return {
          async *[Symbol.asyncIterator]() {
            let i = 0
            for await (const v of inner) {
              const value = withKey ? (v as [ScanKey, unknown])[1] : v
              const result = await validateItem(schema, value, `${pointer}/${i++}`, policy)
              if ('skip' in result) continue
              yield withKey ? [(v as [ScanKey, unknown])[0], result.value] : result.value
            }
          },
        }
      }

      // Batched: each native yield is an array; validate (or skip) every
      // element. With `onInvalid: 'skip'` a batch may shrink or come back empty.
      return {
        async *[Symbol.asyncIterator]() {
          let i = 0
          for await (const b of inner) {
            const out: unknown[] = []
            for (const v of b as unknown[]) {
              const value = withKey ? (v as [ScanKey, unknown])[1] : v
              const result = await validateItem(schema, value, `${pointer}/${i++}`, policy)
              if ('skip' in result) continue
              out.push(withKey ? [(v as [ScanKey, unknown])[0], result.value] : result.value)
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
