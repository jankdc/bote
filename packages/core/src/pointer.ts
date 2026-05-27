// RFC 6901 JSON Pointer Static Typing Validator

type ValidateTokenChars<S extends string> = S extends `${string}~${infer Rest}`
  ? Rest extends `0${infer After}` | `1${infer After}`
    ? ValidateTokenChars<After>
    : false
  : true

type ValidateTokens<S extends string> = S extends `${infer Token}/${infer Rest}`
  ? ValidateTokenChars<Token> extends true
    ? ValidateTokens<Rest>
    : false
  : ValidateTokenChars<S>

type IsPointerLiteral<S extends string> = S extends '' ? true : S extends `/${infer Rest}` ? ValidateTokens<Rest> : false

export type PointerLiteral<S extends string> = IsPointerLiteral<S> extends true ? S : `Error: invalid JSON pointer "${S}"`

export type Pointer = string & { readonly __pointer: unique symbol }

/**
 * Brand a dynamically-built `string` as a JSON Pointer so it can be passed
 * anywhere a pointer is accepted. This is an opaque, unchecked assertion - it
 * deliberately does no work and exists only to mark the dynamic boundary; the
 * native parser (RFC 6901) is the single source of truth and rejects a
 * malformed pointer when it is resolved. Prefer the compile-time-checked
 * literal happy path (`PointerLiteral<S>`); reach for this only when a pointer is
 * built from `string`-typed parts.
 */
export function pointer(s: string): Pointer {
  return s as Pointer
}
