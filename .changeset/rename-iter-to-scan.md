---
'@botejs/native': minor
'@botejs/core': minor
---

Rename the value-stream traversal verb `iter` to `scan` (`Cursor.scan`; native iterator class `CursorScan`). `walk` navigates positions (yields cursors); `scan` sweeps and extracts values. Breaking rename with no behavior change - update call sites from `cursor.iter(pointer)` to `cursor.scan(pointer)`.
