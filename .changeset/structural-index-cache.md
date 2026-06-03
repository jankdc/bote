---
'@botejs/native': minor
'@botejs/core': minor
---

Replace the chunk cache with a structural-index cache.

- **Structural-index cache.** Warm queries now reuse the _containers_ walked, not source bytes: each container keeps an object child-table (`name → offset`) plus a resume offset (objects) or sorted `(index, offset)` landmarks (arrays), so a later query landing in a walked container starts its scan near the target and faults fewer chunks. The walk itself still caches no chunk or bitmap bytes.
- **New tuning knobs.** `open` accepts `indexCacheEntries` (cache slot budget, `0` disables, default 1024), `objectMemberCap` (max tabled members per object, `0` disables, default unbounded), and `arrayIndexInterval` (element stride between array landmarks, `0` disables, default 16).
- **`chunkBytes` is now required** on the source descriptor (whole, multiple of 64).
- **Removed.** `Cursor.cacheStats()` and the `CacheStats` type, the `BoteOptions`/`SessionOptions` second argument to `open`, and the `maxResidentChunks` knob. Bounded residency is now structural (the burst window), asserted in the native layer rather than sampled from JS.
