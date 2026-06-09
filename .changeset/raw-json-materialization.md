---
'@botejs/native': minor
'@botejs/core': minor
---

Materialize values via raw JSON text + `JSON.parse` instead of a `serde_json::Value` round-trip. `get` and `iter` previously parsed a value into a `serde_json::Value` in Rust, which napi then re-walked into JS one property/element at a time (an FFI crossing each). The native layer now hands the value's raw JSON bytes across the boundary and the facade `JSON.parse`s it in a single pass.

Two behavior changes follow from parsing with `JSON.parse`:

- Integers beyond 2^53 now deserialize to a JS `Number` (matching native `JSON.parse`) rather than being promoted to a lossless `BigInt`.
- A malformed value at the resolved path now surfaces as a uniform bote error (`malformed JSON value at <path>`) raised by the facade, rather than a Rust-originated parse error.
