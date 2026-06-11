---
'@botejs/native': minor
'@botejs/core': minor
---

Consolidate every error bote raises under one `BoteError` base class.

`BoteError` (exported from `@botejs/core`) is the abstract base for everything bote throws from its own logic. Catch it to catch anything bote raises, then branch on the new `code` field (`BoteErrorCode`) for the precise kind. Every message stays `bote:`-prefixed.

The concrete subclasses are now all exported and all carry `code`:

- `PathError` (`code`: `PathFaultCode`) - a path that contradicts the document's shape; carries `path`.
- `ValidationError` (`code`: `'validation'`) - schema validation failure; carries `issues` and `path`.
- `MalformedJsonError` (`code`: `JsonFaultCode`) - a malformed value, now distinguishing `unexpected_eof` from `malformed_json`; carries `path`. Was a plain `Error`.
- `SourceReadError` (`code`: `'source_io'`) - a failing source `read()`; carries `path`. Was a plain `Error`.
- `ClosedCursorError` (`code`: `'closed'`) - any call on a closed cursor. Was a plain `Error`.

New exported native fault-code types: `BoteErrorCode`, `JsonFaultCode`, `SourceFaultCode` (alongside the existing `PathFaultCode`). The native addon now emits typed `bote:<code>[:<detail>]` fault lines that the facade rebuilds into the typed errors above, so the human-readable messages live entirely on the JS side.
