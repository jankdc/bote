import { open as openNative, type Cursor as NativeCursor } from '@bote/native'

import type { JsonPointer } from './pointer.ts'
import type { Source, SourceReader } from './sources.ts'
import { runStandardSchema, type StandardSchemaV1 } from './validate.ts'

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

export interface Cursor {
  /** Object-member key or array-element index that this cursor was yielded under by `walk`. `null` on the root cursor. */
  readonly key: string | number | null

  has<S extends string>(pointer: JsonPointer<S>): Promise<boolean>
  has<S extends string>(pointer: JsonPointer<S>, schema: StandardSchemaV1): Promise<boolean>

  get<S extends string>(pointer: JsonPointer<S>): Promise<unknown>
  get<S extends string, Sch extends StandardSchemaV1>(
    pointer: JsonPointer<S>,
    schema: Sch,
  ): Promise<InferOutput<Sch>>

  iter<S extends string>(pointer: JsonPointer<S>): AsyncIterable<unknown>
  iter<S extends string, Sch extends StandardSchemaV1>(
    pointer: JsonPointer<S>,
    schema: Sch,
  ): AsyncIterable<InferOutput<Sch>>

  walk<S extends string>(pointer: JsonPointer<S>): AsyncIterable<Cursor>
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
        read: async ({ offset, buf }: { offset: number; buf: Uint8Array }) => reader.read(offset, buf),
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
    iter(pointer: string, schema?: StandardSchemaV1): AsyncIterable<unknown> {
      const inner = native.iter(pointer)
      if (!schema) return inner
      return {
        async *[Symbol.asyncIterator]() {
          let i = 0
          for await (const v of inner) {
            yield await runStandardSchema(schema, v, `${pointer}/${i++}`)
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
  }
  return cursor as Cursor
}
