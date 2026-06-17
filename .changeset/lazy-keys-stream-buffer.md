---
'@botejs/native': patch
---

Cut per-value and per-key allocations on the read path.

- Values now stream straight into the caller's buffer. `materialize`,
  `project`, and the `iter` item builder append into one shared `Vec` instead
  of each returning a fresh `Vec` that is then copied in. `read_range` appends
  and rolls back on a chunk fault, so a retry re-appends from a clean slate.
- Key comparison borrows the member-key interior in place rather than
  allocating per key. The new `read_slice` returns a `Cow` that only copies
  when a key straddles a chunk seam, and decoding runs solely when an escape is
  present. Key matching/decoding moves into a dedicated `keys` module.
- The structural-index cache takes ownership of scanned members
  (`apply_scan_record` / `merge_object`) instead of cloning them, and chunk
  splitting reuses the source `Bytes` via `slice` rather than copying.
