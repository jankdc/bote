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
  email: z.string(),
})

type User = z.infer<typeof User>

await using cursor = await open(fromFile('./your-big.json'))

// if you want one value
const user0: unknown = await cursor.get('/1234/users/0')

// for .get and .scan, you can supply a validator
const user1: User = await cursor.get('/1234/users/1', User)

// if you want to sweep a list of values
for await (const user of cursor.scan('/1234/users')) {
  console.log(user)
}

// project a single field per child without materializing the whole thing
for await (const id of cursor.scan('/1234/users', { select: '/id' })) {
  console.log({ id })
}

// add `withKey: true` to get the index (or member name) alongside the value
for await (const [i, id] of cursor.scan('/1234/users', { select: '/id', withKey: true })) {
  console.log({ i, id })
}

// for open-ended per-child work (conditional reads, recursive descent, nested
// scans), `walk` still yields a subcursor positioned at each child:
for await (const userCursor of cursor.walk('/1234/users')) {
  if (await userCursor.has('/details')) {
    console.log(await userCursor.get('/details'))
  }
}

// 'await using' would normally clean up resources for you
// when it goes out of lexical scope. if you hate that,
// you can do it explicitly as well.
await cursor.close()
```

given a **seekable** source (e.g. a file, an HTTP range) and a JSON pointer, it can retrieve values in a JSON quickly, without loading the whole thing in-memory.

here's a run (Apple M1 Pro 2021, 500MB JSON array file, cold-cache, default settings):

| operation    | approach   |      time | js heap peak Δ | rust heap peak |
| ------------ | ---------- | --------: | -------------: | -------------: |
| items[0]     | JSON.parse |    1.75 s |        1.21 GB |            n/a |
| items[len/2] | JSON.parse |    1.82 s |        1.21 GB |            n/a |
| items[len-1] | JSON.parse |    1.76 s |        1.21 GB |            n/a |
| items[0]     | bote       |   1.43 ms |        25.9 KB |        94.9 KB |
| items[len/2] | bote       | 328.81 ms |         1.3 MB |        56.6 MB |
| items[len-1] | bote       | 636.78 ms |         1.3 MB |        56.6 MB |

## sources

bote currently only has `fromFile` and `fromHttpRange` as pre-built sources. create your own by implementing the `Source` interface. see [./packages/core/src/sources.ts](./packages/core/src/sources.ts) on how it works.

## status

pre-1.0 so still in development and APIs may change based on feedback, bugs and holy divinations from the coding gods.

## license

MIT.
