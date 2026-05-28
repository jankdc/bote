import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { open, fromBuffer, fromFile, fromHttpRange, type Source } from '../src/index.ts'
import { DOC, enc } from './fixtures.ts'

/** Replace `globalThis.fetch` for the duration of a test; returns a restore fn. */
function mockFetch(handler: (url: string, init: RequestInit) => Response | Promise<Response>): () => void {
  const original = globalThis.fetch
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) =>
    handler(String(input), init ?? {})) as typeof fetch
  return () => {
    globalThis.fetch = original
  }
}

// Source construction & I/O contract, plus cursor lifecycle (close / dispose).

test('source_custom_open_and_get', async (t) => {
  const cursor = await open(fromBuffer(enc(DOC)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
  assert.equal(await cursor.get('/meta/enabled'), true)
})

test('source_from_file_reads_from_disk', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'bote-'))
  const path = join(dir, 'doc.json')
  writeFileSync(path, DOC)
  const cursor = await open(fromFile(path, { chunkBytes: 64 }), { maxResidentChunks: 16 })
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
  const names: string[] = []
  for await (const name of cursor.scan('/users', { select: '/name' })) {
    names.push(name as string)
  }
  assert.deepEqual(names, ['Alice', 'Bob'])
})

test('source_open_is_deferred_until_open_call', async () => {
  let opened = 0
  const source: Source = {
    open: () => {
      opened += 1
      const data = enc(DOC)
      return Promise.resolve({
        size: data.length,
        read: (offset, length) => Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length))),
      })
    },
  }
  assert.equal(opened, 0, 'constructing a Source must not trigger open()')
  const cursor = await open(source)
  assert.equal(opened, 1)
  await cursor.close()
})

test('lifecycle_close_drives_reader_close_exactly_once', async () => {
  let closeCalls = 0
  const source: Source = {
    open: () => {
      const data = enc(DOC)
      return Promise.resolve({
        size: data.length,
        read: (offset, length) => Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length))),
        close: async () => {
          closeCalls += 1
        },
      })
    },
  }
  const cursor = await open(source)
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
  await cursor.close()
  await cursor.close()
  assert.equal(closeCalls, 1)
})

test('lifecycle_await_using_disposes_reader_at_scope_exit', async () => {
  let closeCalls = 0
  const source: Source = {
    open: () => {
      const data = enc(DOC)
      return Promise.resolve({
        size: data.length,
        read: (offset, length) => Promise.resolve(data.subarray(offset, Math.min(offset + length, data.length))),
        close: async () => {
          closeCalls += 1
        },
      })
    },
  }
  const cursor = await open(source)
  try {
    assert.equal(await cursor.get('/users/0/name'), 'Alice')
    assert.equal(closeCalls, 0, 'reader stays open inside the scope')
  } finally {
    await cursor[Symbol.asyncDispose]()
  }
  assert.equal(closeCalls, 1, 'scope exit must drive Symbol.asyncDispose -> reader.close')
})

test('lifecycle_open_failure_still_closes_reader', async () => {
  // The native `open` rejects on `size: NaN` (lib.rs requires a finite,
  // non-negative number). `open()` must still drive the reader's `close()`
  // when the native side fails after `source.open()` has succeeded - otherwise
  // a failed open leaks a file handle.
  let closeCalls = 0
  const source: Source = {
    open: () =>
      Promise.resolve({
        size: Number.NaN,
        read: () => Promise.resolve(new Uint8Array()),
        close: async () => {
          closeCalls += 1
        },
      }),
  }
  await assert.rejects(() => open(source))
  assert.equal(closeCalls, 1, 'reader.close must run even when openNative rejects')
})

// fromHttpRange isn't exercised by any of the existing specs - they all use
// memory or file sources. Mocking `globalThis.fetch` lets us cover the HTTP
// protocol surface without a real server. The four cases below are: happy
// path (HEAD then GET-with-Range), missing `Accept-Ranges: bytes` rejection,
// non-OK HEAD rejection, and a misconfigured server that ignores Range and
// returns 200 (which we must reject so we don't buffer the whole body).

test('source_from_http_range_reads_with_head_then_get_range', async (t) => {
  const data = enc(DOC)
  const restore = mockFetch((_url, init) => {
    if (init.method === 'HEAD') {
      return new Response(null, {
        status: 200,
        headers: {
          'content-length': String(data.byteLength),
          'accept-ranges': 'bytes',
        },
      })
    }
    // GET: parse the Range header and slice `data`.
    const range = new Headers(init.headers).get('range') ?? ''
    const m = range.match(/^bytes=(\d+)-(\d+)$/)
    assert.ok(m, `expected bytes=N-M range header, got ${JSON.stringify(range)}`)
    const start = Number(m[1])
    const endInclusive = Number(m[2])
    // TS 6's BodyInit accepts ArrayBufferView<ArrayBuffer>, not the generic
    // Uint8Array<ArrayBufferLike> returned by .subarray(). .slice() copies
    // into a fresh ArrayBuffer; pass that directly.
    const body = data.slice(start, endInclusive + 1).buffer as ArrayBuffer
    return new Response(body, { status: 206 })
  })
  t.after(restore)

  const cursor = await open(fromHttpRange('https://example.test/doc.json', { chunkBytes: 64 }))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
  assert.equal(await cursor.get('/meta/enabled'), true)
})

test('source_from_http_range_rejects_when_accept_ranges_missing', async (t) => {
  const restore = mockFetch(() =>
    new Response(null, {
      status: 200,
      headers: { 'content-length': '100' }, // no accept-ranges advertised
    }),
  )
  t.after(restore)
  await assert.rejects(
    () => open(fromHttpRange('https://example.test/doc.json')),
    /does not advertise Accept-Ranges/,
  )
})

test('source_from_http_range_rejects_when_head_not_ok', async (t) => {
  const restore = mockFetch(() => new Response(null, { status: 404, statusText: 'Not Found' }))
  t.after(restore)
  await assert.rejects(
    () => open(fromHttpRange('https://example.test/doc.json')),
    /HEAD .* failed: 404/,
  )
})

test('source_from_http_range_rejects_when_get_returns_200_ignoring_range', async (t) => {
  // The server returns 200 with the full body for the Range GET. fromHttpRange
  // rejects on read so we never buffer the whole body in memory.
  const data = enc(DOC)
  const restore = mockFetch((_url, init) => {
    if (init.method === 'HEAD') {
      return new Response(null, {
        status: 200,
        headers: {
          'content-length': String(data.byteLength),
          'accept-ranges': 'bytes',
        },
      })
    }
    // Same BodyInit-cast as above: pass the underlying ArrayBuffer.
    return new Response(data.slice().buffer as ArrayBuffer, { status: 200 })
  })
  t.after(restore)
  const cursor = await open(fromHttpRange('https://example.test/doc.json', { chunkBytes: 64 }))
  t.after(() => cursor.close())
  await assert.rejects(() => cursor.get('/users/0/name'), /ignored Range and returned 200/)
})
