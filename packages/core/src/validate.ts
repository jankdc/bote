import type { StandardSchemaV1 } from '@standard-schema/spec'

export type { StandardSchemaV1 }

export class ValidationError extends Error {
  readonly issues: readonly StandardSchemaV1.Issue[]
  readonly pointer: string

  constructor(issues: readonly StandardSchemaV1.Issue[], pointer: string) {
    super(`bote: schema validation failed at ${pointer || '/'}: ${issues[0]?.message ?? 'unknown'}`)
    this.name = 'ValidationError'
    this.issues = issues
    this.pointer = pointer
  }
}

export async function runStandardSchema<O>(
  schema: StandardSchemaV1<unknown, O>,
  value: unknown,
  pointer: string,
): Promise<O> {
  const result = await schema['~standard'].validate(value)
  if (result.issues) throw new ValidationError(result.issues, pointer)
  return result.value
}

/**
 * Validate one item for a stream fold. On failure: `'throw'` raises a
 * `ValidationError`; `'skip'` returns `{ skip: true }` so the caller can drop
 * the item (turning the schema into a filter). On success, returns the typed
 * value.
 */
export async function validateItem<O>(
  schema: StandardSchemaV1<unknown, O>,
  value: unknown,
  pointer: string,
  onInvalid: 'throw' | 'skip',
): Promise<{ skip: true } | { value: O }> {
  const result = await schema['~standard'].validate(value)
  if (result.issues) {
    if (onInvalid === 'skip') return { skip: true }
    throw new ValidationError(result.issues, pointer)
  }
  return { value: result.value }
}
