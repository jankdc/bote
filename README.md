# bote

A fast, modern and low-memory approach to processing a big JSON:

```sh
npm install @botejs/core
```

```ts
import { fileURLToPath } from 'node:url';
import { open, fromFile } from '@botejs/core';

// 181 MB GeoJSON:
// { type: "...", features: [{ properties: { STREET: "..." }}] }
const filePath = fileURLToPath(new URL('../citylots.json', import.meta.url));

await using cursor = await open(fromFile(filePath));

const byStreet = await cursor
  .iter('features', {
    select: ['properties', 'STREET'],
  })
  .reduce((tally, street) => {
    if (typeof street === 'string') {
      tally.set(street, (tally.get(street) ?? 0) + 1);
    }
    return tally;
  }, new Map());

console.log([...byStreet].sort((a, b) => b[1] - a[1]).slice(0, 10));
```

Given a **seekable** or **forward** source and a path, it retrieves values out of a JSON, without loading the whole thing in-memory.

Here's a run of snippet above (Apple M1 Pro 2021, default settings, RUNS=100, Node v26):

| method                        | mean time        | mean peak footprint (MB) |
| ----------------------------- | ---------------- | ------------------------ |
| bote v0.9                     | 0.447 ± 0.002 s  | 34.1 ± 1.9               |
| JSON.parse                    | 0.828 ± 0.023 s  | 508.9 ± 2.4              |
| @discoveryjs/json-ext: v1.1.0 | 1.303 ± 0.012 s  | 397.4 ± 2.7              |
| JSONStream: v1.3              | 4.448 ± 0.055 s  | 62.2 ± 0.7               |
| @streamparser/json: v0.0.22   | 4.935 ± 0.021 s  | 60.1 ± 6.4               |
| oboe.js: v2.1                 | 8.041 ± 0.340 s  | 97.0 ± 1.3               |
| stream-json: v3.4.0           | 12.323 ± 0.876 s | 149.0 ± 6.7              |

