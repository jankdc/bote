// Node 18 and Node 20.3 predate `Symbol.asyncDispose`; mirror what TS emits for
// `await using` so the well-known symbol is available across our engine range.
if (!(Symbol as { asyncDispose?: symbol }).asyncDispose) {
  ;(Symbol as { asyncDispose?: symbol }).asyncDispose = Symbol.for('Symbol.asyncDispose')
}

export type { CacheStats } from '@botejs/native'
export type { JsonPointer } from './pointer.ts'
export { ValidationError, type StandardSchemaV1 } from './validate.ts'
export { eq, lt, lte, gt, gte, exists, and, type Predicate } from './predicate.ts'
export { open, type Cursor, type RootCursor, type ScanOptions, type SessionOptions } from './open.ts'

export {
  fromBuffer,
  fromFile,
  fromHttpRange,
  type FactoryOptions,
  type Source,
  type SourceReader,
  type HttpRangeOptions,
} from './sources.ts'
