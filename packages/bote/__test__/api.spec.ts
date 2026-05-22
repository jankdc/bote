import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { open, fromBuffer, fromFile, type Source } from '../src/index.ts'

const DOC = JSON.stringify({
  users: [
    { id: 1, name: 'Alice', tags: ['admin', 'staff'] },
    { id: 2, name: 'Bob', tags: ['guest'] },
  ],
  meta: { version: 'v2', enabled: true },
})

test('source_custom_open_and_get', async (t) => {
  const cursor = await open(fromBuffer(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
  assert.equal(await cursor.get('/meta/enabled'), true)
})

test('source_from_buffer_roundtrips_json', async (t) => {
  const cursor = await open(fromBuffer(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  assert.equal(await cursor.get('/users/1/id'), 2)
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
      const data = new TextEncoder().encode(DOC)
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

test('cursor_close_drives_reader_close_exactly_once', async () => {
  let closeCalls = 0
  const source: Source = {
    open: () => {
      const data = new TextEncoder().encode(DOC)
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

test('await_using_cursor_disposes_reader_at_scope_exit', async () => {
  let closeCalls = 0
  const source: Source = {
    open: () => {
      const data = new TextEncoder().encode(DOC)
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
  {
    await using cursor = await open(source)
    assert.equal(await cursor.get('/users/0/name'), 'Alice')
    assert.equal(closeCalls, 0, 'reader stays open inside the scope')
  }
  assert.equal(closeCalls, 1, 'scope exit must drive Symbol.asyncDispose -> reader.close')
})

test('iter_materializes_each_child', async (t) => {
  const cursor = await open(fromBuffer(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  const tags: unknown[] = []
  for await (const v of cursor.iter('/users/0/tags')) tags.push(v)
  assert.deepEqual(tags, ['admin', 'staff'])
})

test('walk_subcursor_key_reflects_parent_step', async (t) => {
  const cursor = await open(fromBuffer(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  const keys: Array<string | number | null> = []
  for await (const user of cursor.walk('/users')) {
    keys.push(user.key)
  }
  assert.deepEqual(keys, [0, 1])
  const memberKeys: Array<string | number | null> = []
  for await (const member of cursor.walk('/meta')) {
    memberKeys.push(member.key)
  }
  assert.deepEqual(memberKeys.sort(), ['enabled', 'version'])
})

test('pointer_rejects_malformed_literals', async (t) => {
  const cursor = await open(fromBuffer(new TextEncoder().encode(DOC)))
  t.after(() => cursor.close())
  // Each line below should be a TS error at the call site; we still await
  // the rejected promise so the runtime check doesn't leak an unhandled
  // rejection if the type validator is ever weakened.
  // @ts-expect-error pointer must start with '/' or be empty
  await assert.rejects(cursor.get('users'))
  // @ts-expect-error '~' must be followed by '0' or '1'
  await assert.rejects(cursor.get('/foo~'))
  // @ts-expect-error '~2' is not a valid escape
  await assert.rejects(cursor.get('/foo~2bar'))
  // Sanity: well-formed pointers compile.
  assert.equal(await cursor.get('/users/0/name'), 'Alice')
})
