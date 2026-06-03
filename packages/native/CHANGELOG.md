# @botejs/native

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
