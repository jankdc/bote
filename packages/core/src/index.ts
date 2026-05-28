// Node 18 and Node 20.3 predate `Symbol.asyncDispose`; mirror what TS emits for
// `await using` so the well-known symbol is available across our engine range.
if (!(Symbol as { asyncDispose?: symbol }).asyncDispose) {
  ;(Symbol as { asyncDispose?: symbol }).asyncDispose = Symbol.for('Symbol.asyncDispose')
}

export type { CacheStats } from '@botejs/native'
export { ValidationError, type StandardSchemaV1 } from './validate.ts'
export { pointer, type PointerLiteral, type Pointer } from './pointer.ts'
export {
  open,
  DEFAULT_ITER_BATCH,
  type Cursor,
  type RootCursor,
  type IterIndex as IterKey,
  type IterOptions,
  type SessionOptions,
} from './open.ts'

export {
  fromBuffer,
  fromFile,
  fromHttpRange,
  type FactoryOptions,
  type Source,
  type SourceReader,
  type HttpRangeOptions,
} from './sources.ts'
