---
'@botejs/native': minor
'@botejs/core': minor
---

Make the API's unhappy paths consistent.

**Breaking:** new `PathError` (exported from `@botejs/core`, carries `path`) is thrown for a path that contradicts the document's shape:

- traversing through a non-container, a wrong-kind segment (member name against an array, index against an object), a container op (`count`/`iter`/`walk`) on a present scalar, and `iter`-on-an-object / `walk`-on-an-array (was a plain `Error`).
- A clean miss (missing key, out-of-bounds index) still returns the not-found sentinel: `get`->`undefined`, `count`->`0`, `hop`->`null`, `iter`/`walk`->empty.
- `has` returns `false` for any miss or shape mismatch and never throws on navigation; it is now presence-only, so a malformed leaf value at the resolved path reports `true`.
- `select` sub-paths that don't match an element's shape yield `null`.

Other changes:

- `get(path, schema)` runs the schema against a missing key (required schema -> `ValidationError`, optional -> `undefined`).
- Knob/option validation moved to the facade with uniform types: `RangeError` for `batch`, the cache knobs, `chunkBytes`, `size`, `onInvalid`; `TypeError` for `withIndex`, a non-Standard-Schema trailing object, and bad `select` field values.
- `iter`'s `batch` is capped at `MAX_ITER_BATCH` (1,000,000).
- A source `read()` returning 0 bytes for an in-bounds request now errors instead of hanging.
- `get`/`has` no longer crash on a non-schema trailing object.
- A failing reader `close()` during a failed `open()` no longer masks the original error (attached as `error.cause`).
- `close()` invalidates the cursor uniformly (any later call throws `bote: cursor is closed`); sub-cursors from `walk`/`hop` share the root's closed state.
