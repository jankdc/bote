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

Here's a run (Apple M1 Pro 2021, default settings, RUNS=100):

| method                      | mean time (seconds) | mean peak footprint (MB) |
| --------------------------- | ------------------- | ------------------------ |
| bote: v0.8                   | 0.517 ± 0.018 s     | 40.3 ± 2.5               |
| JSON.parse                   | 0.816 ± 0.031 s     | 648.9 ± 2.4              |
| JSONStream: v1.3             | 4.452 ± 0.052 s     | 57.9 ± 3.9               |
| @streamparser/json: v0.0.22  | 5.103 ± 0.084 s     | 47.9 ± 2.3               |
| oboe.js: v2.1                | 8.566 ± 0.295 s     | 100.0 ± 4.6              |
| stream-json: v3.4.0          | 13.346 ± 0.569 s    | 207.6 ± 8.4              |

For comparison notes, go [here](https://github.com/jankdc/bote-comparison).

## Features

- Modern `AsyncIterator` API with helpers that emulate the [tc39 ones](https://github.com/tc39/proposal-async-iterator-helpers)
- Validate with [Standard Schema](https://standardschema.dev/), avoiding those pesky `unknown`s
- Supports multiple sources of data (e.g. file, network, stream) or write a custom one. (see [sources.js](./examples/sources.js) for the built-in ones)
- For forward-only sources, there's support for replaying/buffering, allowing navigation to previous values

## Documentation

Coming soon. Check the [./examples](./examples/) folder for usages. I've also heavily JSDoc'ed the hell out of the API so have fun
playing around with it for now.

## Status

Pre-1.0. Still in development and APIs may change based on feedback, bugs and holy divinations from the coding gods.

I would say 90% satisfactory for MVP, but I'm getting there.

## License

MIT.
