---
'@botejs/native': minor
'@botejs/core': minor
---

Add `Cursor.hop(...path)`: resolves a path once and returns a cursor anchored at that value (or `null` if absent), so later relative reads start from its anchor.
