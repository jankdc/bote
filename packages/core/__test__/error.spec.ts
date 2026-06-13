import { test } from 'node:test';
import assert from 'node:assert/strict';

import { DOC, enc } from './fixtures.ts';

import {
  open,
  fromBuffer,
  BoteError,
  PathError,
  ValidationError,
  MalformedJsonError,
  SourceReadError,
  ClosedCursorError,
  type SeekableSource,
  type StandardSchemaV1,
} from '../src/index.ts';

/** A source whose every `read` rejects, to drive the I/O fault path. */
function failingSource(message: string): SeekableSource {
  return {
    seekable: true,
    open: () => Promise.resolve({ seekable: true, size: 64, read: () => Promise.reject(new Error(message)) }),
  };
}

const numberSchema: StandardSchemaV1<unknown, number> = {
  '~standard': {
    version: 1,
    vendor: 'test',
    validate: (value) => (typeof value === 'number' ? { value } : { issues: [{ message: 'expected number' }] }),
  },
};

test('error_every_bote_error_extends_BoteError_and_carries_a_code', async (t) => {
  const cursor = await open(fromBuffer(enc('{"a": 1, "s": "x"}')));
  t.after(() => cursor.close());

  const path = await cursor.get('a', 'b').catch((e) => e);
  assert.ok(path instanceof PathError);
  assert.ok(path instanceof BoteError);
  assert.equal(path.code, 'through_scalar');

  const validation = await cursor.get('s', numberSchema).catch((e) => e);
  assert.ok(validation instanceof ValidationError);
  assert.ok(validation instanceof BoteError);
  assert.equal(validation.code, 'validation');
});

test('error_malformed_value_throws_MalformedJsonError_anchored_to_path', async (t) => {
  // `0123` is a locatable span but not valid JSON; the facade decode rejects it.
  const cursor = await open(fromBuffer(enc('{"a": 0123}')));
  t.after(() => cursor.close());

  const err = await cursor.get('a').catch((e) => e);
  assert.ok(err instanceof MalformedJsonError);
  assert.ok(err instanceof BoteError);
  assert.equal(err.code, 'malformed_json');
  assert.deepEqual(err.path, ['a']);
  assert.match(err.message, /^bote: malformed JSON at a$/);
});

test('error_truncated_input_throws_MalformedJsonError_with_eof_code', async (t) => {
  const cursor = await open(fromBuffer(enc('{"a": "xx')));
  t.after(() => cursor.close());

  const err = await cursor.get('a').catch((e) => e);
  assert.ok(err instanceof MalformedJsonError);
  assert.equal(err.code, 'unexpected_eof');
  assert.match(err.message, /unexpected end of JSON input/);
});

test('error_reader_failure_throws_SourceReadError_with_cause', async () => {
  const cursor = await open(failingSource('disk on fire'));
  const err = await cursor.get('users', 0).catch((e) => e);
  assert.ok(err instanceof SourceReadError);
  assert.ok(err instanceof BoteError);
  assert.equal(err.code, 'source_io');
  assert.deepEqual(err.path, ['users', 0]);
  // The native reason is preserved for debugging via the standard cause chain.
  assert.match(err.message, /disk on fire/);
});

test('error_use_after_close_throws_ClosedCursorError', async () => {
  const cursor = await open(fromBuffer(enc(DOC)));
  await cursor.close();

  const err = await cursor.get('users').catch((e) => e);
  assert.ok(err instanceof ClosedCursorError);
  assert.ok(err instanceof BoteError);
  assert.equal(err.code, 'closed');
  assert.equal(err.message, 'bote: cursor is closed');
});

test('error_native_faults_are_typed_across_every_entry_point', async (t) => {
  // A reader that fails identically for get/has/count/hop/iter, so each surfaces
  // the same typed SourceReadError rather than a bare Error.
  const cursor = await open(failingSource('io down'));
  t.after(() => cursor.close());

  for (const op of [
    () => cursor.get('a'),
    () => cursor.has('a'),
    () => cursor.count('a'),
    () => cursor.hop('a'),
    async () => {
      for await (const _ of cursor.iter('a')) {
        // drain
      }
    },
  ]) {
    const err = await op().catch((e) => e);
    assert.ok(err instanceof SourceReadError, `expected SourceReadError, got ${err?.constructor?.name}`);
    assert.equal(err.code, 'source_io');
  }
});

test('error_argument_mistakes_stay_native_TypeError_RangeError', async (t) => {
  const cursor = await open(fromBuffer(enc(DOC)));
  t.after(() => cursor.close());

  // Bad path segment: programmer error, not a BoteError.
  await assert.rejects(
    () => cursor.get({} as never),
    (e) => e instanceof TypeError && !(e instanceof BoteError),
  );
  // Out-of-range option: RangeError, not a BoteError.
  assert.throws(
    () => cursor.iter('users', { batch: -1 }),
    (e) => e instanceof RangeError && !(e instanceof BoteError),
  );
});
