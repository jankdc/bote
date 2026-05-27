import { fromBuffer, type Source } from '../src/index.ts'

export function memorySource(data: Uint8Array, chunkBytes?: number): Source {
  return fromBuffer(data, chunkBytes === undefined ? undefined : { chunkBytes })
}

export const enc = (s: string): Uint8Array => new TextEncoder().encode(s)

// A small, fully-valid users + meta document.
export const DOC = JSON.stringify({
  users: [
    { id: 1, name: 'Alice', tags: ['admin', 'staff'] },
    { id: 2, name: 'Bob', tags: ['guest'] },
  ],
  meta: { version: 'v2', enabled: true },
})

// Same shape as DOC, but the third user's `id` is a string, so a User schema
// fails at `/users/2`.
export const USERS_WITH_INVALID = JSON.stringify({
  users: [
    { id: 1, name: 'Alice', tags: ['admin', 'staff'] },
    { id: 2, name: 'Bob', tags: ['guest'] },
    { id: 'oops', name: 'Carol', tags: [] },
  ],
  meta: { version: 'v2', enabled: true },
})

// Homogeneous rows where the third row's `n` is non-numeric, so a `{ n: number }`
// schema fails at `/rows/2`.
export const MIXED = JSON.stringify({ rows: [{ n: 1 }, { n: 2 }, { n: 'bad' }, { n: 4 }] })

// Orders used by the where-filtering and scan select/batch tests. `e` is the
// lone `pending` row and the only total over the others' range.
export const ORDERS = JSON.stringify({
  orders: [
    { id: 'a', status: 'paid', total: 120, customer: { country: 'US' } },
    { id: 'b', status: 'refunded', total: 80, customer: { country: 'GB' } },
    { id: 'c', status: 'paid', total: 50, customer: { country: 'US' } },
    { id: 'd', status: 'paid', total: 200, customer: { country: 'DE' } },
    { id: 'e', status: 'pending', total: 999, customer: { country: 'US' } },
  ],
})

// A `{"k0000":0,...}` object with `count` keys, for cache-pressure tests.
export function bigObject(count: number): string {
  const parts = ['{']
  for (let i = 0; i < count; i++) {
    if (i > 0) parts.push(',')
    parts.push(`"k${String(i).padStart(4, '0')}":${i}`)
  }
  parts.push('}')
  return parts.join('')
}
