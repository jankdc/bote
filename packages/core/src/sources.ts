import { open as fsOpen } from 'node:fs/promises';
import { Readable } from 'node:stream';

import { ForwardReplayError } from './error.ts';

/**
 * The bytes a `read` resolves to, plus an end-of-stream flag. `eof` is `true`
 * iff this read reached the end of the underlying stream. A seekable reader can
 * compute it from `size`; a forward reader discovers it as the stream drains.
 */
export interface ReadResult {
  readonly data: Uint8Array;
  readonly eof: boolean;
}

/**
 * A handle on an opened byte stream. The reader owns whatever resources back the
 * stream (a file handle, a `fetch` body, an `AbortController`, etc.) and surfaces
 * them through `close()`. Constructed by `Source.open()`; never by callers directly.
 *
 * `seekable` declares the access model:
 *   - `true`: `read(offset, length)` may be called at any offset, in any order.
 *     `size` is required. This random access is what lets the structural-index
 *     cache resume scans near a target.
 *   - `false`: a single forward pass. `read` is called with non-decreasing
 *     offsets; the cache is forced off. `size` may be omitted (the end is found
 *     via `eof`). Rewinding to an earlier offset re-acquires the stream (see
 *     `fromReadable`'s `rewind` option) or throws {@link ForwardReplayError}.
 */
export interface Reader {
  /** Whether reads may target any offset/order (`true`) or a single forward pass (`false`). */
  readonly seekable: boolean;
  /** Total length in bytes. Required for seekable readers; optional for forward ones. */
  readonly size?: number;
  /** Preferred read granularity in bytes. Must be a non-zero multiple of 64. */
  readonly chunkBytes?: number;
  /**
   * Read up to `length` bytes starting at `offset`. `data.byteLength` is the
   * actual count read (`<= length`); `eof` is `true` iff the read reached the
   * end of the stream.
   */
  read(offset: number, length: number): Promise<ReadResult>;
  /** Release resources held by the reader. Driven once by the `open()` lifecycle. */
  close?(): Promise<void> | void;
}

/**
 * Describes how to obtain a byte stream. `open()` is called once per cursor for
 * a seekable source; a forward source's reader re-acquires its stream on demand.
 * Provide your own object to plug in a custom backend.
 */
export interface Source {
  /** Mirrors {@link Reader.seekable}; lets `open()` enforce the right knobs at compile time. */
  readonly seekable: boolean;
  /** Acquire the stream. Resolves to a `Reader` that owns any underlying resources. */
  open(): Promise<Reader>;
}

/**
 * A {@link Source} statically known to be seekable (random access, cache-eligible).
 * `open()` discriminates on this, so a custom backend must brand its `seekable`
 * as the literal `true` (annotate the object `SeekableSource`) to be accepted.
 */
export type SeekableSource = Source & { readonly seekable: true };

/**
 * A {@link Source} statically known to be forward-only (single pass, cache forced
 * off). Brand a custom backend's object `ForwardSource` so `open()` accepts it and
 * rejects cache knobs at compile time.
 */
export type ForwardSource = Source & { readonly seekable: false };

export interface FactoryOptions {
  /** Override the factory's default chunk size. Must be a non-zero multiple of 64. */
  chunkBytes?: number;
}

export interface HttpRangeOptions extends FactoryOptions {
  /** Merged into every request (headers, credentials, signal, etc.). */
  init?: RequestInit;
}

/**
 * A function that produces a fresh readable stream each time it is called. A
 * forward reader invokes it once up front, and again on every re-acquisition, so
 * it is also the seam for per-acquisition customization (a freshly minted auth
 * token, a new `AbortSignal`, etc.).
 */
export type ReadableProducer = () =>
  | NodeJS.ReadableStream
  | ReadableStream<Uint8Array>
  | Promise<NodeJS.ReadableStream | ReadableStream<Uint8Array>>;

