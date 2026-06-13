import { Readable } from 'node:stream';

import { ForwardReplayError } from '../error.ts';
import type { FactoryOptions, ForwardSource, Reader, ReadResult } from './base.ts';

/** Default chunk size, in bytes, for forward streams: a large pull keeps a streamed scan moving. */
const DEFAULT_STREAM_CHUNK_BYTES = 256 * 1024;

const EMPTY = new Uint8Array(0);

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

export interface HttpRequestOptions extends Omit<ReadableOptions, 'size'> {
  /** Merged into every `fetch` (headers, credentials, signal, etc.). */
  init?: RequestInit;
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
 * A forward-only source over an HTTP request (GET is default), streamed in a single pass. A
 * convenience wrapper around {@link fromReadable} whose producer re-fetches `url`
 * (reusing `init`, so auth headers, credentials, and an `AbortSignal` survive
 * each acquisition). For repeated or random access over HTTP, prefer the seekable
 * {@link fromHttpRange}.
 */
export function fromHttpRequest(url: string, options?: HttpRequestOptions): ForwardSource {
  const { init, ...readable } = options ?? {};
  const produce: ReadableProducer = async () => {
    const res = await fetch(url, { ...init });
    if (!res.ok) {
      throw new Error(`${url} failed: ${res.status} ${res.statusText}`);
    }
    if (!res.body) {
      throw new Error(`${url} returned no body`);
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
