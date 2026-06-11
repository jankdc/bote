import { open as fsOpen } from 'node:fs/promises';

/**
 * A handle on an opened seekable byte stream. The reader owns whatever
 * resources back the stream (a file handle, an `AbortController`, etc.) and
 * surfaces them through `close()`. Constructed by `Source.open()`; never by
 * library callers directly.
 */
export interface SourceReader {
  /** Total length of the underlying byte stream. */
  readonly size: number;
  /** Preferred read granularity in bytes. Must be a non-zero multiple of 64. */
  readonly chunkBytes?: number;
  /**
   * Read up to `length` bytes starting at `offset` and resolve with the
   * bytes read. The returned `Uint8Array`'s `.byteLength` is the actual
   * count, which must be `<= length`.
   */
  read(offset: number, length: number): Promise<Uint8Array>;
  /** Release resources held by the reader. Driven once by the `open()` lifecycle. */
  close?(): Promise<void> | void;
}

/**
 * Describes how to obtain a seekable byte stream. Provide your own object implementing
 * this interface to plug in custom backends.
 */
export interface Source {
  /** Acquire the stream. Resolves to a `SourceReader` that owns any underlying resources. */
  open(): Promise<SourceReader>;
}

export interface FactoryOptions {
  /** Override the factory's default chunk size. Must be a non-zero multiple of 64. */
  chunkBytes?: number;
}

export interface HttpRangeOptions extends FactoryOptions {
  /** Merged into every request (headers, credentials, signal, etc.). */
  init?: RequestInit;
}

/** Default chunk size, in bytes, for in-memory sources. */
const DEFAULT_BUFFER_CHUNK_BYTES = 4 * 1024;

/** Default chunk size, in bytes, for local files: matches typical filesystem readahead. */
const DEFAULT_FILE_CHUNK_BYTES = 64 * 1024;

/** Default chunk size, in bytes, for HTTP range reads: amortizes RTT across more data. */
const DEFAULT_URL_CHUNK_BYTES = 256 * 1024;

export function fromBuffer(buf: Uint8Array | ArrayBuffer, options?: FactoryOptions): Source {
  const view = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  const chunkBytes = options?.chunkBytes ?? DEFAULT_BUFFER_CHUNK_BYTES;
  return {
    open: () =>
      Promise.resolve({
        size: view.byteLength,
        chunkBytes,
        read: (offset, length) => Promise.resolve(view.subarray(offset, Math.min(offset + length, view.byteLength))),
      }),
  };
}

export function fromFile(path: string, options?: FactoryOptions): Source {
  const chunkBytes = options?.chunkBytes ?? DEFAULT_FILE_CHUNK_BYTES;
  return {
    open: async () => {
      const handle = await fsOpen(path, 'r');
      const stat = await handle.stat();
      let closed = false;
      return {
        size: stat.size,
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
          return buf.subarray(0, filled);
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

export function fromHttpRange(url: string, options?: HttpRangeOptions): Source {
  const init = options?.init;
  const chunkBytes = options?.chunkBytes ?? DEFAULT_URL_CHUNK_BYTES;
  return {
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
            return new Uint8Array(await res.arrayBuffer());
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
