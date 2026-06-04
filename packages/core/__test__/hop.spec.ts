import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open } from '../src/index.ts'
import { memorySource, enc } from './fixtures.ts'

test('hop_into_object_resolves_relatives_against_anchor', async () => {
  const cursor = await open(memorySource(enc('{"meta":{"version":"v2","enabled":true}}')))
  const meta = await cursor.hop('meta')
  assert.ok(meta)
  assert.equal(await meta.get('version'), 'v2')
  assert.equal(await meta.get('enabled'), true)
  assert.deepEqual(await meta.get(), { version: 'v2', enabled: true })
})

test('hop_to_array_element_anchors_at_value', async () => {
  const cursor = await open(memorySource(enc('{"users":[{"name":"Alice"},{"name":"Bob"}]}')))
  const bob = await cursor.hop('users', 1)
  assert.ok(bob)
  assert.equal(await bob.get('name'), 'Bob')
})

test('hop_missing_path_returns_null', async () => {
  const cursor = await open(memorySource(enc('{"users":[1,2]}')))
  assert.equal(await cursor.hop('missing'), null)
  assert.equal(await cursor.hop('users', 5), null)
  // A scalar isn't a container, but hop still anchors at it; descending misses.
  const n = await cursor.hop('users', 0)
  assert.ok(n)
  assert.equal(await n.get(), 1)
})

test('hop_chains_relative_to_the_previous_hop', async () => {
  const data = enc('{"a":{"b":{"c":[10,20,30]}}}')
  const cursor = await open(memorySource(data))
  const b = await cursor.hop('a', 'b')
  assert.ok(b)
  const c = await b.hop('c')
  assert.ok(c)
  assert.equal(await c.count(), 3)
  assert.equal(await c.get(2), 30)
})

test('hop_empty_path_anchors_at_cursor', async () => {
  const cursor = await open(memorySource(enc('{"x":1,"y":2}')))
  const self = await cursor.hop()
  assert.ok(self)
  assert.equal(await self.get('x'), 1)
  // Re-anchoring a sub-cursor on its own value lands back on that value.
  const x = await cursor.hop('x')
  assert.ok(x)
  const again = await x.hop()
  assert.ok(again)
  assert.equal(await again.get(), 1)
})

test('hop_supports_iter_and_walk_from_the_anchor', async () => {
  const data = enc('{"orders":[{"id":"a"},{"id":"b"},{"id":"c"}],"meta":{"a":1,"b":2}}')
  const cursor = await open(memorySource(data))
  const orders = await cursor.hop('orders')
  assert.ok(orders)
  const ids: unknown[] = []
  for await (const batch of orders.iter()) ids.push(...batch)
  assert.deepEqual(ids, [{ id: 'a' }, { id: 'b' }, { id: 'c' }])
  const meta = await cursor.hop('meta')
  assert.ok(meta)
  const keys: string[] = []
  for await (const [key] of meta.walk()) keys.push(key)
  assert.deepEqual(keys, ['a', 'b'])
})

test('hop_crosses_chunk_boundaries', async () => {
  const items = Array.from({ length: 100 }, (_, i) => `{"id":${i},"name":"item-${i}"}`)
  const data = enc('{"items":[' + items.join(',') + ']}')
  const cursor = await open(memorySource(data, 128))
  const item = await cursor.hop('items', 73)
  assert.ok(item)
  assert.equal(await item.get('id'), 73)
  assert.equal(await item.get('name'), 'item-73')
})

test('hop_rejects_invalid_path_segments', async () => {
  const cursor = await open(memorySource(enc('{"a":1}')))
  await assert.rejects(() => cursor.hop(-1 as unknown as string), TypeError)
})
