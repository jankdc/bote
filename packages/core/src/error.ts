import type { StandardSchemaV1 } from '@standard-schema/spec';
import type { PathFaultCode, JsonFaultCode, SourceFaultCode } from '@botejs/native';

import { formatPath, type Path } from './path.ts';

export type { PathFaultCode, JsonFaultCode, SourceFaultCode };

export type BoteErrorCode = PathFaultCode | JsonFaultCode | SourceFaultCode | 'validation' | 'closed';

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

export class SourceReadError extends BoteError {
  declare readonly code: SourceFaultCode;
  readonly path: Path;

  constructor(path: Path, detail: string, options?: ErrorOptions) {
    super('source_io', `bote: source read failed at ${formatPath(path)}: ${detail}`, options);
    this.name = 'SourceReadError';
    this.path = path;
  }
}

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
    return new SourceReadError(path, detail ?? '', { cause: err });
  }
  return err;
}

const PATH_FAULT_MESSAGE: Record<PathFaultCode, (segment?: number) => string> = {
  wrong_kind: (segment) => `path segment ${segment} does not match the container kind`,
  scalar_target: () => 'target value is not a container',
  through_scalar: (segment) => `path traverses a non-container value at segment ${segment}`,
};
