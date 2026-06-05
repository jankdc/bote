// Node 18 and Node 20.3 predate `Symbol.asyncDispose`; mirror what TS emits for
// `await using` so the well-known symbol is available across our engine range.
if (!(Symbol as { asyncDispose?: symbol }).asyncDispose) {
  ;(Symbol as { asyncDispose?: symbol }).asyncDispose = Symbol.for('Symbol.asyncDispose')
}

export { type IterOptions } from './args.ts'

export {
  ValidationError,
  PathError,
  formatPath,
  type Path,
  type PathFaultCode,
  type Segment,
  type StandardSchemaV1,
} from './validate.ts'

export {
  open,
  DEFAULT_ITER_BATCH,
  MAX_ITER_BATCH,
  type Cursor,
  type RootCursor,
  type OpenOptions,
  type WalkEntry,
  type IterIndex as IterKey,
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
