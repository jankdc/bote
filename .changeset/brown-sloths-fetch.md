---
'@botejs/native': patch
---

Adjust the structural-index caching strategy for iteration and projection.

Iterating a container with `.select` no longer caches the sub-route resolved out
of every child. Each child is anchored at a unique offset visited exactly once,
so caching those resolves only churned the bounded index with entries that were
never read back - and rebuilt a per-object member table on every element.
Projection now resolves without touching the cache, so a full-document `.select`
scan runs substantially faster at the same memory.

In exchange, iterating an array now leaves sparse landmarks behind. As it
streams, `iter` samples `(index, offset)` on the same `arrayIndexInterval` grid
the resolver uses, so a later random index into that array resumes from the
nearest landmark instead of rescanning from the top.

The streaming scan also prunes its chunk window before reading each read-ahead
burst instead of after, so a multi-fault scan now holds at most one burst of
chunks resident at a time rather than the spent burst plus the freshly read one.
This roughly halves peak native memory during a full scan (around 36 MiB down to
20 MiB at the default 64 KiB chunk size) with no change to throughput.
