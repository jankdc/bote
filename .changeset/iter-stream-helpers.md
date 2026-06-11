---
'@botejs/core': minor
---

\***\*BREAKING:\*\*** the raw-batch escape hatch is renamed from `batches()` to
`raw()`. Migrate by renaming the call:
`cursor.iter(...).batches()` to `cursor.iter(...).raw()`.

Add chainable helpers to the `iter` stream.

`cursor.iter(...)` now returns an `IterStream<T>` with lazy operators and
eager terminals, so common item-processing no longer needs a hand-written
`for await` loop.

- **Lazy operators** (return a new `IterStream`, nothing runs until iterated or
  a terminal is awaited): `map`, `filter` (with type-guard narrowing), `take`,
  `drop`. Each callback receives a zero-based item index; `map`/`filter` await
  async callbacks. `take` releases the native scan once its limit is reached.
- **Terminals** (await the walk): `toArray`, `forEach`, `reduce`, `find`,
  `some`, `every`. `find`/`some` short-circuit on the first match.
