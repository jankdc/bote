# @botejs/core

## 0.8.0

### Minor Changes

- 272420c: Breaking: Had to adjust OpenOptions to differentiate between
  seekable options and forward-only options.

## 0.7.0

### Minor Changes

- cb9487d: Remove the `.count` API (breaking).

  Counting the members of a container is rare enough that it doesn't warrant a
  dedicated native scan, and it composes from the existing streaming API:

  ```js
  let n = 0;
  for await (const _ of cursor.iter('items')) n++;
  ```

  This also drops the now-unused `child_count` field from the structural-index
  cache, which only ever served repeat `count` calls.

### Patch Changes

- Updated dependencies [cb9487d]
  - @botejs/native@0.7.0

## 0.6.0

### Minor Changes

- 30fab5b: Consolidate every error bote raises under one `BoteError` base class.

  `BoteError` (exported from `@botejs/core`) is the abstract base for everything bote throws from its own logic. Catch it to catch anything bote raises, then branch on the new `code` field (`BoteErrorCode`) for the precise kind. Every message stays `bote:`-prefixed.

  The concrete subclasses are now all exported and all carry `code`:
  - `PathError` (`code`: `PathFaultCode`) - a path that contradicts the document's shape; carries `path`.
  - `ValidationError` (`code`: `'validation'`) - schema validation failure; carries `issues` and `path`.
  - `MalformedJsonError` (`code`: `JsonFaultCode`) - a malformed value, now distinguishing `unexpected_eof` from `malformed_json`; carries `path`. Was a plain `Error`.
  - `SourceReadError` (`code`: `'source_io'`) - a failing source `read()`; carries `path`. Was a plain `Error`.
  - `ClosedCursorError` (`code`: `'closed'`) - any call on a closed cursor. Was a plain `Error`.

  New exported native fault-code types: `BoteErrorCode`, `JsonFaultCode`, `SourceFaultCode` (alongside the existing `PathFaultCode`). The native addon now emits typed `bote:<code>[:<detail>]` fault lines that the facade rebuilds into the typed errors above, so the human-readable messages live entirely on the JS side.

- 717f41b: ## Breaking Changes
  - Rename `fromHttpStream` to `fromHttpRequest` and its options type `HttpStreamOptions` to `HttpRequestOptions`. The request method is no longer pinned to GET (it remains the default), so callers can override it via `init.method`.

- beb3d3c: Add forward-only streaming sources, so a document can be scanned as it arrives
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

### Patch Changes

- Updated dependencies [305fd88]
- Updated dependencies [30fab5b]
- Updated dependencies [beb3d3c]
  - @botejs/native@0.6.0

## 0.5.0

### Minor Changes

- 6e85990: \***\*BREAKING:\*\*** the raw-batch escape hatch is renamed from `batches()` to
  `raw()`. Migrate by renaming the call:
  `cursor.iter(...).batches()` to `cursor.iter(...).raw()`.

  Add chainable helpers to the `iter` stream.

  `cursor.iter(...)` now returns an `IterStream<T>` with lazy operators and
  eager terminals, so common item-processing no longer needs a hand-written
  `for await` loop.
  - **Lazy operators** (return a new `IterStream`, nothing runs until iterated or
    a terminal is awaited): `map`, `filter` (with type-guard narrowing), `take`,
    `drop`. Each callback receives a zero-based item index; `map`/`filter` await
    async callbacks. `take` releases the native scan once its limit is reached.
  - **Terminals** (await the walk): `toArray`, `forEach`, `reduce`, `find`,
    `some`, `every`. `find`/`some` short-circuit on the first match.

