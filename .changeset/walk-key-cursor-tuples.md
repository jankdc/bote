---
'@botejs/native': minor
'@botejs/core': minor
---

`walk` is now object-only and yields `[key, cursor]` tuples. Pointing it at an array throws (use `iter`), and the standalone `Cursor.key` getter is removed.