export interface ReadableOptions extends FactoryOptions {
  /** Known total length, if any. Forwarded to the engine so it can skip rediscovering the end. */
  size?: number;
  /** Transform applied to every (re)acquired stream, e.g. `s => s.pipeThrough(new DecompressionStream('gzip'))`. */
  decode?: (raw: ReadableStream<Uint8Array>) => ReadableStream<Uint8Array>;
  /**
   * What a later query that must re-read from an earlier offset does. Defaults
   * to `'forbid'`. The three settings trade resident memory for re-read ability:
   *   - `'forbid'`: a single forward pass; a rewind throws {@link ForwardReplayError}.
   *   - `'replay'`: re-acquire the stream from the start. Only safe when the
   *     producer is idempotent (yields the same bytes each call). No extra memory.
   *   - `'buffer'`: snapshot the whole stream into memory on first read, enabling
   *     random access at O(n) resident memory.
   */
  rewind?: 'forbid' | 'replay' | 'buffer';
}

export interface HttpStreamOptions extends Omit<ReadableOptions, 'size'> {
  /** Merged into every `fetch` (headers, credentials, signal, etc.). */
  init?: RequestInit;
}

/** Default chunk size, in bytes, for in-memory sources. */
const DEFAULT_BUFFER_CHUNK_BYTES = 4 * 1024;

/** Default chunk size, in bytes, for local files: matches typical filesystem readahead. */
const DEFAULT_FILE_CHUNK_BYTES = 64 * 1024;

/** Default chunk size, in bytes, for HTTP range reads: amortizes RTT across more data. */
const DEFAULT_URL_CHUNK_BYTES = 256 * 1024;

/** Default chunk size, in bytes, for forward streams: a large pull keeps a streamed scan moving. */
const DEFAULT_STREAM_CHUNK_BYTES = 256 * 1024;

const EMPTY = new Uint8Array(0);

export function fromBuffer(buf: Uint8Array | ArrayBuffer, options?: FactoryOptions): SeekableSource {
  const view = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  const chunkBytes = options?.chunkBytes ?? DEFAULT_BUFFER_CHUNK_BYTES;
  return {
    seekable: true,
    open: () =>
      Promise.resolve({
        seekable: true,
        size: view.byteLength,
        chunkBytes,
        read: (offset, length) => {
          const data = view.subarray(offset, Math.min(offset + length, view.byteLength));
          return Promise.resolve({ data, eof: offset + data.byteLength >= view.byteLength });
        },
      }),
  };
}

export function fromFile(path: string, options?: FactoryOptions): SeekableSource {
  const chunkBytes = options?.chunkBytes ?? DEFAULT_FILE_CHUNK_BYTES;
  return {
    seekable: true,
    open: async () => {
      const handle = await fsOpen(path, 'r');
      const stat = await handle.stat();
      const size = stat.size;
      let closed = false;
      return {
        seekable: true,
        size,
        chunkBytes,
        read: async (offset, length) => {
          const buf = Buffer.allocUnsafe(length);
          let filled = 0;
          while (filled < length) {
            const { bytesRead } = await handle.read(buf, filled, length - filled, offset + filled);
            if (bytesRead === 0) {
              break;
            }
            filled += bytesRead;
          }
          return { data: buf.subarray(0, filled), eof: offset + filled >= size };
        },
        close: async () => {
          if (closed) {
            return;
          }
          closed = true;
          await handle.close();
        },
      };
    },
  };
}

