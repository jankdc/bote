import type { StandardSchemaV1 } from '@standard-schema/spec';
import type { PathFaultCode, JsonFaultCode, SourceFaultCode } from '@botejs/native';

import { formatPath, type Path } from './path.ts';

export type { PathFaultCode, JsonFaultCode, SourceFaultCode };

export type BoteErrorCode =
  | PathFaultCode
  | JsonFaultCode
  | SourceFaultCode
  | 'validation'
  | 'closed'
  | 'forward_replay';

/** Base class for every error bote raises from its own logic. Catch this to
 *  catch anything bote throws; branch on {@link BoteError.code} for the precise
 *  kind. Always carries a `bote:`-prefixed message. */
export abstract class BoteError extends Error {
  readonly code: BoteErrorCode;

  constructor(code: BoteErrorCode, message: string, options?: ErrorOptions) {
    super(message, options);
    this.code = code;
    this.name = 'BoteError';
  }
}

/** Raised when a `path` cannot be resolved against the document's actual shape -
 *  e.g. it descends through a scalar, or a segment's kind (key vs index) does not
 *  match the container it lands in. `path` is the offending path. */
export class PathError extends BoteError {
  declare readonly code: PathFaultCode;
  readonly path: Path;

  constructor(path: Path, code: PathFaultCode, segment?: number) {
    const reason = (PATH_FAULT_MESSAGE[code] ?? (() => code))(segment);
    super(code, `bote: cannot resolve ${formatPath(path)}: ${reason}`);
    this.name = 'PathError';
    this.path = path;
  }
}

/** Raised when a value fails the Standard Schema passed to `get`/`iter` (and only
 *  when the policy is to throw - `has` and `iter`'s `onInvalid: 'skip'` swallow it).
 *  `issues` carries the schema's validation issues; `path` is where the value lived. */
export class ValidationError extends BoteError {
  declare readonly code: 'validation';
  readonly issues: readonly StandardSchemaV1.Issue[];
  readonly path: Path;

  constructor(issues: readonly StandardSchemaV1.Issue[], path: Path) {
    super('validation', `bote: schema validation failed at ${formatPath(path)}: ${issues[0]?.message ?? 'unknown'}`);
    this.name = 'ValidationError';
    this.issues = issues;
    this.path = path;
  }
}

/** Raised when the bytes at `path` are not valid JSON - either syntactically
 *  malformed or truncated (`unexpected_eof`, e.g. a stream that ended mid-value).
 *  Distinguish the two via {@link BoteError.code}. */
export class MalformedJsonError extends BoteError {
  declare readonly code: JsonFaultCode;
  readonly path: Path;

  constructor(path: Path, code: JsonFaultCode, options?: ErrorOptions) {
    const what = code === 'unexpected_eof' ? 'unexpected end of JSON input' : 'malformed JSON';
    super(code, `bote: ${what} at ${formatPath(path)}`, options);
    this.name = 'MalformedJsonError';
    this.path = path;
  }
}

/** Raised when the underlying source fails to deliver bytes - a file read error,
 *  a failed HTTP fetch, an aborted stream, etc. `detail` (in the message) carries
 *  the backend's reason; the triggering error is attached as `cause`. */
export class SourceReadError extends BoteError {
  declare readonly code: SourceFaultCode;
  readonly path: Path;

  constructor(path: Path, detail: string, options?: ErrorOptions) {
    super('source_io', `bote: source read failed at ${formatPath(path)}: ${detail}`, options);
    this.name = 'SourceReadError';
    this.path = path;
  }
}

/** Raised when a query on a forward-only source needs to re-read bytes before the
 *  point the stream has already passed, and rewinding is forbidden (the default).
 *  `offset` is the byte it wanted; `position` is where the stream had advanced to.
 *  Opt into `rewind: 'replay'` or `'buffer'` (see `fromReadable`), or switch to a
 *  seekable source, to allow the re-read. */
export class ForwardReplayError extends BoteError {
  declare readonly code: 'forward_replay';
  readonly offset: number;
  readonly position: number;

  constructor(offset: number, position: number, options?: ErrorOptions) {
    super(
      'forward_replay',
      `bote: forward source cannot rewind to offset ${offset} from ${position}: the stream has already advanced. ` +
        "Pass { rewind: 'replay' } if the producer is idempotent, { rewind: 'buffer' } to snapshot it in memory, " +
        'or use a seekable source (fromFile/fromBuffer/fromHttpRange) for repeated or out-of-order access.',
      options,
    );
    this.name = 'ForwardReplayError';
    this.offset = offset;
    this.position = position;
  }
}

/** Raised when any operation is attempted on a cursor whose root has been closed
 *  (via `close()` or leaving an `await using` scope). Child cursors share the
 *  root's lifetime, so they throw this too once the root is closed. */
export class ClosedCursorError extends BoteError {
  declare readonly code: 'closed';

  constructor() {
    super('closed', 'bote: cursor is closed');
    this.name = 'ClosedCursorError';
  }
}

/** `bote:<code>[:<detail>]` lines the native addon emits in place of a human
 *  message, so the typed error and its message live on this side only. `<code>`
 *  is a Rust-owned native fault code; `<detail>` is a path fault's offending
 *  segment or a source fault's reason. The code groups below are typed against
 *  the Rust enums, so renaming a code in Rust breaks compilation here. */
const NATIVE_ERROR = /^bote:([a-z_]+)(?::([\s\S]*))?$/;

const PATH_CODES = ['through_scalar', 'scalar_target', 'wrong_kind'] as const satisfies readonly PathFaultCode[];
const JSON_CODES = ['malformed_json', 'unexpected_eof'] as const satisfies readonly JsonFaultCode[];
const SOURCE_CODE: SourceFaultCode = 'source_io';
const FORWARD_REWIND = /forward source cannot rewind to offset (\d+) from (\d+)/;

/** Rebuild a typed {@link BoteError} from a native addon error, anchoring it to
 *  the `path` of the call it surfaced through. Pass-through for anything that
 *  isn't a recognized native error (including errors already typed here). */
export function deserializeNativeError(err: unknown, path: Path): unknown {
  if (!(err instanceof Error) || err instanceof BoteError) {
    return err;
  }
  const match = NATIVE_ERROR.exec(err.message);
  if (!match) {
    return err;
  }
  const code = match[1];
  const detail = match[2];
  if ((PATH_CODES as readonly string[]).includes(code)) {
    const segment = detail === undefined ? undefined : Number(detail);
    return new PathError(path, code as PathFaultCode, segment);
  }
  if ((JSON_CODES as readonly string[]).includes(code)) {
    return new MalformedJsonError(path, code as JsonFaultCode, { cause: err });
  }
  if (code === SOURCE_CODE) {
    // A forward reader rejects its read() with a ForwardReplayError; the native
    // layer can only relay it as a generic source_io fault, so rebuild the typed
    // error from the message it wrapped (offset/position survive in the detail).
    const rewind = FORWARD_REWIND.exec(detail ?? '');
    if (rewind) {
      return new ForwardReplayError(Number(rewind[1]), Number(rewind[2]), { cause: err });
    }
    return new SourceReadError(path, detail ?? '', { cause: err });
  }
  return err;
}

const PATH_FAULT_MESSAGE: Record<PathFaultCode, (segment?: number) => string> = {
  wrong_kind: (segment) => `path segment ${segment} does not match the container kind`,
  scalar_target: () => 'target value is not a container',
  through_scalar: (segment) => `path traverses a non-container value at segment ${segment}`,
};
