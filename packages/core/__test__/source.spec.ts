import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { open, fromBuffer, fromFile, type Source } from '../src/index.ts'
import { DOC, enc } from './fixtures.ts'

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
  for await (const user of cursor.walk('/users')) {
    names.push((await user.get('/name')) as string)
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
        read: async (offset, dst) => {
          const end = Math.min(offset + dst.byteLength, data.length)
          const n = Math.max(0, end - offset)
          if (n > 0) dst.set(data.subarray(offset, end))
          return n
        },
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
        read: async (offset, dst) => {
          const end = Math.min(offset + dst.byteLength, data.length)
          const n = Math.max(0, end - offset)
          if (n > 0) dst.set(data.subarray(offset, end))
          return n
        },
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
        read: async (offset, dst) => {
          const end = Math.min(offset + dst.byteLength, data.length)
          const n = Math.max(0, end - offset)
          if (n > 0) dst.set(data.subarray(offset, end))
          return n
        },
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
