---
'@botejs/native': minor
'@botejs/core': minor
---

Unify iteration on `iter`; remove `walk`.

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
