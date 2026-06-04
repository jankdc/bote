import { open as openNative, type Cursor as NativeCursor } from '@botejs/native'

import { validatePath } from './path.ts'
import type { Source, SourceReader } from './sources.ts'
import { runStandardSchema, validateItem, type Path, type Segment, type StandardSchemaV1 } from './validate.ts'

import { splitArgs, serializeSelect, normalizeIterTail, type IterOptions, type VariadicPathArgs } from './args.ts'

type InferOutput<Sch> = Sch extends StandardSchemaV1<unknown, infer O> ? O : never

type SelectMapShape<S> = { -readonly [K in keyof S]: unknown }

/** Zero-based index of an array element. */
export type IterIndex = number
/** One `walk` step: the member's key paired with a cursor anchored at its value. */
export type WalkEntry = [key: string, cursor: Cursor]

export const DEFAULT_SOURCE_CHUNK_BYTES = 64 * 1024
export const DEFAULT_ITER_BATCH = 1000

export interface OpenOptions {
  /**
   * Slot budget for the structural-index cache: one slot per cached container
   * plus one per tabled object member. When a scan tips the cache over this
   * budget, the deepest (least navigationally useful) containers are evicted
   * first, LRU-tiebroken, keeping the shallow backbone that resumes future
   * scans. Bounds resident cache memory regardless of document size. `0`
   * disables the cache entirely. Omit for the native default (1024).
   */
  indexCacheEntries?: number
  /**
   * Max object members tabled per walked container in the structural-index
   * cache. The table is a dense prefix; past the cap, lookups of later members
   * resume-scan from the cap boundary. Lower trades cache memory for resume work
   * on pathologically large objects. `0` disables object member indexing. Omit
   * for the native default (unbounded).
   */
  objectMemberCap?: number
  /**
   * Element-index stride between sampled array members in the structural-index
   * cache. A later index resumes from the nearest array member at or before it, so
   * a smaller stride means denser array members (more memory, shorter resume
   * scans). `0` disables array-member indexing. Omit for the native default (16).
   *
   * Setting both `objectMemberCap` and `arrayIndexInterval` to `0` disables the
   * cache entirely (no source bytes are ever cached either way), as does
   * `indexCacheEntries: 0`.
   */
  arrayIndexInterval?: number
}

export interface Cursor {
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

  walk(...path: Segment[]): AsyncIterable<WalkEntry>
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
  const { indexCacheEntries, objectMemberCap, arrayIndexInterval } = options ?? {}
  for (const [name, value] of [
    ['indexCacheEntries', indexCacheEntries],
    ['objectMemberCap', objectMemberCap],
    ['arrayIndexInterval', arrayIndexInterval],
  ] as const) {
    if (value !== undefined && (!Number.isInteger(value) || value < 0)) {
      throw new RangeError(`open: ${name} must be a non-negative integer (0 disables), got ${value}`)
    }
  }
  const reader = await source.open()
  const chunkBytes = reader.chunkBytes ?? DEFAULT_SOURCE_CHUNK_BYTES
  let native: NativeCursor
  try {
    native = openNative({
      size: reader.size,
      chunkBytes,
      indexCacheEntries,
      objectMemberCap,
      arrayIndexInterval,
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
    walk(...path: Segment[]): AsyncIterable<WalkEntry> {
      validatePath(path)
      return {
        async *[Symbol.asyncIterator]() {
          for await (const [key, child] of native.walk(path)) {
            yield [key, wrap(child)]
          }
        },
      }
    },
  }

  return cursor as Cursor
}
