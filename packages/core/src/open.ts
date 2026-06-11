import { open as openNative, type Cursor as NativeCursor } from '@botejs/native';

import { wrap, type CursorState, type RootCursor } from './cursor.ts';
import type { Source, SourceReader } from './sources.ts';

export const DEFAULT_SOURCE_CHUNK_BYTES = 64 * 1024;

export interface OpenOptions {
  /**
   * Slot budget for the structural-index cache: one slot per cached container
   * plus one per tabled object member. When a scan tips the cache over this
   * budget, the deepest (least navigationally useful) containers are evicted
   * first, LRU-tiebroken, keeping the shallow backbone that resumes future
   * scans. Bounds resident cache memory regardless of document size. `0`
   * disables the cache entirely. Omit for the native default (1024).
   */
  indexCacheEntries?: number;
  /**
   * Max object members tabled per walked container in the structural-index
   * cache. The table is a dense prefix; past the cap, lookups of later members
   * resume-scan from the cap boundary. Lower trades cache memory for resume work
   * on pathologically large objects. `0` disables object member indexing. Omit
   * for the native default (unbounded).
   */
  objectMemberCap?: number;
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
  arrayIndexInterval?: number;
}

/**
 * Open a cursor over a seekable source.
 *
 * The returned `RootCursor` owns the reader: `close()` (or `await using`)
 * drives the reader's own `close()` exactly once.
 */
export async function open(source: Source, options?: OpenOptions): Promise<RootCursor> {
  const { indexCacheEntries, objectMemberCap, arrayIndexInterval } = options ?? {};
  for (const [name, value] of [
    ['indexCacheEntries', indexCacheEntries],
    ['objectMemberCap', objectMemberCap],
    ['arrayIndexInterval', arrayIndexInterval],
  ] as const) {
    if (value !== undefined && (!Number.isSafeInteger(value) || value < 0)) {
      throw new RangeError(`open: ${name} must be a non-negative integer (0 disables), got ${value}`);
    }
  }
  const reader = await source.open();
  const chunkBytes = reader.chunkBytes ?? DEFAULT_SOURCE_CHUNK_BYTES;
  let native: NativeCursor;
  try {
    if (!Number.isInteger(reader.size) || reader.size < 0) {
      throw new RangeError(`open: source size must be a non-negative integer, got ${reader.size}`);
    }
    if (!Number.isSafeInteger(chunkBytes) || chunkBytes <= 0) {
      throw new RangeError(`open: chunkBytes must be a positive integer, got ${chunkBytes}`);
    }
    if (chunkBytes % 64 !== 0) {
      throw new RangeError(`open: chunkBytes must be a multiple of 64, got ${chunkBytes}`);
    }
    native = openNative({
      size: reader.size,
      chunkBytes,
      indexCacheEntries,
      objectMemberCap,
      arrayIndexInterval,
      read: async ({ offset, length }: { offset: number; length: number }) => reader.read(offset, length),
    });
  } catch (err) {
    // Don't let a failing cleanup mask the original open error; attach it as cause.
    try {
      await closeReader(reader);
    } catch (closeErr) {
      if (err instanceof Error) {
        (err as { cause?: unknown }).cause ??= closeErr;
      }
    }
    throw err;
  }

  const state: CursorState = { closed: false };
  const close = async (): Promise<void> => {
    if (state.closed) {
      return;
    }
    state.closed = true;
    await closeReader(reader);
  };
  return Object.assign(wrap(native, state), {
    close,
    [Symbol.asyncDispose]: close,
  }) as RootCursor;
}

async function closeReader(reader: SourceReader): Promise<void> {
  if (reader.close) {
    await reader.close();
  }
}
