---
'@botejs/core': minor
---

Add `schema` / `onInvalid` to `scan`. With a `schema`, each yielded item (after `select`) is validated; `onInvalid: 'throw'` (the default) raises a `ValidationError`, while `onInvalid: 'skip'` drops invalid items, turning the schema into a conformance filter. Batched scans are handled too, skipping shrinks the affected batch.
