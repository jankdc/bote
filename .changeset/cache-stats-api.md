---
'@botejs/native': minor
---

Add `Cursor.cacheStats()`, exposing live chunk-cache occupancy (`residentBytes`, `bitmapBytes`, `residentChunks`, `ceilingBytes`). `residentBytes + bitmapBytes` is the total native memory held for source data and stays at or below `ceilingBytes` regardless of document size, making the bounded-memory contract observable from JS.
