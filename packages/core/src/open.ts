import { open as openNative, type Cursor as NativeCursor } from '@botejs/native'

import { validatePath } from './path.ts'
import type { Source, SourceReader } from './sources.ts'

import {
  runStandardSchema,
  validateItem,
  formatPath,
  PathError,
  type Path,
  type PathFaultCode,
  type Segment,
  type StandardSchemaV1,
} from './validate.ts'

import {
  splitArgs,
  isSchema,
  serializeSelect,
  normalizeIterTail,
  type IterOptions,
  type VariadicPathArgs,
} from './args.ts'

type InferOutput<Sch> = Sch extends StandardSchemaV1<unknown, infer O> ? O : never

type SelectMapShape<S> = { -readonly [K in keyof S]: unknown }

/** Zero-based index of an array element. */
export type IterIndex = number
/** One `walk` step: the member's key paired with a cursor anchored at its value. */
export type WalkEntry = [key: string, cursor: Cursor]

export const DEFAULT_SOURCE_CHUNK_BYTES = 64 * 1024
export const DEFAULT_ITER_BATCH = 1000
export const MAX_ITER_BATCH = 1_000_000

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
  hop(...path: Segment[]): Promise<Cursor | null>

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
  const { indexCacheEntries, objectMemberCap, arrayIndexInterval } = options ?? {}
  for (const [name, value] of [
    ['indexCacheEntries', indexCacheEntries],
    ['objectMemberCap', objectMemberCap],
    ['arrayIndexInterval', arrayIndexInterval],
  ] as const) {
    if (value !== undefined && (!Number.isSafeInteger(value) || value < 0)) {
      throw new RangeError(`open: ${name} must be a non-negative integer (0 disables), got ${value}`)
    }
  }
  const reader = await source.open()
  const chunkBytes = reader.chunkBytes ?? DEFAULT_SOURCE_CHUNK_BYTES
  let native: NativeCursor
  try {
    if (!Number.isInteger(reader.size) || reader.size < 0) {
      throw new RangeError(`open: source size must be a non-negative integer, got ${reader.size}`)
    }
    if (!Number.isSafeInteger(chunkBytes) || chunkBytes <= 0) {
      throw new RangeError(`open: chunkBytes must be a positive integer, got ${chunkBytes}`)
    }
    if (chunkBytes % 64 !== 0) {
      throw new RangeError(`open: chunkBytes must be a multiple of 64, got ${chunkBytes}`)
    }
    native = openNative({
      size: reader.size,
      chunkBytes,
      indexCacheEntries,
      objectMemberCap,
      arrayIndexInterval,
      read: async ({ offset, length }: { offset: number; length: number }) => reader.read(offset, length),
    })
  } catch (err) {
    // Don't let a failing cleanup mask the original open error; attach it as cause.
    try {
      await closeReader(reader)
    } catch (closeErr) {
      if (err instanceof Error) (err as { cause?: unknown }).cause ??= closeErr
    }
    throw err
  }

  const state: CursorState = { closed: false }
  const close = async (): Promise<void> => {
    if (state.closed) return
    state.closed = true
    await closeReader(reader)
  }
  return Object.assign(wrap(native, state), {
    close,
    [Symbol.asyncDispose]: close,
  }) as RootCursor
}

async function closeReader(reader: SourceReader): Promise<void> {
  if (reader.close) await reader.close()
}

const NATIVE_PATH_ERROR = /^bote:path:([a-z_]+)(?::(\d+))?$/

function deserializeError(err: unknown, path: Path): unknown {
  if (err instanceof Error && !(err instanceof PathError)) {
    const match = NATIVE_PATH_ERROR.exec(err.message)
    if (match) {
      const segment = match[2] === undefined ? undefined : Number(match[2])
      return new PathError(path, match[1] as PathFaultCode, segment)
    }
  }
  return err
}

type CursorState = { closed: boolean }

/** Throw a uniform error for any operation on a closed cursor, so use-after-close
 *  is one defined contract regardless of source (some readers' reads keep working
 *  after close, others throw an opaque I/O error). */
function ensureOpen(state: CursorState): void {
  if (state.closed) throw new Error('bote: cursor is closed')
}