- bf63de8: `iter` yields items by default; `batches()` is the new escape hatch.

  `cursor.iter(...)` is now a stream **of items**: `for await (const item of cursor.iter(...))`
  yields one value at a time (a `[key, value]` tuple with `withKey: true`). Raw
  batch access moves behind an explicit `batches()`. The main difference between this method and
  the original individual yield item is that this one materialises the batch of items behind the
  scenes and yield it individually, amortizing the cost of the FFI rust-js passing whilst still
  being performant.
  - **Breaking:** the default iterator's yield type changes from `T[]` to `T`.
    Migrate a batch loop by appending `.batches()`:
    `for await (const batch of cursor.iter(...))` -> `for await (const batch of cursor.iter(...).batches())`.
    TypeScript flags every site that treated a yield as an array.
  - Per-item iteration costs a flat ~10% over a full walk; every hot path keeps a
    zero-tax alternative (`batches()` and the `collect`/`forEach`/`reduce`
    terminals
  - `break`ing out of the item loop releases the underlying native scan.

- dc4fc1d: Unify iteration on `iter`; remove `walk`.

  `iter` is now kind-agnostic: an object target yields its member values in
  document order (arrays are unchanged). The `walk` verb and its
  `iter_on_object` / `walk_on_array` path faults are gone.
  - **Breaking:** `walk(path)` is removed. Iterate object members with
    `iter(path)`; for `[key, value]` pairs use `iter(path, { withKey: true })`.
    To descend lazily, pair `withKey` (with a small `select` to learn the keys
    cheaply) with `hop(key)`.
  - **Breaking:** the `iter` option `withIndex` is renamed to `withKey`. The key
    is the member name for objects and the element index for arrays; the exported
    `IterKey` type widens from `number` to `string | number`.

  Duplicate object keys are preserved by tuple yields (one `[key, value]` per
  occurrence), unlike `JSON.parse`, which keeps the last.

### Patch Changes

- Updated dependencies [dc4fc1d]
  - @botejs/native@0.5.0

## 0.4.0

### Minor Changes

- e0c03ad: Materialize values via raw JSON text + `JSON.parse` instead of a `serde_json::Value` round-trip. `get` and `iter` previously parsed a value into a `serde_json::Value` in Rust, which napi then re-walked into JS one property/element at a time (an FFI crossing each). The native layer now hands the value's raw JSON bytes across the boundary and the facade `JSON.parse`s it in a single pass.

  Two behavior changes follow from parsing with `JSON.parse`:
  - Integers beyond 2^53 now deserialize to a JS `Number` (matching native `JSON.parse`) rather than being promoted to a lossless `BigInt`.
  - A malformed value at the resolved path now surfaces as a uniform bote error (`malformed JSON value at <path>`) raised by the facade, rather than a Rust-originated parse error.

### Patch Changes

- ce2b760: Harden error handling: a synchronous throw inside the JS `read` fn now surfaces as a rejected promise instead of crashing the host process, and `PathError` carries a stable `code` (`PathFaultCode`) you can branch on, with the human-readable message owned by the facade. The path resolver's last runtime `expect` invariant is now carried in the type system (via `Option::insert`), removing a panic path from the hot scan loop.
- Updated dependencies [ce2b760]
- Updated dependencies [e0c03ad]
  - @botejs/native@0.4.0

## 0.3.0

### Minor Changes

- 893751a: Make the API's unhappy paths consistent.

  **Breaking:** new `PathError` (exported from `@botejs/core`, carries `path`) is thrown for a path that contradicts the document's shape:
  - traversing through a non-container, a wrong-kind segment (member name against an array, index against an object), a container op (`count`/`iter`/`walk`) on a present scalar, and `iter`-on-an-object / `walk`-on-an-array (was a plain `Error`).
  - A clean miss (missing key, out-of-bounds index) still returns the not-found sentinel: `get`->`undefined`, `count`->`0`, `hop`->`null`, `iter`/`walk`->empty.
  - `has` returns `false` for any miss or shape mismatch and never throws on navigation; it is now presence-only, so a malformed leaf value at the resolved path reports `true`.
  - `select` sub-paths that don't match an element's shape yield `null`.

  Other changes:
  - `get(path, schema)` runs the schema against a missing key (required schema -> `ValidationError`, optional -> `undefined`).
  - Knob/option validation moved to the facade with uniform types: `RangeError` for `batch`, the cache knobs, `chunkBytes`, `size`, `onInvalid`; `TypeError` for `withIndex`, a non-Standard-Schema trailing object, and bad `select` field values.
  - `iter`'s `batch` is capped at `MAX_ITER_BATCH` (1,000,000).
  - A source `read()` returning 0 bytes for an in-bounds request now errors instead of hanging.
  - `get`/`has` no longer crash on a non-schema trailing object.
  - A failing reader `close()` during a failed `open()` no longer masks the original error (attached as `error.cause`).
  - `close()` invalidates the cursor uniformly (any later call throws `bote: cursor is closed`); sub-cursors from `walk`/`hop` share the root's closed state.

- 52fe8be: Add `Cursor.hop(...path)`: resolves a path once and returns a cursor anchored at that value (or `null` if absent), so later relative reads start from its anchor.
- 7a49177: `walk` is now object-only and yields `[key, cursor]` tuples. Pointing it at an array throws (use `iter`), and the standalone `Cursor.key` getter is removed.

### Patch Changes

- 7243a60: Fix a severe slowdown on repeated deep reads into a very wide object. The structural-index cache built its object member table with a linear scan (O(members^2)) and could mint a table larger than the cache budget, only to evict it immediately and rebuild it on the next read. Member tables are now hash-backed (O(1) lookup/dedup) and clamped to the budget.
- Updated dependencies [893751a]
- Updated dependencies [52fe8be]
- Updated dependencies [7a49177]
- Updated dependencies [7243a60]
  - @botejs/native@0.3.0

## 0.2.0

### Minor Changes

- cc14e77: Reshape the cursor API around path segments and batched, projecting iteration.
  - **Path addressing.** `get`, `has`, `count`, `iter`, and `walk` now take a value as variadic path segments (`cursor.get('users', 1000, 'name')`) instead of a JSON-pointer string. Segments are strings (object keys) or non-negative integers (array indices). A missing path resolves to `undefined` (`get`) / `false` (`has`) / `0` (`count`).
  - **`count(...path)`.** Added this so that it's easy to get the length of an array.
  - **`iter` batching, projection, and validation.** `iter` yields arrays of up to `batch` items, keeping peak JS memory to one batch. `select` projects each child before it crosses. A single sub-path yields the bare sub-value, a named map yields an object of sub-values in declared key order, so non-selected parts of a child never materialize (a missing sub-path yields `null`). A `schema` validates each item after `select`: `onInvalid: 'throw'` raises a `ValidationError`, `onInvalid: 'skip'` drops invalid items to act as a conformance filter. `withIndex` yields `[index, value]` pairs. `get`/`has` likewise accept a Standard Schema validator as the final argument.
  - **Key order.** Object output (from `get`, `iter`, and `select` maps) is emitted in document / declared key order, where it was previously sorted.
  - **Fix.** Breaking out of a `for await` over `iter`/`walk` (or any early termination) now releases the underlying iterator and pins cleanly.

- 381d12d: Replace the chunk cache with a structural-index cache.
  - **Structural-index cache.** Warm queries now reuse the _containers_ walked, not source bytes: each container keeps an object child-table (`name -> offset`) plus a resume offset (objects) or sorted `(index, offset)` landmarks (arrays), so a later query landing in a walked container starts its scan near the target and faults fewer chunks. The walk itself still caches no chunk or bitmap bytes.
  - **New tuning knobs.** `open` accepts `indexCacheEntries` (cache slot budget, `0` disables, default 1024), `objectMemberCap` (max tabled members per object, `0` disables, default unbounded), and `arrayIndexInterval` (element stride between array landmarks, `0` disables, default 16).
  - **`chunkBytes` is now required** on the source descriptor (whole, multiple of 64).
  - **Removed.** `Cursor.cacheStats()` and the `CacheStats` type, the `BoteOptions`/`SessionOptions` second argument to `open`, and the `maxResidentChunks` knob. Bounded residency is now structural (the burst window), asserted in the native layer rather than sampled from JS.

### Patch Changes

- Updated dependencies [cc14e77]
- Updated dependencies [381d12d]
  - @botejs/native@0.2.0

## 0.1.4

### Patch Changes

- 266e241: add README to core

## 0.1.3

### Patch Changes

- 65590bf: fix fromHttpRange picking up compressed bytes.
- Updated dependencies [65590bf]
  - @botejs/native@0.1.3

## 0.1.2

### Patch Changes

- 289c70b: more CI pain
- Updated dependencies [289c70b]
  - @botejs/native@0.1.2

## 0.1.1

### Patch Changes

- 62c280d: Bump to 0.1.1 to move past the partially-published 0.1.0 platform sub-package.
- Updated dependencies [62c280d]
  - @botejs/native@0.1.1
