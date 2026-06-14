import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import { gzipSync } from 'node:zlib';

import { open, fromReadable, fromBuffer, fromHttpRequest, ForwardReplayError, SourceReadError } from '../src/index.ts';
import { DOC, enc } from './fixtures.ts';

/** A fresh web stream over `data`, emitted in small chunks so reads must reassemble. */
function webStreamOf(data: Uint8Array, chunkSize = 16): ReadableStream<Uint8Array> {
  let offset = 0;
  return new ReadableStream({
    pull(controller) {
      if (offset >= data.byteLength) {
        controller.close();
        return;
      }
      const end = Math.min(offset + chunkSize, data.byteLength);
      controller.enqueue(data.subarray(offset, end));
      offset = end;
    },
  });
}

/** Replace `globalThis.fetch` for the duration of a test; returns a restore fn. */
function mockFetch(handler: (url: string, init: RequestInit) => Response | Promise<Response>): () => void {
  const original = globalThis.fetch;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) =>
    handler(String(input), init ?? {})) as typeof fetch;
  return () => {
    globalThis.fetch = original;
  };
}

test('forward_single_pass_serves_one_query', async (t) => {
  const cursor = await open(fromReadable(() => webStreamOf(enc(DOC))));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('meta', 'version'), 'v2');
});

test('forward_iter_single_consumption_yields_all', async (t) => {
  const cursor = await open(fromReadable(() => webStreamOf(enc(DOC))));
  t.after(() => cursor.close());
  const names: unknown[] = [];
  for await (const name of cursor.iter('users', { select: ['name'] })) {
    names.push(name);
  }
  assert.deepEqual(names, ['Alice', 'Bob']);
});

test('forward_accepts_node_readable', async (t) => {
  const bytes = enc(DOC);
  const chunks = [bytes.subarray(0, 30), bytes.subarray(30, 90), bytes.subarray(90)];
  const cursor = await open(fromReadable(() => Readable.from(chunks)));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 1, 'name'), 'Bob');
});

test('forward_size_hint_resolves', async (t) => {
  const bytes = enc(DOC);
  const cursor = await open(fromReadable(() => webStreamOf(bytes), { size: bytes.byteLength }));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice');
});

test('forward_second_query_throws_replay_error', async (t) => {
  const cursor = await open(fromReadable(() => webStreamOf(enc(DOC))));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice');
  const err = await cursor.get('meta', 'version').catch((e) => e);
  assert.ok(err instanceof ForwardReplayError, `expected ForwardReplayError, got ${err}`);
  assert.equal(err.code, 'forward_replay');
  // The guidance must name the actual opt-in shape, so it can't drift from the API.
  assert.match(err.message, /rewind: 'replay'/);
  assert.match(err.message, /rewind: 'buffer'/);
});

test('forward_hop_then_get_throws_without_replay', async (t) => {
  const cursor = await open(fromReadable(() => webStreamOf(enc(DOC))));
  t.after(() => cursor.close());
  const meta = await cursor.hop('meta');
  assert.ok(meta);
  await assert.rejects(() => meta.get('version'), ForwardReplayError);
});

test('forward_stream_error_surfaces_as_source_error', async (t) => {
  const cursor = await open(
    fromReadable(
      () =>
        new ReadableStream({
          start(controller) {
            controller.error(new Error('upstream boom'));
          },
        }),
    ),
  );
  t.after(() => cursor.close());
  const err = await cursor.get('meta').catch((e) => e);
  assert.ok(err instanceof SourceReadError, `expected SourceReadError, got ${err}`);
  assert.match(err.message, /upstream boom/);
});

test('replay_serves_second_query', async (t) => {
  let acquisitions = 0;
  const source = fromReadable(
    () => {
      acquisitions++;
      return webStreamOf(enc(DOC));
    },
    { rewind: 'replay' },
  );
  const cursor = await open(source);
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice');
  assert.equal(await cursor.get('meta', 'version'), 'v2');
  assert.ok(acquisitions >= 2, `replay must re-acquire the producer, saw ${acquisitions} acquisitions`);
});

