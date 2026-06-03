# bote

a minimal, ergonomic and low-memory approach to navigating a big JSON:

```sh
npm install @botejs/core
```

```ts
import { open, fromFile } from '@botejs/core'

import * as z from 'zod' // or bring your own Standard Schema validator

const User = z.object({
  id: z.string(),
  name: z.string(),
  email: z.string(),
  details: z.object({
    lastLoggedIn: z.number(),
  }),
})

type User = z.infer<typeof User>

await using cursor = await open(fromFile('./your-big.json'))

// users[1000].name
const desc0: unknown = await cursor.get('users', 1000, 'name')
// for .get and .iter, you can supply a validator as the last argument
const desc1: string = await cursor.get('users', 1000, 'name', User.shape.name)

// iterate an array in batches
for await (const batch of cursor.iter('users', User)) {
  // batch: User[]
  for (const user of batch) {
    console.log(user)
  }
}

// pick several fields into a named object to avoid resolving big items
for await (const batch of cursor.iter('users', {
  select: {
    id: 'id',
    logged: ['details', 'lastLoggedIn'],
  },
  schema: z.object({
    id: User.shape.id,
    logged: User.shape.details.lastLoggedIn,
  }),
})) {
  // batch: { id: string, logged: number }[]
  for (const userLog of batch) {
    console.log(userLog)
  }
}

// or pick a single field
for await (const batch of cursor.iter('users', {
  select: 'name',
  schema: User.shape.name,
})) {
  // batch: string[]
  for (const name of batch) {
    console.log({ name })
  }
}

// for open-ended per-child work (e.g. conditional reads, recursive descent, nested
// iters), `walk` yields a subcursor positioned at each child:
for await (const metaCursor of cursor.walk('meta')) {
  if (metaCursor.key === 'details') {
    const detailsValue = await metaCursor.get()
    console.log(detailsValue)
  }
}

// 'await using' would normally clean up resources for you
// when it goes out of lexical scope. if you hate that,
// you can do it explicitly as well.
await cursor.close()
```

given a **seekable** source (e.g. a file, an HTTP range) and a path, it can retrieve values in a JSON quickly, without loading the whole thing in-memory.

here's a run (Apple M1 Pro 2021, ~500MB JSON array file, cold-cache, default settings):

| operation      | approach   |      time | js heap peak Δ | rust heap peak |
| -------------- | ---------- | --------: | -------------: | -------------: |
| items[0]       | JSON.parse | 616.02 ms |        1.03 GB |            n/a |
| items[535399]  | JSON.parse | 604.63 ms |        1.03 GB |            n/a |
| items[1070797] | JSON.parse | 600.68 ms |        1.03 GB |            n/a |
| items[0]       | bote       | 527.80 µs |       291.6 KB |       130.4 KB |
| items[535399]  | bote       | 187.24 ms |       742.3 KB |        36.7 MB |
| items[1070797] | bote       | 371.61 ms |       828.7 KB |        37.1 MB |

## sources

bote currently only has `fromFile` and `fromHttpRange` as pre-built sources. create your own by implementing the `Source` interface. see [./packages/core/src/sources.ts](./packages/core/src/sources.ts) on how it works.

## status

pre-1.0 so still in development and APIs may change based on feedback, bugs and holy divinations from the coding gods.

## license

MIT.
