---
'@botejs/native': minor
'@botejs/core': minor
---

Add native predicates and `where` pushdown. New constructors `eq`/`lt`/`lte`/`gt`/`gte`/`exists`/`and` build a data IR that is evaluated natively off the raw bytes, so non-matches never materialize. `where` filters `walk` (yields only matching cursors), `scan` (yields only matching values), and `count` (counts only matches), with the predicate's pointers resolved relative to each child. Predicates are total and non-throwing: a missing pointer or a type mismatch is `false`. `eq` is type-aware (`1` ≠ `"1"`); comparisons parse only the value's bytes.
