import { open, fromFile, fromBuffer, fromHttpRange, fromReadable, fromHttpRequest } from '@botejs/core';
import { createReadStream } from 'node:fs';

// A source tells `open` where the JSON bytes come from. they split into two
// families: Seekable sources support random access and index caching;
// forward sources are a single streamed pass with the cache forced off.

// The seekable family (fromFile/fromBuffer/fromHttpRange) is queryable in any
// order and cache-eligible.

// `fromFile` reads a local file by path. every factory takes an optional
// `chunkBytes` (a non-zero multiple of 64) to override the read granularity.
await using file = await open(fromFile('./users.json', { chunkBytes: 128 * 1024 }));
console.log(await file.get('users', 0, 'name'));

// fromBuffer reads bytes already in memory (a Uint8Array or ArrayBuffer), so it
// suits data you fetched or built yourself rather than a file on disk.
await using buffer = await open(fromBuffer(new TextEncoder().encode('{"ok":true}')));
console.log(await buffer.get('ok'));

// fromHttpRange reads a remote file over HTTP range requests, fetching only the
// byte windows each query needs. `init` is merged into every request (headers,
// credentials, an AbortSignal). the server must advertise Accept-Ranges: bytes.
await using remote = await open(
  fromHttpRange('https://example.com/big.json', {
    init: { headers: { authorization: 'Bearer ...' } },
  }),
);
console.log(await remote.get('users', 1000, 'name'));

// The forward family (fromReadable/fromHttpRequest) is a single streamed pass.

// fromReadable wraps a re-openable readable stream. pass a thunk that produces a
// fresh stream (a live Readable can't be re-streamed), not the stream itself.
await using stream = await open(
  fromReadable(() => createReadStream('./events.json'), {
    // Known total length, if any; lets the engine skip rediscovering the end. (number, optional)
    size: undefined,
    // Transform applied to each (re)acquired stream, e.g. to decompress.
    // (function, optional)
    decode: (raw) => raw.pipeThrough(new DecompressionStream('gzip')),
    // What a query needing an earlier offset does. 'forbid' (single pass; a rewind
    // throws ForwardReplayError), 'replay' (re-acquire from the start; only safe
    // when the thunk is idempotent), or 'buffer' (snapshot into memory for random
    // access at O(n) memory). ('forbid' | 'replay' | 'buffer', default: 'forbid')
    rewind: 'replay',
  }),
);
for await (const event of stream.iter('events')) {
  console.log(event.type);
}

// fromHttpRequest streams an HTTP response body in one pass (GET by default). It
// is the forward counterpart to fromHttpRange: Prefer it when you scan once and
// don't need or have HTTTP range support.
await using download = await open(
  fromHttpRequest('https://example.com/events.json', {
    init: { headers: { authorization: 'Bearer ...' } },
    rewind: 'buffer',
  }),
);
console.log(await download.get('events', 0, 'type'));
