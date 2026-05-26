import { test } from 'node:test'
import assert from 'node:assert/strict'

import { open, fromBuffer, ValidationError, type Source, type StandardSchemaV1 } from '../src/index.ts'

function memorySource(data: Uint8Array): Source {
  return fromBuffer(data)
}
const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

// rows[2] has a non-number `n`, so it fails `numberN`.
const MIXED = JSON.stringify({ rows: [{ n: 1 }, { n: 2 }, { n: 'bad' }, { n: 4 }] })

function numberN(): StandardSchemaV1<unknown, { n: number }> {
  return {
    '~standard': {
      version: 1,
      vendor: 'test',
      validate(value) {
        const o = value as Record<string, unknown>
        if (typeof o?.n !== 'number') return { issues: [{ message: 'n must be a number' }] }
        return { value: { n: o.n } }
      },
    },
  }
}

function asyncNumberN(): StandardSchemaV1<unknown, { n: number }> {
  return {
    '~standard': {
      version: 1,
      vendor: 'test',
      async validate(value) {
        await Promise.resolve()
        const o = value as Record<string, unknown>
        if (typeof o?.n !== 'number') return { issues: [{ message: 'n must be a number' }] }
        return { value: { n: o.n } }
      },
    },
  }
}

test('scan_schema_throws_on_invalid_by_default', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const seen: number[] = []
  await assert.rejects(
    async () => {
      for await (const row of db.scan('/rows', numberN())) seen.push(row.n)
    },
    (err: unknown) => {
      assert.ok(err instanceof ValidationError)
      assert.equal(err.pointer, '/rows/2')
      return true
    },
  )
  assert.deepEqual(seen, [1, 2]) // yields valid items, then throws on the third
})

test('scan_schema_skip_filters_invalid', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const ns: number[] = []
  for await (const row of db.scan('/rows', { schema: numberN(), onInvalid: 'skip' })) ns.push(row.n)
  assert.deepEqual(ns, [1, 2, 4]) // the invalid row is dropped, not thrown
})

test('scan_schema_skip_async_validator', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const ns: number[] = []
  for await (const row of db.scan('/rows', { schema: asyncNumberN(), onInvalid: 'skip' })) ns.push(row.n)
  assert.deepEqual(ns, [1, 2, 4])
})

test('scan_schema_skip_with_select_validates_projected_shape', async (t) => {
  // select reshapes each child to { n }, then the schema validates that shape.
  const doc = JSON.stringify({ rows: [{ v: 1 }, { v: 'bad' }, { v: 3 }] })
  const db = await open(memorySource(enc(doc)))
  t.after(() => db.close())
  const ns: number[] = []
  for await (const row of db.scan('/rows', { select: { n: '/v' }, schema: numberN(), onInvalid: 'skip' })) {
    ns.push(row.n)
  }
  assert.deepEqual(ns, [1, 3])
})

test('scan_schema_batch_skip_shrinks_batches', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  const batches: number[][] = []
  for await (const b of db.scan('/rows', { schema: numberN(), onInvalid: 'skip', batch: 2 })) {
    batches.push(b.map((r) => r.n))
  }
  // native batches the raw rows [1,2],[bad,4]; skip drops `bad` -> [[1,2],[4]]
  assert.deepEqual(batches, [[1, 2], [4]])
})

test('scan_schema_batch_throws_on_invalid_by_default', async (t) => {
  const db = await open(memorySource(enc(MIXED)))
  t.after(() => db.close())
  await assert.rejects(async () => {
    for await (const _b of db.scan('/rows', { schema: numberN(), batch: 2 })) {
      // consume
    }
  }, ValidationError)
})