For comparison notes, go [here](https://github.com/jankdc/bote-comparison).

## Features

- Modern `AsyncIterator` API with helpers that emulate the [tc39 ones](https://github.com/tc39/proposal-async-iterator-helpers)
- Validate with [Standard Schema](https://standardschema.dev/), avoiding those pesky `unknown`s
- Supports multiple sources of data (e.g. file, network, stream) or write a custom one. (see [sources.js](./examples/sources.js) for the built-in ones)
- For forward-only sources, there's support for replaying/buffering, allowing navigation to previous values

## Supported

- **Node.js >= 22.18.0**
- **ESM-only**
- **Platforms**
  - macOS (Apple Silicon `aarch64` and Intel `x86_64`)
  - Linux x64 (`x86_64`, glibc)
  - Windows x64 (`x86_64`, MSVC)
  - More if requested :)

## API

- [`open(source, options?): Cursor`](#opensource-options)
- [`fromFile(path, options?)`](#fromfilepath-options)
- [`fromBuffer(bytes, options?)`](#frombufferbytes-options)
- [`fromHttpRange(url, options?)`](#fromhttprangeurl-options)
- [`fromHttpRequest(url, options?)`](#fromhttprequesturl-options)
- [`fromReadable(produce, options?)`](#fromreadableproduce-options)
- [`cursor.get(...path, schema?)`](#cursorgetpath-schema)
- [`cursor.has(...path, schema?)`](#cursorhaspath-schema)
- [`cursor.hop(...path)`](#cursorhoppath)
- [`cursor.iter(...path, options?): IterStream`](#cursoriterpath-options)
- [`cursor.close()`](#cursorclose)
- [**`IterStream`**](#iterstream)
- [**Errors**](#errors)

### open(source, options?)

```ts
const cursor = await open(source: Source, options?: SeekableOpenOptions): Promise<RootCursor>;
```

Opens a cursor over a source. The returned `RootCursor` owns the underlying
reader: `close()` (or letting an `await using` scope end) releases it exactly
once. A **seekable** source supports the index cache and repeated, out-of-order
queries; a **forward** source is a single pass and rejects the cache knobs.

```js
import { open, fromFile } from '@botejs/core';

await using cursor = await open(fromFile('./users.json'));
console.log(await cursor.get('users', 0, 'name'));
```

<details><summary>Cache options (seekable sources only)</summary>

The cache remembers where members live as bote walks through the JSON.
It caches structure, never source bytes. The defaults are good;
reach for these only to bound memory tighter or to turn the cache off.

| option               | default   | meaning                                                            |
| -------------------- | --------- | ------------------------------------------------------------------ |
| `indexCacheEntries`  | `1024`    | index entries kept in memory; `0` disables the cache               |
| `objectMemberCap`    | unlimited | keys indexed per object; `0` skips indexing object keys            |
| `arrayIndexInterval` | `16`      | index every Nth array position; `0` skips indexing array positions |

```js
await using cursor = await open(fromFile('./big.json'), {
  arrayIndexInterval: 8,
  indexCacheEntries: 4096,
  objectMemberCap: 256,
});
```

Passing any of these to a forward source throws a `RangeError`.

</details>

### fromFile(path, options?)

```ts
const source = fromFile(path: string, options?: { chunkBytes?: number }): SeekableSource;
```

A seekable source over a local file. Opens a handle on `open()` and reads byte
ranges on demand, so large files are never fully read, only the chunks a query
touches. `chunkBytes` (a non-zero multiple of 64) overrides the read granularity.

```js
await using cursor = await open(fromFile('./users.json', { chunkBytes: 128 * 1024 }));
```

### fromBuffer(bytes, options?)

```ts
const source = fromBuffer(bytes: Uint8Array | ArrayBuffer, options?: { chunkBytes?: number }): SeekableSource;
```

A seekable source over JSON already resident in memory - data you fetched or
built yourself rather than a file on disk.

```js
await using cursor = await open(fromBuffer(new TextEncoder().encode('{"ok":true}')));
console.log(await cursor.get('ok')); // -> true
```

### fromHttpRange(url, options?)

```ts
const source = fromHttpRange(url: string, options?: { chunkBytes?: number; init?: RequestInit }): SeekableSource;
```

A seekable source over a remote file using HTTP range requests. A `HEAD`
discovers the length and confirms `Accept-Ranges: bytes`; each read then fetches
only its byte window. `init` is merged into every request (headers, credentials,
an `AbortSignal`).

```js
await using cursor = await open(
  fromHttpRange('https://example.com/big.json', {
    init: { headers: { authorization: 'Bearer ...' } },
  }),
);
console.log(await cursor.get('users', 1000, 'name'));
```

### fromReadable(produce, options?)

```ts
const source = fromReadable(produce: () => ReadableStream | NodeReadable, options?: ReadableOptions): ForwardSource;
```

A forward-only source backed by a re-openable readable stream. Pass a **thunk**
that produces a fresh stream (a live `Readable` cannot be re-streamed), not the
stream itself. Each cursor operation is an independent scan from the start, so a
second query rewinds, which by default throws (see `rewind`).

```js
import { createReadStream } from 'node:fs';

await using cursor = await open(fromReadable(() => createReadStream('./events.json')));
for await (const event of cursor.iter('events')) {
  console.log(event.type);
}
```

<details><summary>Forward options</summary>

| option       | default    | meaning                                                                |
| ------------ | ---------- | ---------------------------------------------------------------------- |
| `size`       | discovered | known total length, if any; lets the engine skip rediscovering the end |
| `decode`     | none       | transform applied to each (re)acquired stream, e.g. to decompress      |
| `rewind`     | `'forbid'` | what a query needing an earlier offset does (see below)                |
| `chunkBytes` | `262144`   | read granularity (non-zero multiple of 64)                             |

`rewind` trades resident memory for re-read ability:

- `'forbid'` - a single forward pass; a rewind throws `ForwardReplayError`.
- `'replay'` - re-acquire the stream from the start. Safe only when the producer
  is idempotent (yields the same bytes each call). No extra memory.
- `'buffer'` - snapshot the whole stream into memory on first read, enabling
  random access at O(n) resident memory.

```js
await using cursor = await open(
  fromReadable(() => createReadStream('./events.json.gz'), {
    decode: (raw) => raw.pipeThrough(new DecompressionStream('gzip')),
    rewind: 'replay',
  }),
);
```

</details>

### fromHttpRequest(url, options?)

```ts
const source = fromHttpRequest(url: string, options?: HttpRequestOptions): ForwardSource;
```

A forward-only source over an HTTP response body, streamed in one pass (GET by
default). The forward counterpart to `fromHttpRange`: prefer it when you scan
once and the server has no range support. Takes the same `decode`/`rewind`
options as `fromReadable`, plus `init` merged into every `fetch`.

```js
await using cursor = await open(
  fromHttpRequest('https://example.com/events.json', {
    init: { headers: { authorization: 'Bearer ...' } },
    rewind: 'buffer',
  }),
);
console.log(await cursor.get('events', 0, 'type'));
```

### cursor.get(...path, schema?)

```ts
const value = await cursor.get(...path: Segment[], schema?: StandardSchema): Promise<unknown>;
```

Reads and decodes the value at `path`, returning a real JS value, or `undefined`
if the path is absent (distinct from a present JSON `null`). With no segments it
decodes the whole document. Reading a whole container materializes all of it, so
prefer `iter` for large arrays/objects.

```js
const name = await cursor.get('users', 0, 'name'); // -> "Ada"
const missing = await cursor.get('users', 0, 'nope'); // -> undefined
const nulled = await cursor.get('users', 0, 'deletedAt'); // -> null (the member exists)
```

<details><summary>Validating with a schema</summary>

Pass a [Standard Schema](https://standardschema.dev/) (zod, valibot, arktype,
...) as the trailing argument to validate and parse the value. The return type is
inferred from the schema's output, and a validation miss throws a
`ValidationError`.

```js
import { z } from 'zod';

const age = await cursor.get('users', 0, 'age', z.number()); // typed as number
```

</details>

### cursor.has(...path, schema?)

```ts
const exists = await cursor.has(...path: Segment[], schema?: StandardSchema): Promise<boolean>;
```

Reports whether a value exists at `path` without decoding it.
A member explicitly set to JSON `null` still counts as present;
an out-of-range array index is absent.

```js
if (await cursor.has('users', 0, 'email')) {
  console.log(await cursor.get('users', 0, 'email'));
}
console.log(await cursor.has('users', 999)); // -> false on a shorter array
```

<details><summary>Validating with a schema</summary>

With a trailing schema, `has` also requires the value to validate. Unlike `get`,
a parse or validation miss yields `false` instead of throwing.

```js
import { z } from 'zod';

if (await cursor.has('users', 0, 'email', z.string().email())) {
  console.log('has a well-formed email');
}
```

</details>

### cursor.hop(...path)

```ts
const child = await cursor.hop(...path: Segment[]): Promise<Cursor | null>;
```

Resolves `path` to a container and hands back a new cursor anchored there, so
further `get`/`has`/`iter`/`hop` run relative to it. Returns `null` when nothing
lives at the path. A child shares the root's source and lifetime. Closing the
root closes it too, and there is nothing to close on the child itself.

```js
const user = await cursor.hop('users', 0);
if (user) {
  console.log(await user.get('name'));
  const city = await (await user.hop('address'))?.get('city');
}
```

### cursor.iter(...path, options?)

```ts
const stream = cursor.iter(...path: Segment[], options?: IterOptions | StandardSchema): IterStream;
```

Streams the members of the array or object at `path` one item at a time, so a
million-element array never lands in memory all at once. An empty path iterates
the root container; iterating an object yields its values (use `withKey` for the
names). Returns an [`IterStream`](#iterstream).

```js
for await (const user of cursor.iter('users')) {
  console.log(user.name);
}
```

<details><summary>Options</summary>

A trailing [Standard Schema](https://standardschema.dev/) is shorthand for
`{ schema }`. The full options object:

| option          | default   | meaning                                                                             |
| --------------- | --------- | ----------------------------------------------------------------------------------- |
| `select`        | none      | project each member: a segment/path picks a sub-value, a field map builds an object |
| `schema`        | none      | validate each item (after `select`)                                                 |
| `withKey`       | `false`   | yield `[key, value]` tuples (key = member name or array index)                      |
| `onInvalid`     | `'throw'` | policy for items failing `schema`; `'skip'` drops them                              |
| `maxBatchCount` | `1000`    | max items fetched across the native boundary per pull                               |
| `maxBatchBytes` | `262144`  | max serialized bytes held per pull (caps peak memory for large items)               |

```js
for await (const row of cursor.iter('users', {
  select: { id: 'id', email: ['contact', 'email'] },
  schema: z.object({ id: z.number(), email: z.string() }),
  onInvalid: 'skip',
})) {
  console.log(row.id, row.email);
}
```

</details>

### cursor.close()

```ts
await cursor.close(): Promise<void>;
```

Releases the underlying source (file handle, fetch body, etc.). Idempotent, and
only on the root cursor. Prefer `await using` so it runs automatically when the
scope ends; call it directly when you cannot use that syntax.

```js
const cursor = await open(fromFile('./users.json'));
try {
  console.log(await cursor.get('users', 0, 'name'));
} finally {
  await cursor.close();
}
```

### IterStream

`iter` returns a lazy, single-pass pipeline that mirrors the [TC39 async iterator
helpers](https://github.com/tc39/proposal-async-iterator-helpers).

```js
const firstFive = await cursor
  .iter('users', { select: 'name' })
  .filter((name) => name.startsWith('A'))
  .take(5)
  .toArray();
```

Supports `map`, `filter`, `take`, `drop`, `toArray`, `forEach`, `reduce`, `find`, `some` and `every`

### Errors

Everything bote throws extends `BoteError` (catch that to catch anything; branch
on `.code` for the kind). The concrete types are `PathError`, `ValidationError`,
`MalformedJsonError`, `SourceReadError`, `ForwardReplayError`, and
`ClosedCursorError`. Most carry the `path` where the fault occurred.

```js
import { BoteError, ValidationError } from '@botejs/core';

try {
  await cursor.get('users', 0, 'age', z.number());
} catch (err) {
  if (err instanceof ValidationError) {
    console.error(err.issues);
  }
}
```

## Status

Pre-1.0. Still in development and APIs may change based on feedback, bugs and holy divinations from the coding gods.

After a lot of chaos, I'm finally-kinda-sorta happy with the public API. Major breaking changes seems to be slowing
down so feedback from the community and dogfooding on my end is what's next.

## License

MIT.
