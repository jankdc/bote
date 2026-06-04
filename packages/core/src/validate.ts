import type { StandardSchemaV1 } from '@standard-schema/spec'

export type { StandardSchemaV1 }

export type Segment = string | number
export type Path = readonly Segment[]

export class ValidationError extends Error {
  readonly issues: readonly StandardSchemaV1.Issue[]
  readonly path: Path

  constructor(issues: readonly StandardSchemaV1.Issue[], path: Path) {
    super(`bote: schema validation failed at ${formatPath(path)}: ${issues[0]?.message ?? 'unknown'}`)
    this.name = 'ValidationError'
    this.issues = issues
    this.path = path
  }
}

export class PathError extends Error {
  readonly path: Path

  constructor(reason: string, path: Path) {
    super(`bote: cannot resolve ${formatPath(path)}: ${reason}`)
    this.name = 'PathError'
    this.path = path
  }
}

export async function runStandardSchema<O>(
  schema: StandardSchemaV1<unknown, O>,
  value: unknown,
  path: Path,
): Promise<O> {
  const result = await schema['~standard'].validate(value)
  if (result.issues) throw new ValidationError(result.issues, path)
  return result.value
}

export async function validateItem<O>(
  schema: StandardSchemaV1<unknown, O>,
  value: unknown,
  path: Path,
  onInvalid: 'throw' | 'skip',
): Promise<{ skip: true } | { value: O }> {
  const result = await schema['~standard'].validate(value)
  if (result.issues) {
    if (onInvalid === 'skip') return { skip: true }
    throw new ValidationError(result.issues, path)
  }
  return { value: result.value }
}

export function formatPath(path: Path): string {
  if (path.length === 0) return '(root)'
  let out = ''
  for (let i = 0; i < path.length; i++) {
    const seg = path[i]
    if (typeof seg === 'number') {
      out += `[${seg}]`
      continue
    }
    if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(seg)) {
      out += i === 0 ? seg : `.${seg}`
    } else {
      out += `[${JSON.stringify(seg)}]`
    }
  }
  return out
}
