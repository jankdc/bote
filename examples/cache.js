// A seekable source (fromFile/fromBuffer/fromHttpRange) keeps a small
// *structural-index* cache: as scans walk containers it remembers where members
// live (an object's `name -> offset` table, an array's sampled index landmarks)
// so a later query that lands in an already-walked container resumes its scan near
// the target instead of from the top. it caches *structure*, never source bytes,
// so it can't grow unbounded with document size.
//
// The defaults are good; reach for these knobs only to bound memory tighter on
// huge/awkward docs, or to turn the cache off when every query hits cold paths.
// they are rejected for forward sources, which cannot rewind to reuse an index.

import { open, fromFile } from '@botejs/core';

// All three knobs are optional. Hover on them for more information.
await using cursor = await open(fromFile('./big.json'), {
  arrayIndexInterval: 8,
  indexCacheEntries: 4096,
  objectMemberCap: 256,
});

// The payoff is on repeat, out-of-order access into the same regions: the first
// query into a container pays to walk it, later ones near it resume cheaply.
console.log(await cursor.get('users', 100_000, 'name'));
console.log(await cursor.get('users', 100_001, 'name')); // resumes near the last

// Turn the cache off entirely if you don't need it.
await using noCache = await open(fromFile('./big.json'), {
  objectMemberCap: 0,
  indexCacheEntries: 0,
  arrayIndexInterval: 0,
});
console.log(await noCache.get('users', 0, 'name'));