function wrap(native: NativeCursor, state: CursorState): Cursor {
  const cursor = {
    async hop(...path: Segment[]): Promise<Cursor | null> {
      ensureOpen(state)
      validatePath(path)
      let child: NativeCursor | null
      try {
        child = await native.hop(path)
      } catch (err) {
        throw deserializeError(err, path)
      }
      return child ? wrap(child, state) : null
    },
    async has(...args: VariadicPathArgs<StandardSchemaV1>): Promise<boolean> {
      ensureOpen(state)
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args)
      if (schema !== undefined && !isSchema(schema)) {
        throw new TypeError('has: expected a Standard Schema as the trailing argument')
      }
      if (!schema) return native.has(path)
      if (!(await native.has(path))) return false
      const text = await native.get(path)
      const value = text === undefined ? undefined : parseValue(text, path)
      const result = await validateItem(schema, value, path, 'skip')
      return !('skip' in result)
    },
    async get(...args: VariadicPathArgs<StandardSchemaV1>): Promise<unknown> {
      ensureOpen(state)
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args)
      if (schema !== undefined && !isSchema(schema)) {
        throw new TypeError('get: expected a Standard Schema as the trailing argument')
      }
      let value: unknown
      try {
        const text = await native.get(path)
        value = text === undefined ? undefined : parseValue(text, path)
      } catch (err) {
        throw deserializeError(err, path)
      }
      if (!schema) return value
      return runStandardSchema(schema, value, path)
    },
    async count(...path: Segment[]): Promise<number> {
      ensureOpen(state)
      validatePath(path)
      try {
        return await native.count(path)
      } catch (err) {
        throw deserializeError(err, path)
      }
    },
    iter(...args: VariadicPathArgs<StandardSchemaV1 | IterOptions>): AsyncIterable<unknown[]> {
      ensureOpen(state)
      const { path, tail } = splitArgs<StandardSchemaV1 | IterOptions>(args)
      const { schema, select, batch, onInvalid, withIndex } = normalizeIterTail(tail)
      if (batch !== undefined && (!Number.isInteger(batch) || batch <= 0 || batch > MAX_ITER_BATCH)) {
        throw new RangeError(`iter: batch must be an integer in 1..=${MAX_ITER_BATCH}, got ${batch}`)
      }
      if (withIndex !== undefined && typeof withIndex !== 'boolean') {
        throw new TypeError(`iter: withIndex must be a boolean, got ${typeof withIndex}`)
      }
      if (onInvalid !== undefined && onInvalid !== 'throw' && onInvalid !== 'skip') {
        throw new RangeError(`iter: onInvalid must be "throw" or "skip", got ${JSON.stringify(onInvalid)}`)
      }

      const resolvedBatch = batch ?? DEFAULT_ITER_BATCH
      const selectIr = select !== undefined ? serializeSelect(select) : undefined
      const inner = native.iter(path, { selectIr, batch: resolvedBatch })

      if (!schema) {
        return {
          async *[Symbol.asyncIterator]() {
            let i = 0
            try {
              for await (const b of inner) {
                const batch = parseValue(b, path) as unknown[]
                if (!withIndex) {
                  yield batch
                  continue
                }
                const out: unknown[] = new Array(batch.length)
                for (let j = 0; j < batch.length; j++) {
                  out[j] = [i++, batch[j]]
                }
                yield out
              }
            } catch (err) {
              throw deserializeError(err, path)
            }
          },
        } as AsyncIterable<unknown[]>
      }
      const policy = onInvalid ?? 'throw'
      return {
        async *[Symbol.asyncIterator]() {
          let i = 0
          try {
            for await (const b of inner) {
              const out: unknown[] = []
              for (const v of parseValue(b, path) as unknown[]) {
                const index = i++
                const result = await validateItem(schema, v, [...path, index], policy)
                if ('skip' in result) {
                  continue
                }

                out.push(withIndex ? [index, result.value] : result.value)
              }
              yield out
            }
          } catch (err) {
            throw deserializeError(err, path)
          }
        },
      }
    },
    walk(...path: Segment[]): AsyncIterable<WalkEntry> {
      ensureOpen(state)
      validatePath(path)
      return {
        async *[Symbol.asyncIterator]() {
          try {
            for await (const [key, child] of native.walk(path)) {
              yield [key, wrap(child, state)]
            }
          } catch (err) {
            throw deserializeError(err, path)
          }
        },
      }
    },
  }

  return cursor as Cursor
}

function parseValue(text: string, path: Path): unknown {
  try {
    return JSON.parse(text)
  } catch {
    throw new Error(`bote: malformed JSON value at ${formatPath(path)}`)
  }
}
