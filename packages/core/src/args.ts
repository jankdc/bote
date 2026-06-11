import { validatePath } from './path.ts'
import type { Path, Segment, StandardSchemaV1 } from './validate.ts'

export interface IterOptions {
  select?: Segment | Path | Record<string, Segment | Path>
  /** How many items cross the native boundary per fetch, which also bounds the
   *  resident materialization window (the memory knob) and sets the array size
   *  yielded by `IterStream.raw()`. The default item loop drains each fetch
   *  one item at a time, so this doesn't change what item iteration yields, only
   *  how much is fetched and held at once. Higher is faster but holds more in
   *  memory. */
  batch?: number
  /** Validate each yielded item against this schema (after `select`). */
  schema?: StandardSchemaV1
  /** Policy for items failing `schema`. Default `'throw'`; `'skip'` drops them. */
  onInvalid?: 'throw' | 'skip'
  /** Yield `[key, value]` tuples instead of bare values. `key` is the member
   *  name for objects and the zero-based index for arrays. */
  withKey?: boolean
}

export type VariadicPathArgs<TTail> = [...Segment[]] | [...Segment[], TTail]

export function splitArgs<TTail>(args: VariadicPathArgs<TTail>): { path: Segment[]; tail: TTail | undefined } {
  let pathArgs: unknown[]
  let tail: TTail | undefined
  if (args.length === 0) {
    pathArgs = []
    tail = undefined
  } else {
    const last = args[args.length - 1]
    if (last !== null && typeof last === 'object' && !Array.isArray(last)) {
      pathArgs = args.slice(0, -1)
      tail = last as TTail
    } else {
      pathArgs = args as unknown[]
      tail = undefined
    }
  }
  validatePath(pathArgs)
  return { path: pathArgs as Segment[], tail }
}

export function isSchema(value: unknown): value is StandardSchemaV1 {
  return typeof value === 'object' && value !== null && '~standard' in value
}

export function normalizeIterTail(tail: StandardSchemaV1 | IterOptions | undefined): IterOptions {
  if (!tail) return {}
  if (isSchema(tail)) return { schema: tail }
  return tail
}

export function serializeSelect(select: Segment | Path | Record<string, Segment | Path>): string {
  if (typeof select === 'string' || typeof select === 'number') {
    const one = [select]
    validatePath(one)
    return JSON.stringify({ one })
  }
  if (Array.isArray(select)) {
    validatePath(select)
    if (select.length === 0) {
      throw new RangeError('iter: select sub-path must have at least one segment')
    }
    return JSON.stringify({ one: select })
  }
  if (select === null || typeof select !== 'object') {
    throw new TypeError(`iter: select must be a segment, path, or field map, got ${describeSelect(select)}`)
  }
  const entries = Object.entries(select).map(([k, sub]) => {
    const path = typeof sub === 'string' || typeof sub === 'number' ? [sub] : sub
    if (!Array.isArray(path)) {
      throw new TypeError(
        `iter: select field ${JSON.stringify(k)} must be a segment or path, got ${describeSelect(sub)}`,
      )
    }
    validatePath(path)
    if (path.length === 0) {
      throw new RangeError(`iter: select field ${JSON.stringify(k)} sub-path must have at least one segment`)
    }
    return [k, path] as const
  })
  if (entries.length === 0) {
    throw new RangeError('iter: select must have at least one field')
  }
  return JSON.stringify({ map: entries })
}

function describeSelect(value: unknown): string {
  if (value === null) return 'null'
  return Array.isArray(value) ? 'array' : typeof value
}