test('buffer_serves_out_of_order_queries', async (t) => {
  let acquisitions = 0;
  const source = fromReadable(
    () => {
      acquisitions++;
      return webStreamOf(enc(DOC));
    },
    { rewind: 'buffer' },
  );
  const cursor = await open(source);
  t.after(() => cursor.close());
  // A later member first, then an earlier one: random access over the snapshot.
  assert.equal(await cursor.get('meta', 'version'), 'v2');
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice');
  assert.equal(acquisitions, 1, 'buffer mode snapshots a single pass');
});

test('decode_gzip_per_acquisition', async (t) => {
  const compressed = new Uint8Array(gzipSync(Buffer.from(DOC)));
  const cursor = await open(
    fromReadable(() => webStreamOf(compressed), {
      rewind: 'replay',
      // DecompressionStream's writable is typed WritableStream<BufferSource>, which
      // the DOM lib won't unify with pipeThrough's Uint8Array chunk; cast the pair.
      decode: (raw) =>
        raw.pipeThrough(new DecompressionStream('gzip') as unknown as ReadableWritablePair<Uint8Array, Uint8Array>),
    }),
  );
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice');
  // A re-acquisition must gunzip the fresh pass too.
  assert.equal(await cursor.get('meta', 'enabled'), true);
});

test('http_get_reads_forward_body', async (t) => {
  const data = enc(DOC);
  const restore = mockFetch(() => new Response(data.slice().buffer as ArrayBuffer, { status: 200 }));
  t.after(restore);
  const cursor = await open(fromHttpRequest('https://example.test/doc.json'));
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 1, 'name'), 'Bob');
});

test('http_authorization_header_survives_refetch', async (t) => {
  const data = enc(DOC);
  const seen: Array<string | null> = [];
  const restore = mockFetch((_url, init) => {
    seen.push(new Headers(init.headers).get('authorization'));
    return new Response(data.slice().buffer as ArrayBuffer, { status: 200 });
  });
  t.after(restore);
  const cursor = await open(
    fromHttpRequest('https://example.test/doc.json', {
      rewind: 'replay',
      init: { headers: { Authorization: 'Bearer t0ken' } },
    }),
  );
  t.after(() => cursor.close());
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice');
  assert.equal(await cursor.get('meta', 'version'), 'v2');
  assert.ok(seen.length >= 2, `expected a re-fetch, saw ${seen.length} requests`);
  assert.ok(
    seen.every((h) => h === 'Bearer t0ken'),
    `every request must carry the auth header, saw ${JSON.stringify(seen)}`,
  );
});

test('http_failed_status_rejects_open', async (t) => {
  // The first pass is acquired by open(), so a failed GET surfaces there - the
  // same site fromHttpRange surfaces a failed HEAD.
  const restore = mockFetch(() => new Response(null, { status: 503, statusText: 'Unavailable' }));
  t.after(restore);
  await assert.rejects(() => open(fromHttpRequest('https://example.test/doc.json')), /failed: 503/);
});

test('open_rejects_cache_knobs_on_forward_source', async () => {
  await assert.rejects(
    () =>
      open(
        // @ts-expect-error - a forward source's overload takes no cache-knob options
        fromReadable(() => webStreamOf(enc(DOC))),
        { objectMemberCap: 8 },
      ),
    /is not allowed for a forward source/,
  );
});

test('open_forward_overload_rejects_knobs_at_compile_time', () => {
  async function _typeChecks() {
    const forward = fromReadable(() => webStreamOf(enc(DOC)));
    const seekable = fromBuffer(enc(DOC));

    await open(forward); // forward overload takes no options

    // @ts-expect-error - indexCacheEntries is not allowed for a forward source
    await open(forward, { indexCacheEntries: 0 });

    // @ts-expect-error - objectMemberCap is not allowed for a forward source
    await open(forward, { objectMemberCap: 0 });

    // @ts-expect-error - arrayIndexInterval is not allowed for a forward source
    await open(forward, { arrayIndexInterval: 16 });

    await open(seekable, { arrayIndexInterval: 16 }); // seekable accepts the knobs
  }
  assert.equal(typeof _typeChecks, 'function');
});

test('forward_producer_deferred_until_open', async () => {
  let calls = 0;
  const source = fromReadable(() => {
    calls++;
    return webStreamOf(enc(DOC));
  });
  assert.equal(calls, 0, 'constructing a forward source must not invoke the producer');
  const cursor = await open(source);
  assert.equal(calls, 1, 'open() acquires exactly one pass');
  await cursor.close();
});
