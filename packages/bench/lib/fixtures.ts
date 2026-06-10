// Doc builders, temp-doc lifecycle, and the shape/pattern table used by the
// matrix worker. Drivers open these docs through the `@botejs/core` facade
// (`open(fromFile(path))`), so there's no Source adapter here.

import type { Path, Segment } from '@botejs/core'
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'

import { withTempDir } from './tmp.ts'

export type DocShape = 'array-of-objects' | 'object-of-objects' | 'deep-nested' | 'wide-flat'
export type FixturePattern = 'shallow' | 'mid' | 'deep' | 'iter-all' | 'obj-iter' | 'obj-iter-name' | 'obj-iter-first'

export interface DocFixture {
  shape: DocShape
  buf: Uint8Array
  paths: Record<FixturePattern, Path | null>
}

export function buildFixture(shape: DocShape, scale: number, padWidth: number): DocFixture {
  switch (shape) {
    case 'array-of-objects':
      return buildArrayOfObjects(scale, padWidth)
    case 'object-of-objects':
      return buildObjectOfObjects(scale, padWidth)
    case 'deep-nested':
      return buildDeepNested(scale, padWidth)
    case 'wide-flat':
      return buildWideFlat(scale, padWidth)
  }
}

export function buildArrayDoc(n: number, padWidth: number): Uint8Array {
  const parts: string[] = ['{"items":[']
  for (let i = 0; i < n; i++) {
    if (i > 0) parts.push(',')
    parts.push(`{"id":${i},"name":"item-${String(i).padStart(padWidth, '0')}","tags":["a","b"]}`)
  }
  parts.push(']}')
  return new TextEncoder().encode(parts.join(''))
}

export function buildObjectDoc(n: number, padWidth: number): Uint8Array {
  const parts: string[] = ['{"items":{']
  for (let i = 0; i < n; i++) {
    if (i > 0) parts.push(',')
    const k = `item-${String(i).padStart(padWidth, '0')}`
    parts.push(`"${k}":{"id":${i},"name":"${k}","tags":["a","b"]}`)
  }
  parts.push('}}')
  return new TextEncoder().encode(parts.join(''))
}

function buildArrayOfObjects(items: number, padWidth: number): DocFixture {
  return {
    shape: 'array-of-objects',
    buf: buildArrayDoc(items, padWidth),
    paths: {
      shallow: ['items', 0, 'name'],
      mid: ['items', Math.floor(items / 2), 'name'],
      deep: ['items', items - 1, 'name'],
      'iter-all': ['items'],
      'obj-iter': null,
      'obj-iter-name': null,
      'obj-iter-first': null,
    },
  }
}

function buildObjectOfObjects(items: number, padWidth: number): DocFixture {
  const key = (i: number): Path => ['items', `item-${String(i).padStart(padWidth, '0')}`, 'name']
  return {
    shape: 'object-of-objects',
    buf: buildObjectDoc(items, padWidth),
    paths: {
      shallow: key(0),
      mid: key(Math.floor(items / 2)),
      deep: key(items - 1),
      'iter-all': null,
      'obj-iter': ['items'],
      'obj-iter-name': ['items'],
      'obj-iter-first': ['items'],
    },
  }
}

function buildDeepNested(depth: number, padWidth: number): DocFixture {
  let body = `"name":"leaf-${String(depth).padStart(padWidth, '0')}"`
  for (let i = depth - 1; i >= 0; i--) {
    body = `"a":{${body}},"name":"leaf-${String(i).padStart(padWidth, '0')}"`
  }
  const path = (d: number): Path => {
    const out: Segment[] = []
    for (let i = 0; i < d; i++) out.push('a')
    out.push('name')
    return out
  }
  return {
    shape: 'deep-nested',
    buf: new TextEncoder().encode(`{${body}}`),
    paths: {
      shallow: path(0),
      mid: path(Math.floor(depth / 2)),
      deep: path(depth),
      'iter-all': null,
      'obj-iter': null,
      'obj-iter-name': null,
      'obj-iter-first': null,
    },
  }
}

function buildWideFlat(keys: number, padWidth: number): DocFixture {
  const parts: string[] = ['{']
  for (let i = 0; i < keys; i++) {
    if (i > 0) parts.push(',')
    const k = String(i).padStart(padWidth, '0')
    parts.push(`"k_${k}":"v_${k}"`)
  }
  parts.push('}')
  const key = (i: number): Path => [`k_${String(i).padStart(padWidth, '0')}`]
  return {
    shape: 'wide-flat',
    buf: new TextEncoder().encode(parts.join('')),
    paths: {
      shallow: key(0),
      mid: key(Math.floor(keys / 2)),
      deep: key(keys - 1),
      'iter-all': null,
      'obj-iter': [],
      'obj-iter-name': null,
      'obj-iter-first': [],
    },
  }
}

export async function withTempDoc<T>(
  items: number,
  padWidth: number,
  fn: (path: string, buf: Uint8Array) => Promise<T>,
): Promise<T> {
  return withTempBuf(buildArrayDoc(items, padWidth), fn)
}

export async function withTempObjectDoc<T>(
  items: number,
  padWidth: number,
  fn: (path: string, buf: Uint8Array) => Promise<T>,
): Promise<T> {
  return withTempBuf(buildObjectDoc(items, padWidth), fn)
}

async function withTempBuf<T>(buf: Uint8Array, fn: (path: string, buf: Uint8Array) => Promise<T>): Promise<T> {
  return withTempDir('bote-bench-', (dir) => {
    const path = join(dir, 'doc.json')
    writeFileSync(path, buf)
    return fn(path, buf)
  })
}
