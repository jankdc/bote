# @botejs/native

## 0.4.0

### Minor Changes

- e0c03ad: Materialize values via raw JSON text + `JSON.parse` instead of a `serde_json::Value` round-trip. `get` and `iter` previously parsed a value into a `serde_json::Value` in Rust, which napi then re-walked into JS one property/element at a time (an FFI crossing each). The native layer now hands the value's raw JSON bytes across the boundary and the facade `JSON.parse`s it in a single pass.

  Two behavior changes follow from parsing with `JSON.parse`:
  - Integers beyond 2^53 now deserialize to a JS `Number` (matching native `JSON.parse`) rather than being promoted to a lossless `BigInt`.
  - A malformed value at the resolved path now surfaces as a uniform bote error (`malformed JSON value at <path>`) raised by the facade, rather than a Rust-originated parse error.

### Patch Changes

- ce2b760: Harden error handling: a synchronous throw inside the JS `read` fn now surfaces as a rejected promise instead of crashing the host process, and `PathError` carries a stable `code` (`PathFaultCode`) you can branch on, with the human-readable message owned by the facade. The path resolver's last runtime `expect` invariant is now carried in the type system (via `Option::insert`), removing a panic path from the hot scan loop.

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

- 7243a60: Fix a severe slowdown on repeated deep reads into a very wide object. The structural-index cache built its object member table with a linear scan (O(members²)) and could mint a table larger than the cache budget, only to evict it immediately and rebuild it on the next read. Member tables are now hash-backed (O(1) lookup/dedup) and clamped to the budget.

## 0.2.0

### Minor Changes

- cc14e77: Reshape the cursor API around path segments and batched, projecting iteration.
  - **Path addressing.** `get`, `has`, `count`, `iter`, and `walk` now take a value as variadic path segments (`cursor.get('users', 1000, 'name')`) instead of a JSON-pointer string. Segments are strings (object keys) or non-negative integers (array indices). A missing path resolves to `undefined` (`get`) / `false` (`has`) / `0` (`count`).
  - **`count(...path)`.** Added this so that it's easy to get the length of an array.
  - **`iter` batching, projection, and validation.** `iter` yields arrays of up to `batch` items, keeping peak JS memory to one batch. `select` projects each child before it crosses. A single sub-path yields the bare sub-value, a named map yields an object of sub-values in declared key order, so non-selected parts of a child never materialize (a missing sub-path yields `null`). A `schema` validates each item after `select`: `onInvalid: 'throw'` raises a `ValidationError`, `onInvalid: 'skip'` drops invalid items to act as a conformance filter. `withIndex` yields `[index, value]` pairs. `get`/`has` likewise accept a Standard Schema validator as the final argument.
  - **Key order.** Object output (from `get`, `iter`, and `select` maps) is emitted in document / declared key order, where it was previously sorted.
  - **Fix.** Breaking out of a `for await` over `iter`/`walk` (or any early termination) now releases the underlying iterator and pins cleanly.

- 381d12d: Replace the chunk cache with a structural-index cache.
  - **Structural-index cache.** Warm queries now reuse the _containers_ walked, not source bytes: each container keeps an object child-table (`name → offset`) plus a resume offset (objects) or sorted `(index, offset)` landmarks (arrays), so a later query landing in a walked container starts its scan near the target and faults fewer chunks. The walk itself still caches no chunk or bitmap bytes.
  - **New tuning knobs.** `open` accepts `indexCacheEntries` (cache slot budget, `0` disables, default 1024), `objectMemberCap` (max tabled members per object, `0` disables, default unbounded), and `arrayIndexInterval` (element stride between array landmarks, `0` disables, default 16).
  - **`chunkBytes` is now required** on the source descriptor (whole, multiple of 64).
  - **Removed.** `Cursor.cacheStats()` and the `CacheStats` type, the `BoteOptions`/`SessionOptions` second argument to `open`, and the `maxResidentChunks` knob. Bounded residency is now structural (the burst window), asserted in the native layer rather than sampled from JS.

## 0.1.3

### Patch Changes

- 65590bf: fix fromHttpRange picking up compressed bytes.

## 0.1.2

### Patch Changes

- 289c70b: more CI pain

## 0.1.1

### Patch Changes

- 62c280d: Bump to 0.1.1 to move past the partially-published 0.1.0 platform sub-package.
