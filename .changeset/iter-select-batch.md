---
'@botejs/native': minor
'@botejs/core': minor
---

Add `iter` projection and batching. `select` projects each child before it crosses - a single sub-pointer yields the bare sub-value, a map yields an object of named sub-values in the declared key order, so the non-selected parts of a child never materialize (a missing sub-pointer yields `null`). `batch` yields arrays of up to N items at a time, keeping peak JS memory to one batch. Both compose with `where`. Object output (from `get`, `iter`, and `select` maps) is now emitted in document/declared key order (serde_json `preserve_order`), where it was previously sorted.
