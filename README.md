# bote

A fast, modern and low-memory approach to processing a big JSON:

```sh
npm install @botejs/core
```

```ts
// node examples/citylots.js
import { join } from 'node:path';
import { open, fromFile } from '@botejs/core';

// 181 MB GeoJSON:
// { type: "...", features: [{ properties: { STREET: "..." }}] }
const filePath = join(import.meta.dirname, 'citylots.json');

await using cursor = await open(fromFile(filePath));

console.log(`type: ${await cursor.get('type')}`);
// type: 'FeatureCollection'

console.log(`features: ${await cursor.count('features')}`);
// features: 206_560

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
// [[ 'UNKNOWN', 2843 ], [ 'MASON', 2651 ], [ 'PINE', 1799 ], ... ]
```

Given a **seekable** source (e.g. a file, an HTTP range) or "forward-only" source (e.g. HTTP GET request) and a path, it retrieves values out of a JSON, without loading the whole thing in-memory.

Here's a comparison of running above (using Apple M1 Pro 2021's `/usr/bin/time -l`):

| method             | mean time | mean peak footprint (MB) |
| ------------------ | --------- | ------------------------ |
| JSON.parse         | 0.81 s    | 647.0                    |
| bote               | 1.062 s   | 89.0                     |
| @streamparser/json | 4.363 s   | 98.7                     |
| JSONStream         | 4.417 s   | 60.7                     |
| oboe.js            | 9.649 s   | 102.6                    |
| stream-json        | 18.693 s  | 184.3                    |

## Status

Pre-1.0. Still in development and APIs may change based on feedback, bugs and holy divinations from the coding gods.

I would say 90% satisfactory for MVP, but I'm getting there.

## License

MIT.
