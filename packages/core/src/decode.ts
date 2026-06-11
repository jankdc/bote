import { PathError, formatPath, type Path, type PathFaultCode } from './path.ts';

const NATIVE_PATH_ERROR = /^bote:path:([a-z_]+)(?::(\d+))?$/;

export function deserializeError(err: unknown, path: Path): unknown {
  if (err instanceof Error && !(err instanceof PathError)) {
    const match = NATIVE_PATH_ERROR.exec(err.message);
    if (match) {
      const segment = match[2] === undefined ? undefined : Number(match[2]);
      return new PathError(path, match[1] as PathFaultCode, segment);
    }
  }
  return err;
}

export function parseValue(text: string, path: Path): unknown {
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`bote: malformed JSON value at ${formatPath(path)}`);
  }
}
