import { open as openNative, type CacheStats, type Cursor as NativeCursor } from '@botejs/native'

import type { PointerLiteral, Pointer } from './pointer.ts'
import { serializePredicate, type Predicate } from './predicate.ts'
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

export interface ScanOptions {
  /** Filter items natively here. */
  where?: Predicate
  /** Project each child before it crosses: a sub-pointer yields the bare value;
   *  a map yields an object of those sub-values. */
  select?: string | Record<string, string>
  /** Yield arrays of up to `batch` items instead of one at a time.. */
  batch?: number
  /** Validate each yielded item against this schema (after `select`). */
  schema?: StandardSchemaV1
  /** Policy for items failing `schema`. Default `'throw'`; `'skip'` drops them, turning the schema into a filter. */
  onInvalid?: 'throw' | 'skip'
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

  count<S extends string>(pointer: PointerLiteral<S> | Pointer, options?: { where?: Predicate }): Promise<number>

  scan<S extends string>(pointer: PointerLiteral<S> | Pointer): AsyncIterable<unknown>
  scan<S extends string, Sch extends StandardSchemaV1>(
    pointer: PointerLiteral<S> | Pointer,
    schema: Sch,
  ): AsyncIterable<InferOutput<Sch>>
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

  /** Stream child positions as cursors. With `where`, yields only matches - the filter runs natively, so a sparse descent crosses once per match. */
  walk<S extends string>(pointer: PointerLiteral<S> | Pointer, options?: { where?: Predicate }): AsyncIterable<Cursor>

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
  where?: Predicate
  select?: string | Record<string, string>
  batch?: number
  onInvalid?: 'throw' | 'skip'
} {
  if (!arg) return {}
  if ('~standard' in arg) return { schema: arg as StandardSchemaV1 }
  const options = arg as ScanOptions
  return {
    schema: options.schema,
    where: options.where,
    select: options.select,
    batch: options.batch,
    onInvalid: options.onInvalid,
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
    count(pointer: string, options?: { where?: Predicate }): Promise<number> {
      return native.count(pointer, options?.where ? serializePredicate(options.where) : undefined)
    },
    scan(pointer: string, optionsOrSchema?: StandardSchemaV1 | ScanOptions): AsyncIterable<unknown> {
      const { schema, where, select, batch, onInvalid } = normalizeScanArgs(optionsOrSchema)
      if (batch !== undefined && (!Number.isInteger(batch) || batch <= 0)) {
        throw new RangeError(`scan: batch must be a positive integer, got ${batch}`)
      }
      const whereIr = where ? serializePredicate(where) : undefined
      const selectIr = select !== undefined ? serializeSelect(select) : undefined
      const hasArgs = whereIr !== undefined || selectIr !== undefined || batch !== undefined
      const inner = native.scan(pointer, hasArgs ? { whereIr, selectIr, batch } : undefined)
      if (!schema) return inner
      const policy = onInvalid ?? 'throw'

      if (batch === undefined) {
        return {
          async *[Symbol.asyncIterator]() {
            let i = 0
            for await (const v of inner) {
              const result = await validateItem(schema, v, `${pointer}/${i++}`, policy)
              if ('skip' in result) continue
              yield result.value
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
              const result = await validateItem(schema, v, `${pointer}/${i++}`, policy)
              if ('skip' in result) continue
              out.push(result.value)
            }
            yield out
          }
        },
      }
    },
    walk(pointer: string, options?: { where?: Predicate }) {
      const whereIr = options?.where ? serializePredicate(options.where) : undefined
      return {
        async *[Symbol.asyncIterator]() {
          for await (const child of native.walk(pointer, whereIr)) {
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
