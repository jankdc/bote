# @botejs/native

## 0.2.0

### Minor Changes

- cc14e77: Reshape the cursor API around path segments and batched, projecting iteration.

  - **Path addressing.** `get`, `has`, `count`, `iter`, and `walk` now take a value as variadic path segments (`cursor.get('users', 1000, 'name')`) instead of a JSON-pointer string. Segments are strings (object keys) or non-negative integers (array indices). A missing path resolves to `undefined` (`get`) / `false` (`has`) / `0` (`count`).
  - **`count(...path)`.** Added this so that it's easy to get the length of an array.
  - **`iter` batching, projection, and validation.** `iter` yields arrays of up to `batch` items, keeping peak JS memory to one batch. `select` projects each child before it crosses. A single sub-path yields the bare sub-value, a named map yields an object of sub-values in declared key order, so non-selected parts of a child never materialize (a missing sub-path yields `null`). A `schema` validates each item after `select`: `onInvalid: 'throw'` raises a `ValidationError`, `onInvalid: 'skip'` drops invalid items to act as a conformance filter. `withIndex` yields `[index, value]` pairs. `get`/`has` likewise accept a Standard Schema validator as the final argument.
  - **Key order.** Object output (from `get`, `iter`, and `select` maps) is emitted in document / declared key order, where it was previously sorted.
  - **`Cursor.cacheStats()`.** Exposes live chunk-cache occupancy. `residentBytes`, `bitmapBytes`, `residentChunks`, `ceilingBytes`. `residentBytes + bitmapBytes` is the total native memory held for source data and stays at or below `ceilingBytes` regardless of document size, making the bounded-memory contract observable from JS.
  - **Fix.** Breaking out of a `for await` over `iter`/`walk` (or any early termination) now releases the underlying iterator and pins cleanly.

## 0.1.3

### Patch Changes

- 65590bf: fix fromHttpRange picking up compressed bytes.

## 0.1.2

### Patch Changes

- 289c70b: more CI pain

## 0.1.1

### Patch Changes

- 62c280d: Bump to 0.1.1 to move past the partially-published 0.1.0 platform sub-package.
