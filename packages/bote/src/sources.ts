import { open as fsOpen } from 'node:fs/promises'

/**
 * A handle on an opened seekable byte stream. The reader owns whatever
 * resources back the stream (a file handle, an `AbortController`, etc.) and
 * surfaces them through `close()`. Constructed by `Source.open()`; never by
 * library callers directly.
 */
export interface SourceReader {
  /** Total length of the underlying byte stream. */
  readonly size: number
  /** Preferred read granularity in bytes. Must be a non-zero multiple of 64. */
  readonly chunkBytes?: number
  /**
   * Fill `buf` with up to `buf.byteLength` bytes starting at `offset` and
   * resolve with the number of bytes written. The implementation must not
   * retain a reference to `buf` or read from it after the returned promise
   * resolves: `buf` is a view over native-owned memory whose lifetime ends
   * once the promise settles.
   */
  read(offset: number, buf: Uint8Array): Promise<number>
  /** Release resources held by the reader. Driven once by the `open()` lifecycle. */
  close?(): Promise<void> | void
}

/**
 * Describes how to obtain a seekable byte stream. Construction is cheap and
 * synchronous - no I/O happens until `open()` runs, which the top-level
 * `open()` API drives. Provide your own object implementing this interface to
 * plug in custom backends.
 */
export interface Source {
  /** Acquire the stream. Resolves to a `SourceReader` that owns any underlying resources. */
  open(): Promise<SourceReader>
}

export interface FactoryOptions {
  /** Override the factory's default chunk size. Must be a non-zero multiple of 64. */
  chunkBytes?: number
}

export interface HttpRangeOptions extends FactoryOptions {
  /** Merged into every request (headers, credentials, signal, etc.). */
  init?: RequestInit
}

/** Default chunk size, in bytes, for in-memory sources. */
const DEFAULT_BUFFER_CHUNK_BYTES = 4 * 1024

/** Default chunk size, in bytes, for local files: matches typical filesystem readahead. */
const DEFAULT_FILE_CHUNK_BYTES = 64 * 1024

/** Default chunk size, in bytes, for HTTP range reads: amortizes RTT across more data. */
const DEFAULT_URL_CHUNK_BYTES = 256 * 1024

export function fromBuffer(buf: Uint8Array | ArrayBuffer, options?: FactoryOptions): Source {
  const view = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
  const chunkBytes = options?.chunkBytes ?? DEFAULT_BUFFER_CHUNK_BYTES
  return {
    open: () =>
      Promise.resolve({
        size: view.byteLength,
        chunkBytes,
        read: async (offset, dst) => {
          const end = Math.min(offset + dst.byteLength, view.byteLength)
          const n = Math.max(0, end - offset)
          if (n > 0) dst.set(view.subarray(offset, end))
          return n
        },
      }),
  }
}

export function fromFile(path: string, options?: FactoryOptions): Source {
  const chunkBytes = options?.chunkBytes ?? DEFAULT_FILE_CHUNK_BYTES
  return {
    open: async () => {
      const handle = await fsOpen(path, 'r')
      const stat = await handle.stat()
      let closed = false
      return {
        size: stat.size,
        chunkBytes,
        read: async (offset, dst) => {
          const { bytesRead } = await handle.read(dst, 0, dst.byteLength, offset)
          return bytesRead
        },
        close: async () => {
          if (closed) return
          closed = true
          await handle.close()
        },
      }
    },
  }
}

export function fromHttpRange(url: string, options?: HttpRangeOptions): Source {
  const init = options?.init
  const chunkBytes = options?.chunkBytes ?? DEFAULT_URL_CHUNK_BYTES
  return {
    open: async () => {
      const controller = new AbortController()
      const userSignal = init?.signal
      if (userSignal) {
        if (userSignal.aborted) {
          controller.abort(userSignal.reason)
        } else {
          userSignal.addEventListener('abort', () => controller.abort(userSignal.reason), { once: true })
        }
      }
      const headHeaders = new Headers(init?.headers)
      headHeaders.set('Accept-Encoding', 'identity')
      const head = await fetch(url, { ...init, headers: headHeaders, method: 'HEAD', signal: controller.signal })
      if (!head.ok) {
        throw new Error(`HEAD ${url} failed: ${head.status} ${head.statusText}`)
      }
      const sizeHeader = head.headers.get('content-length')
      const size = sizeHeader === null ? NaN : Number.parseInt(sizeHeader, 10)
      if (!Number.isFinite(size) || size < 0) {
        throw new Error(`HEAD ${url} returned no valid Content-Length`)
      }
      const acceptsRanges = (head.headers.get('accept-ranges') ?? '').toLowerCase().includes('bytes')
      if (!acceptsRanges) {
        throw new Error(`HEAD ${url} does not advertise Accept-Ranges: bytes`)
      }
      let closed = false
      return {
        size,
        chunkBytes,
        read: async (offset, dst) => {
          // HTTP ranges are inclusive on both ends.
          const end = Math.min(offset + dst.byteLength, size) - 1
          const headers = new Headers(init?.headers)
          headers.set('Range', `bytes=${offset}-${end}`)
          headers.set('Accept-Encoding', 'identity')
          const res = await fetch(url, { ...init, headers, method: 'GET', signal: controller.signal })
          if (res.status === 206) {
            const body = new Uint8Array(await res.arrayBuffer())
            dst.set(body)
            return body.byteLength
          }
          // A 200 means the server ignored our Range request and returned the full
          // body. We throw here since the point of using ranges is to not have to
          // buffer the whole thing in memory.
          if (res.status === 200) {
            throw new Error(`Range GET ${url} (bytes=${offset}-${end}) ignored Range and returned 200.`)
          }

          throw new Error(`Range GET ${url} (bytes=${offset}-${end}) failed: ${res.status}`)
        },
        close: async () => {
          if (closed) return
          closed = true
          controller.abort()
        },
      }
    },
  }
}
