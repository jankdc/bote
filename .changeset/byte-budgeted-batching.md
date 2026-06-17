---
'@botejs/core': minor
'@botejs/native': minor
---

iter batching is now bounded by bytes as well as count.

**Breaking:** the `iter` option `batch` is renamed to `maxBatchCount`, and the
exported constants `DEFAULT_ITER_BATCH` / `MAX_ITER_BATCH` become
`DEFAULT_MAX_BATCH_COUNT` / `MAX_BATCH_COUNT`.

**New:** `maxBatchBytes` caps the serialized bytes held per fetch (default
`262144`, 256 KiB; must be a positive integer). A fetch flushes when it reaches
`maxBatchCount` items or `maxBatchBytes` bytes, whichever binds first, so neither
is guaranteed - both are caps the fetch fills up to. At least one item is always
fetched, so a single item larger than the budget still makes progress.

This keeps peak memory bounded when items are large (e.g. records with big nested
arrays) regardless of count: on the 181 MB citylots GeoJSON, fully materializing
each feature drops from ~210 MB to ~50 MB of V8 heap at the same throughput. To
let the count dominate instead, set `maxBatchBytes` high.
