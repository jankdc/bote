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

test('source_custom_open_and_get', async (t) => {
  const cursor = await open(fromBuffer(enc(DOC)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice')
  assert.equal(await cursor.get('meta', 'enabled'), true)
})

test('source_from_file_reads_from_disk', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'bote-'))
  const path = join(dir, 'doc.json')
  writeFileSync(path, DOC)
  const cursor = await open(fromFile(path, { chunkBytes: 64 }))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice')
  const names: string[] = []
  for await (const batch of cursor.iter('users', { select: ['name'] })) {
    for (const name of batch) names.push(name as string)
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

test('source_rejects_fractional_chunk_bytes', async () => {
  await assert.rejects(() => open(fromBuffer(enc(DOC), { chunkBytes: 0.5 })), /chunkBytes must be a positive integer/)
})

test('source_rejects_zero_chunk_bytes', async () => {
  await assert.rejects(() => open(fromBuffer(enc(DOC), { chunkBytes: 0 })), /chunkBytes must be a positive integer/)
})

test('source_rejects_chunk_bytes_not_multiple_of_64', async () => {
  await assert.rejects(() => open(fromBuffer(enc(DOC), { chunkBytes: 100 })), /chunkBytes must be a multiple of 64/)
})

test('source_rejects_invalid_size_in_facade', async () => {
  const withSize = (size: number): Source => ({
    open: () => Promise.resolve({ size, read: () => Promise.resolve(new Uint8Array()) }),
  })
  await assert.rejects(() => open(withSize(Number.NaN)), /source size must be a non-negative integer, got NaN/)
  await assert.rejects(
    () => open(withSize(Number.POSITIVE_INFINITY)),
    /source size must be a non-negative integer, got Infinity/,
  )
  await assert.rejects(() => open(withSize(-1)), /source size must be a non-negative integer, got -1/)
  await assert.rejects(() => open(withSize(1.5)), /source size must be a non-negative integer, got 1.5/)
})

// fromHttpRange isn't exercised by the memory/file specs. Mocking `globalThis.fetch`
// covers the HTTP protocol surface without a real server.

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
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice')
  assert.equal(await cursor.get('meta', 'enabled'), true)
})

test('source_from_http_range_rejects_when_accept_ranges_missing', async (t) => {
  const restore = mockFetch(
    () =>
      new Response(null, {
        status: 200,
        headers: { 'content-length': '100' }, // no accept-ranges advertised
      }),
  )
  t.after(restore)
  await assert.rejects(() => open(fromHttpRange('https://example.test/doc.json')), /does not advertise Accept-Ranges/)
})

test('source_from_http_range_rejects_when_head_not_ok', async (t) => {
  const restore = mockFetch(() => new Response(null, { status: 404, statusText: 'Not Found' }))
  t.after(restore)
  await assert.rejects(() => open(fromHttpRange('https://example.test/doc.json')), /HEAD .* failed: 404/)
})

test('source_from_http_range_rejects_when_get_returns_200_ignoring_range', async (t) => {
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
  await assert.rejects(() => cursor.get('users', 0, 'name'), /ignored Range and returned 200/)
})

test('source_zero_byte_read_errors_instead_of_hanging', async () => {
  // A read() that returns 0 bytes for an in-bounds, positive-length request is a
  // contract violation, not EOF. The scan must surface it rather than re-faulting
  // the same offset forever. The timeout race turns a regression (hang) into a
  // failure instead of stalling the suite.
  const source: Source = {
    open: () =>
      Promise.resolve({
        size: 1024, // declares bytes that read() never delivers
        read: () => Promise.resolve(new Uint8Array()),
      }),
  }
  const cursor = await open(source)
  const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error('hung: query never settled')), 3000))
  await assert.rejects(() => Promise.race([cursor.get('a'), timeout]), /returned 0 bytes/)
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
  assert.equal(await cursor.get('users', 0, 'name'), 'Alice')
  await cursor.close()
  await cursor.close()
  assert.equal(closeCalls, 1)
})

test('lifecycle_use_after_close_throws_uniformly', async () => {
  // close() invalidates the cursor for every method, regardless of source - a
  // single defined contract, not the source-dependent behavior of the raw reader
  // (fromBuffer reads would keep working; a file read would throw an opaque I/O
  // error). Sub-cursors from hop share the same closed state.
  const cursor = await open(fromBuffer(enc(DOC)))
  const child = await cursor.hop('meta')
  assert.ok(child)
  await cursor.close()

  await assert.rejects(() => cursor.get('users', 0, 'name'), /cursor is closed/)
  await assert.rejects(() => cursor.has('users'), /cursor is closed/)
  await assert.rejects(() => cursor.count('users'), /cursor is closed/)
  await assert.rejects(() => cursor.hop('users'), /cursor is closed/)
  assert.throws(() => cursor.iter('users'), /cursor is closed/)
  // The escaped sub-cursor is invalidated by the root's close too.
  await assert.rejects(() => child.get('version'), /cursor is closed/)
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
    assert.equal(await cursor.get('users', 0, 'name'), 'Alice')
    assert.equal(closeCalls, 0, 'reader stays open inside the scope')
  } finally {
    await cursor[Symbol.asyncDispose]()
  }
  assert.equal(closeCalls, 1, 'scope exit must drive Symbol.asyncDispose -> reader.close')
})

test('lifecycle_open_failure_still_closes_reader', async () => {
  // `open` rejects on `size: NaN` (a non-negative integer is required). It must
  // still drive the reader's `close()` when validation fails after
  // `source.open()` has succeeded - otherwise a failed open leaks a file handle.
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

test('lifecycle_cleanup_failure_does_not_mask_open_error', async () => {
  // openNative rejects (size: NaN) AND reader.close() also throws. The caller
  // must still see the original open failure, not the cleanup error - the close
  // failure rides along as `.cause`.
  const source: Source = {
    open: () =>
      Promise.resolve({
        size: Number.NaN,
        read: () => Promise.resolve(new Uint8Array()),
        close: async () => {
          throw new Error('close blew up')
        },
      }),
  }
  await assert.rejects(
    () => open(source),
    (err: unknown) => {
      assert.ok(err instanceof Error)
      assert.doesNotMatch(err.message, /close blew up/, 'primary error must not be the cleanup error')
      assert.match((err.cause as Error)?.message ?? '', /close blew up/, 'cleanup error attached as cause')
      return true
    },
  )
})
