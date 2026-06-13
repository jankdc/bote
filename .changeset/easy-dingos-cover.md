---
'@botejs/core': minor
---

## Breaking Changes

- Rename `fromHttpStream` to `fromHttpRequest` and its options type `HttpStreamOptions` to `HttpRequestOptions`. The request method is no longer pinned to GET (it remains the default), so callers can override it via `init.method`.
