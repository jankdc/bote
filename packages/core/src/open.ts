import { open as openNative, type Cursor as NativeCursor } from '@botejs/native'

import { validatePath } from './path.ts'
import type { Source, SourceReader } from './sources.ts'
import { runStandardSchema, validateItem, type Path, type Segment, type StandardSchemaV1 } from './validate.ts'

import { splitArgs, serializeSelect, normalizeIterTail, type IterOptions, type VariadicPathArgs } from './args.ts'

type InferOutput<Sch> = Sch extends StandardSchemaV1<unknown, infer O> ? O : never

type SelectMapShape<S> = { -readonly [K in keyof S]: unknown }

/** Zero-based index of an array element. */
export type IterIndex = number

export const DEFAULT_SOURCE_CHUNK_BYTES = 64 * 1024
export const DEFAULT_ITER_BATCH = 1000

export interface OpenOptions {
  /**
   * Capacity of the structural-index cache, in slots: one slot per cached
   * container plus one per tabled object member. The cache restores cross-query
   * warmth - a later query that lands in an already-walked container starts its
   * scan near the target - and caches no source bytes, so the resident-memory
   * bound is untouched. `0` disables it entirely. Omit to use the native
   * default (1024).
   */
  indexCacheEntries?: number
}

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
  walk(...path: Segment[]): AsyncIterable<Cursor>
}

export interface RootCursor extends Cursor, AsyncDisposable {
  /** Close the underlying source. Idempotent. */
  close(): Promise<void>
}

/**
 * Open a cursor over a seekable source.
 *
 * The returned `RootCursor` owns the reader: `close()` (or `await using`)
 * drives the reader's own `close()` exactly once.
 */
export async function open(source: Source, options?: OpenOptions): Promise<RootCursor> {
  const indexCacheEntries = options?.indexCacheEntries
  if (indexCacheEntries !== undefined && (!Number.isInteger(indexCacheEntries) || indexCacheEntries < 0)) {
    throw new RangeError(
      `open: indexCacheEntries must be a non-negative integer (0 disables), got ${indexCacheEntries}`,
    )
  }
  const reader = await source.open()
  const chunkBytes = reader.chunkBytes ?? DEFAULT_SOURCE_CHUNK_BYTES
  let native: NativeCursor
  try {
    native = openNative({
      size: reader.size,
      chunkBytes,
      indexCacheEntries,
      read: async ({ offset, length }: { offset: number; length: number }) => reader.read(offset, length),
    })
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
  }

  return cursor as Cursor
}
