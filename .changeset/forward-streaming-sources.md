---
'@botejs/native': minor
'@botejs/core': minor
---

Add forward-only streaming sources, so a document can be scanned as it arrives
instead of requiring a seekable backing store.

Two new factories produce a `ForwardSource`:

- `fromReadable(produce, options?)` - wraps a thunk that yields a Node or web
  `ReadableStream`. `produce` is called to (re)acquire the stream, with an
  optional `decode` transform (e.g. `s => s.pipeThrough(new DecompressionStream('gzip'))`).
- `fromHttpStream(url, options?)` - streams a URL's body without range requests;
  takes the same options plus a `fetch` `init`.

A source now declares `seekable`, and `open` is overloaded on it. Seekable
sources (`fromFile`/`fromBuffer`/`fromHttpRange`) keep the structural-index cache
and repeated, out-of-order access. A forward source is a single forward pass: the
cache is forced off, and its cache knobs are rejected both at compile time (via
the new `ForwardOpenOptions`) and at runtime. A source may now omit `size`; an
unknown-size stream discovers its end from an `eof` flag on each read.

A query that must re-read from an earlier offset on a forward source is governed
by `rewind` (default `'forbid'`):

- `'forbid'` - a single pass; a backward read throws `ForwardReplayError`.
- `'replay'` - re-acquire the stream from the start; only safe when the producer
  is idempotent. No extra resident memory.
- `'buffer'` - snapshot the stream into memory on first read for O(n) random
  access.

New error: `ForwardReplayError` (`code: 'forward_replay'`), carrying the
requested `offset` and the stream's current `position`. The native read contract
changes accordingly: `read(args)` now resolves to `{ data, eof }` (was a bare
`Uint8Array`), and `size` is optional.

New exports from `@botejs/core`: `fromReadable`, `fromHttpStream`,
`ForwardReplayError`, and the types `Source`, `Reader`, `ReadResult`,
`ForwardSource`, `ReadableProducer`, `ReadableOptions`, `HttpStreamOptions`, and
`ForwardOpenOptions`. The reader interface was renamed `SourceReader` ->
`Reader`.
