// bote keeps a small *structural-index* cache: as scans walk containers it
// remembers where members live (an object's `name -> offset` table, an array's
// sampled index landmarks) so a later query that lands in an already-walked
// container resumes its scan near the target instead of from the top. it caches
// *structure*, never source bytes, so it can't grow unbounded with document size.
//
// the defaults are good; reach for these knobs only to bound memory tighter on
// huge/awkward docs, or to turn the cache off when every query hits cold paths.

import { open, fromFile } from '@botejs/core';

// all three knobs are optional; shown here with their meanings, not their defaults.
await using cursor = await open(fromFile('./big.json'), {
  // total slot budget across the whole cache (one slot per cached container plus
  // one per tabled object member). when a scan tips it over, the deepest, least
  // navigationally useful containers are evicted first (LRU-tiebroken), keeping
  // the shallow backbone that helps future scans.
  // default: 1024
  indexCacheEntries: 4096,

  // max object members tabled per container. lower trades cache memory for
  // less indexing on pathologically wide objects.
  // default: unbounded.
  objectMemberCap: 256,

  // index-stride between sampled array elements. a later index resumes from the
  // nearest landmark at or before it, so a smaller stride means denser landmarks
  // (more memory, faster indexing).
  // default: 16.
  arrayIndexInterval: 8,
});

console.log(await cursor.get('users', 100_000, 'name'));

// turn the cache off entirely. every query scans cold. useful for one-shot reads
// or if you have extreme memory constraints.
// `indexCacheEntries: 0` does the same thing.
await using noCache = await open(fromFile('./big.json'), {
  objectMemberCap: 0,
  arrayIndexInterval: 0,
});
console.log(await noCache.get('users', 0, 'name'));
