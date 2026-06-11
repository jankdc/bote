import type { StandardSchemaV1 } from '@standard-schema/spec';
import { formatPath, type Path } from './path.ts';

export type { StandardSchemaV1 };

export class ValidationError extends Error {
  readonly issues: readonly StandardSchemaV1.Issue[];
  readonly path: Path;

  constructor(issues: readonly StandardSchemaV1.Issue[], path: Path) {
    super(`bote: schema validation failed at ${formatPath(path)}: ${issues[0]?.message ?? 'unknown'}`);
    this.name = 'ValidationError';
    this.issues = issues;
    this.path = path;
  }
}

export async function runStandardSchema<O>(
  schema: StandardSchemaV1<unknown, O>,
  value: unknown,
  path: Path,
): Promise<O> {
  const result = await schema['~standard'].validate(value);
  if (result.issues) {
    throw new ValidationError(result.issues, path);
  }
  return result.value;
}

export async function validateItem<O>(
  schema: StandardSchemaV1<unknown, O>,
  value: unknown,
  path: Path,
  onInvalid: 'throw' | 'skip',
): Promise<{ skip: true } | { value: O }> {
  const result = await schema['~standard'].validate(value);
  if (result.issues) {
    if (onInvalid === 'skip') {
      return { skip: true };
    }
    throw new ValidationError(result.issues, path);
  }
  return { value: result.value };
}
