import { open as fsOpen } from 'node:fs/promises';

import type { FactoryOptions, SeekableSource } from './base.ts';

/** Default chunk size, in bytes, for in-memory sources. */
const DEFAULT_BUFFER_CHUNK_BYTES = 4 * 1024;

/** Default chunk size, in bytes, for local files: matches typical filesystem readahead. */
const DEFAULT_FILE_CHUNK_BYTES = 64 * 1024;

/** Default chunk size, in bytes, for HTTP range reads: amortizes RTT across more data. */
const DEFAULT_URL_CHUNK_BYTES = 256 * 1024;

export interface HttpRangeOptions extends FactoryOptions {
  /** Merged into every request (headers, credentials, signal, etc.). */
  init?: RequestInit;
}

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