export function fromHttpRange(url: string, options?: HttpRangeOptions): SeekableSource {
  const init = options?.init;
  const chunkBytes = options?.chunkBytes ?? DEFAULT_URL_CHUNK_BYTES;
  return {
    seekable: true,
    open: async () => {
      const controller = new AbortController();
      const userSignal = init?.signal;
      if (userSignal) {
        if (userSignal.aborted) {
          controller.abort(userSignal.reason);
        } else {
          userSignal.addEventListener('abort', () => controller.abort(userSignal.reason), { once: true });
        }
      }
      const headHeaders = new Headers(init?.headers);
      headHeaders.set('Accept-Encoding', 'identity');
      const head = await fetch(url, { ...init, headers: headHeaders, method: 'HEAD', signal: controller.signal });
      if (!head.ok) {
        throw new Error(`HEAD ${url} failed: ${head.status} ${head.statusText}`);
      }
      const sizeHeader = head.headers.get('content-length');
      const size = sizeHeader === null ? NaN : Number.parseInt(sizeHeader, 10);
      if (!Number.isFinite(size) || size < 0) {
        throw new Error(`HEAD ${url} returned no valid Content-Length`);
      }
      const acceptsRanges = (head.headers.get('accept-ranges') ?? '').toLowerCase().includes('bytes');
      if (!acceptsRanges) {
        throw new Error(`HEAD ${url} does not advertise Accept-Ranges: bytes`);
      }
      let closed = false;
      return {
        seekable: true,
        size,
        chunkBytes,
        read: async (offset, length) => {
          // HTTP ranges are inclusive on both ends.
          const end = Math.min(offset + length, size) - 1;
          const headers = new Headers(init?.headers);
          headers.set('Range', `bytes=${offset}-${end}`);
          headers.set('Accept-Encoding', 'identity');
          const res = await fetch(url, { ...init, headers, method: 'GET', signal: controller.signal });
          if (res.status === 206) {
            const data = new Uint8Array(await res.arrayBuffer());
            return { data, eof: offset + data.byteLength >= size };
          }
          // A 200 means the server ignored our Range request and returned the full
          // body. We throw here since the point of using ranges is to not have to
          // buffer the whole thing in memory.
          if (res.status === 200) {
            throw new Error(`Range GET ${url} (bytes=${offset}-${end}) ignored Range and returned 200.`);
          }

          throw new Error(`Range GET ${url} (bytes=${offset}-${end}) failed: ${res.status}`);
        },
        close: async () => {
          if (closed) {
            return;
          }
          closed = true;
          controller.abort();
        },
      };
    },
  };
}

/**
 * A forward-only source backed by a re-openable readable stream. `produce` is
 * called to acquire each pass: once up front, and again on a rewind when
 * `rewind: 'replay'` is set. A plain `Readable` instance cannot be re-streamed,
 * so pass a thunk (`() => createReadStream(path)`), not a live stream.
 *
 * Because every cursor operation is an independent scan from the start, a single
 * forward pass serves exactly one query; a second query (or `hop` then `get`)
 * rewinds. By default that throws {@link ForwardReplayError}; opt into
 * `rewind: 'replay'` (idempotent producer) or `rewind: 'buffer'` (in-memory
 * snapshot) for multi-query access.
 */
export function fromReadable(produce: ReadableProducer, options?: ReadableOptions): ForwardSource {
  const chunkBytes = options?.chunkBytes ?? DEFAULT_STREAM_CHUNK_BYTES;
  const size = options?.size;
  const decode = options?.decode;
  const rewind = options?.rewind ?? 'forbid';
  return {
    seekable: false,
    open: () => makeForwardReader(produce, { chunkBytes, size, decode, rewind }),
  };
}

/**
 * A forward-only source over an HTTP GET body, streamed in a single pass. A
 * convenience wrapper around {@link fromReadable} whose producer re-fetches `url`
 * (reusing `init`, so auth headers, credentials, and an `AbortSignal` survive
 * each acquisition). For repeated or random access over HTTP, prefer the seekable
 * {@link fromHttpRange}.
 */
export function fromHttpStream(url: string, options?: HttpStreamOptions): ForwardSource {
  const { init, ...readable } = options ?? {};
  const produce: ReadableProducer = async () => {
    const res = await fetch(url, { ...init, method: 'GET' });
    if (!res.ok) {
      throw new Error(`GET ${url} failed: ${res.status} ${res.statusText}`);
    }
    if (!res.body) {
      throw new Error(`GET ${url} returned no body`);
    }
    return res.body;
  };
  return fromReadable(produce, readable);
}

interface ForwardConfig {
  chunkBytes: number;
  size?: number;
  decode?: (raw: ReadableStream<Uint8Array>) => ReadableStream<Uint8Array>;
  rewind: 'forbid' | 'replay' | 'buffer';
}

