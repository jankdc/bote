import { open as openNative, type Cursor as NativeCursor } from '@botejs/native';

import { wrap, type CursorState, type RootCursor } from './cursor.ts';
import type { SeekableSource, SourceReader } from './sources.ts';

export const DEFAULT_SOURCE_CHUNK_BYTES = 64 * 1024;

export interface OpenOptions {
  /**
   * How much of the index that speeds up repeat lookups to keep in memory,
   * measured in entries. Higher means faster repeat queries but more memory;
   * lower means less memory but slower repeats. Set to `0` to turn the cache
   * off. Defaults to 1024.
   */
  indexCacheEntries?: number;
  /**
   * How many keys per object to index for fast lookup. Higher speeds up access
   * to keys later in large objects but uses more memory; lower saves memory at
   * the cost of slower lookups for those keys. Set to `0` to skip indexing
   * object keys. Defaults to unlimited.
   */
  objectMemberCap?: number;
  /**
   * How often to index array positions, e.g. every 16th element. Lower means
   * faster access to arbitrary array elements but more memory; higher saves
   * memory at the cost of slower access. Set to `0` to skip indexing array
   * positions. Defaults to 16.
   */
  arrayIndexInterval?: number;
}

/**
 * Open a cursor over a seekable source.
 *
 * The returned `RootCursor` owns the reader: `close()` (or `await using`)
 * drives the reader's own `close()` exactly once.
 */
export async function open(source: SeekableSource, options?: OpenOptions): Promise<RootCursor> {
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
      read: async ({ offset, length }: { offset: number; length: number }) => {
        const data = await reader.read(offset, length);
        return { data, eof: offset + data.byteLength >= reader.size };
      },
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
