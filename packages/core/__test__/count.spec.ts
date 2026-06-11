import { test } from 'node:test';
import assert from 'node:assert/strict';

import { open, PathError } from '../src/index.ts';
import { memorySource, enc } from './fixtures.ts';

test('count_array_elements', async (t) => {
  const cursor = await open(memorySource(enc('{"items":[10,20,30,40,50]}')));
  t.after(() => cursor.close());
  assert.equal(await cursor.count('items'), 5);
});

test('count_object_members', async (t) => {
  const cursor = await open(memorySource(enc('{"a":1,"b":2,"c":3}')));
  t.after(() => cursor.close());
  assert.equal(await cursor.count(), 3);
});

test('count_ignores_nested_and_in_string_commas', async (t) => {
  const cursor = await open(memorySource(enc('{"xs":[{"a":[1,2,3]},"c,d,e",[9,9]]}')));
  t.after(() => cursor.close());
  assert.equal(await cursor.count('xs'), 3);
});

test('count_empty_container_is_zero', async (t) => {
  const cursor = await open(memorySource(enc('{"items":[],"obj":{}}')));
  t.after(() => cursor.close());
  assert.equal(await cursor.count('items'), 0);
  assert.equal(await cursor.count('obj'), 0);
});

test('count_missing_is_zero', async (t) => {
  // A clean miss (missing key, OOB index on a real array) counts as 0.
  const cursor = await open(memorySource(enc('{"a":1,"xs":[1,2]}')));
  t.after(() => cursor.close());
  assert.equal(await cursor.count('missing'), 0);
  assert.equal(await cursor.count('xs', 9), 0);
});

test('count_present_scalar_throws_PathError', async (t) => {
  // A container operation aimed at a present scalar is a shape error, not 0.
  const cursor = await open(memorySource(enc('{"a":1,"s":"hi"}')));
  t.after(() => cursor.close());
  await assert.rejects(() => cursor.count('a'), PathError);
  await assert.rejects(() => cursor.count('s'), PathError);
});

test('count_through_scalar_throws_PathError', async (t) => {
  const cursor = await open(memorySource(enc('{"a":1}')));
  t.after(() => cursor.close());
  await assert.rejects(() => cursor.count('a', 'b'), PathError);
});

test('count_large_array_under_eviction', async (t) => {
  // count() iterates the whole array under a tight cache cap, so chunks are
  // fetched repeatedly under forward faulting; the tally must stay correct.
  const items = Array.from({ length: 5000 }, (_, i) => `{"id":${i}}`);
  const cursor = await open(memorySource(enc('[' + items.join(',') + ']'), 256));
  t.after(() => cursor.close());
  assert.equal(await cursor.count(), 5000);
});
