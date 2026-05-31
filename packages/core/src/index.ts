// Node 18 and Node 20.3 predate `Symbol.asyncDispose`; mirror what TS emits for
// `await using` so the well-known symbol is available across our engine range.
if (!(Symbol as { asyncDispose?: symbol }).asyncDispose) {
  ;(Symbol as { asyncDispose?: symbol }).asyncDispose = Symbol.for('Symbol.asyncDispose')
}

export {
  type IterOptions
} from './args.ts'

export {
  ValidationError,
  formatPath,
  type Path,
  type Segment,
  type StandardSchemaV1
} from './validate.ts'

export {
  open,
  DEFAULT_ITER_BATCH,
  type Cursor,
  type RootCursor,
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
