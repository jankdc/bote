---
'@botejs/core': minor
---

`iter` yields items by default; `batches()` is the new escape hatch.

`cursor.iter(...)` is now a stream **of items**: `for await (const item of cursor.iter(...))`
yields one value at a time (a `[key, value]` tuple with `withKey: true`). Raw
batch access moves behind an explicit `batches()`. The main difference between this method and
the original individual yield item is that this one materialises the batch of items behind the
scenes and yield it individually, amortizing the cost of the FFI rust-js passing whilst still
being performant.

- **Breaking:** the default iterator's yield type changes from `T[]` to `T`.
  Migrate a batch loop by appending `.batches()`:
  `for await (const batch of cursor.iter(...))` → `for await (const batch of cursor.iter(...).batches())`.
  TypeScript flags every site that treated a yield as an array.
- Per-item iteration costs a flat ~10% over a full walk; every hot path keeps a
  zero-tax alternative (`batches()` and the `collect`/`forEach`/`reduce`
  terminals
- `break`ing out of the item loop releases the underlying native scan.
