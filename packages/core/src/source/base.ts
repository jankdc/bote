/**
 * The bytes a `read` resolves to, plus an end-of-stream flag. `eof` is `true`
 * iff this read reached the end of the underlying stream. A seekable source can
 * compute it from `size`; a forward source discovers it as the stream drains.
 */
export interface ReadResult {
  readonly data: Uint8Array;
  readonly eof: boolean;
}

/**
 * A handle on an opened byte stream. The reader owns whatever resources back the
 * stream (a file handle, a `fetch` body, an `AbortController`, etc.) and surfaces
 * them through `close()`. Constructed by `Source.open()`; never by callers directly.
 */
export interface Reader {
  /** Total length in bytes. Required for seekable sources; optional for forward ones. */
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
  /**
   * Declares the access model; lets `open()` enforce the right knobs at compile time.
   *   - `true`: `read(offset, length)` may be called at any offset, in any order.
   *     `size` is required. This random access is what lets the structural-index
   *     cache resume scans near a target.
   *   - `false`: a single forward pass. `read` is called with non-decreasing
   *     offsets; the cache is forced off. `size` may be omitted (the end is found
   *     via `eof`). Rewinding to an earlier offset re-acquires the stream (see
   *     `fromReadable`'s `rewind` option) or throws {@link ForwardReplayError}.
   */
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
