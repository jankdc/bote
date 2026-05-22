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