async function makeForwardReader(produce: ReadableProducer, config: ForwardConfig): Promise<Reader> {
  const { chunkBytes, size, decode, rewind } = config;
  const replay = rewind === 'replay';
  const buffer = rewind === 'buffer';

  let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  let pos = 0; // bytes consumed from the active pass (served + skipped)
  let leftover: Uint8Array | null = null; // tail of the last pulled chunk, not yet served
  let done = false;
  let snapshot: Uint8Array | null = null;
  let chain: Promise<unknown> = Promise.resolve();

  const acquire = async (): Promise<void> => {
    let web = toWebStream(await produce());
    if (decode) {
      web = decode(web);
    }
    reader = web.getReader();
    pos = 0;
    leftover = null;
    done = false;
  };

  const release = async (): Promise<void> => {
    const active = reader;
    reader = null;
    if (active) {
      try {
        await active.cancel();
      } catch {
        // cancelling an already-errored/closed stream is not actionable
      }
    }
  };

  // Next run of bytes available from the active pass, or null once it is drained.
  const pull = async (): Promise<Uint8Array | null> => {
    if (leftover) {
      const chunk = leftover;
      leftover = null;
      return chunk;
    }
    if (done || !reader) {
      return null;
    }
    const { value, done: streamDone } = await reader.read();
    if (streamDone) {
      done = true;
      return null;
    }
    return value;
  };

  const readForward = async (offset: number, length: number): Promise<ReadResult> => {
    if (offset < pos) {
      if (!replay) {
        throw new ForwardReplayError(offset, pos);
      }
      await release();
      await acquire();
    }
    if (!reader && !done) {
      await acquire();
    }
    while (pos < offset) {
      const chunk = await pull();
      if (chunk === null) {
        break; // stream ended before the requested offset; the read returns eof
      }
      const skip = offset - pos;
      if (chunk.byteLength <= skip) {
        pos += chunk.byteLength;
      } else {
        leftover = chunk.subarray(skip);
        pos += skip;
      }
    }
    const parts: Uint8Array[] = [];
    let got = 0;
    while (got < length) {
      const chunk = await pull();
      if (chunk === null) {
        break;
      }
      if (chunk.byteLength === 0) {
        continue;
      }
      const take = Math.min(chunk.byteLength, length - got);
      parts.push(take === chunk.byteLength ? chunk : chunk.subarray(0, take));
      if (take < chunk.byteLength) {
        leftover = chunk.subarray(take);
      }
      pos += take;
      got += take;
    }
    return { data: concat(parts, got), eof: done && leftover === null };
  };

  const readSnapshot = (offset: number, length: number): ReadResult => {
    const data = snapshot as Uint8Array;
    if (offset >= data.byteLength) {
      return { data: EMPTY, eof: true };
    }
    const end = Math.min(offset + length, data.byteLength);
    return { data: data.subarray(offset, end), eof: end >= data.byteLength };
  };

  // A forward pass has shared position state, so overlapping scans (e.g. two
  // un-awaited queries) must not interleave their pulls. Serializing reads keeps
  // bookkeeping intact and turns concurrent misuse into a ForwardReplayError.
  const read = (offset: number, length: number): Promise<ReadResult> => {
    const result = chain.then(async () => {
      if (buffer) {
        if (!snapshot) {
          snapshot = await drainAll(produce, decode);
        }
        return readSnapshot(offset, length);
      }
      return readForward(offset, length);
    });
    chain = result.catch(() => {});
    return result;
  };

  if (!buffer) {
    await acquire();
  }

  return { seekable: false, size, chunkBytes, read, close: release };
}

async function drainAll(
  produce: ReadableProducer,
  decode?: (raw: ReadableStream<Uint8Array>) => ReadableStream<Uint8Array>,
): Promise<Uint8Array> {
  let web = toWebStream(await produce());
  if (decode) {
    web = decode(web);
  }
  const reader = web.getReader();
  const parts: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) {
        break;
      }
      if (value && value.byteLength > 0) {
        parts.push(value);
        total += value.byteLength;
      }
    }
  } finally {
    try {
      await reader.cancel();
    } catch {
      // already drained/errored
    }
  }
  return concat(parts, total);
}

function toWebStream(stream: NodeJS.ReadableStream | ReadableStream<Uint8Array>): ReadableStream<Uint8Array> {
  if (typeof (stream as ReadableStream<Uint8Array>).getReader === 'function') {
    return stream as ReadableStream<Uint8Array>;
  }
  return Readable.toWeb(stream as Readable) as unknown as ReadableStream<Uint8Array>;
}

function concat(parts: Uint8Array[], total: number): Uint8Array {
  if (parts.length === 1 && parts[0].byteLength === total) {
    return parts[0];
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.byteLength;
  }
  return out;
}
