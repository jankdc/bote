# bote

a minimal, ergonomic and low-memory approach to navigating a big JSON:

```sh
npm install @botejs/core
```

```ts
import { open, fromFile } from '@botejs/core';
import { publish } from './message-bus';

// e.g. { items: [...] }
await using cursor = await open(fromFile('./some-large.json'));

// items[0]
const first = await cursor.get('items', 0);
console.log(`first item: ${first}`);
```

given a **seekable** source (e.g. a file, an HTTP range) and a path, it retrieves values out of a JSON quickly, without loading the whole thing in-memory.

here's a run (Apple M1 Pro 2021, ~500MB JSON array file, cold-cache, default settings):

| operation      | approach   |      time | js heap peak delta | rust heap peak |
| -------------- | ---------- | --------: | -----------------: | -------------: |
| items[0]       | JSON.parse |    1.81 s |            1.21 GB |            n/a |
| items[535399]  | JSON.parse |    1.74 s |            1.21 GB |            n/a |
| items[1070797] | JSON.parse |    1.74 s |            1.21 GB |            n/a |
| items[0]       | bote       |   1.29 ms |            63.3 KB |       130.8 KB |
| items[535399]  | bote       | 193.49 ms |           191.5 KB |        36.7 MB |
| items[1070797] | bote       | 379.98 ms |           189.8 KB |        37.2 MB |

## array access

`iter` streams the children of a container at a path **one item at a time**, so you never hold the whole collection in memory. it works on either kind: array elements or object member values. each `for await` step yields a single item:

```ts
// e.g. [{ id: 'user-1' }, { id: 'user-2' }, ...]
await using cursor = await open(fromFile('./users.json'));

// root is an array
for await (const user of cursor.iter()) {
  console.log(user);
}
```

the item loop is the ergonomic default; it costs a flat ~10% over a full walk. for hot paths, `.raw()` hands back the raw fetch arrays with no per-item tax (the `batch` option sets their size and the memory bound):

```ts
for await (const users of cursor.iter().raw()) {
  for (const user of users) {
    console.log(user);
  }
}
```

## object access

`iter` over an object yields its **member values** in document order. add `withKey: true` to get **`[key, value]`** pairs instead, where `key` is the member name (for an array, `key` is the element's index). streamed either way, so a million-member object never lands on the heap at once:

```ts
// e.g. { alice: { role: 'admin' }, bob: { role: 'guest' }, ... }
await using cursor = await open(fromFile('./accounts.json'));

for await (const [name, account] of cursor.iter({ withKey: true })) {
  // name is the member name ('alice', 'bob', ...); account is its value
  console.log(`${name}: ${account.role}`);
}
```

see [`recursive.js`](./examples/recursive.js) for advanced use-cases.

## hopping

`hop` resolves a path once and hands back a **cursor** anchored at that value (or `null` if the path isn't there):

```ts
// e.g. { report: { sections: [{ rows: [...] }, ...] } }
await using cursor = await open(fromFile('./report.json'));

const section = await cursor.hop('report', 'sections', 0);
if (section) {
  console.log(await section.count('rows'));
  for await (const row of section.iter('rows')) {
    console.log(row);
  }
}
```

## validation

`get`, and `iter` takes a [Standard Schema](https://standardschema.dev) validator as their last argument (for `iter`, can also be passed in an `options` object). the value is validated and the return type is inferred from the schema, so reads come back typed instead of `unknown`:

```ts
import { open, fromFile } from '@botejs/core';
import * as z from 'zod'; // or any Standard Schema validator

// a downstream API that wants a typed list of recipients
declare function sendNewsletter(recipients: string[]): Promise<void>;

const User = z.object({
  id: z.string(),
  name: z.string(),
  email: z.string(),
});

const cursor = await open(fromFile('./users.json'));

// name: string
const name = await cursor.get('users', 1000, 'name', User.shape.name);

let emails: string[] = [];
// .raw() to hand each fetch's worth of recipients to the batched API at once
for await (const user of cursor.iter('users', User)) {
  // user: User
  emails.push(user.email);
}

await sendNewsletter(emails);
await cursor.close();
```

## memory

bote keeps a small **structural-index** cache: as scans walk containers (arrays and object), it remembers where members live, so a later query that lands in an already walked container resumes near the target instead of from the top. it caches structure, never source bytes, so it can't grow unbounded with document size.

the defaults are good, but `open` takes a few knobs: `indexCacheEntries`, `objectMemberCap`, and `arrayIndexInterval`. to bound memory tighter or turn the cache off. see [`memory.js`](./examples/memory.js) for what each does.

## sources

bote ships `fromFile`, `fromHttpRange`, and `fromBuffer` as pre-built sources. create your own by implementing the `Source` interface. see [`sources-custom.ts`](./examples/sources-custom.ts) or [./packages/core/src/sources.ts](./packages/core/src/sources.ts) for how it works.

## status

pre-1.0 so still in development and APIs may change based on feedback, bugs and holy divinations from the coding gods.

## license

MIT.
