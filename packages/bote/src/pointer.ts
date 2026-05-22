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

type IsJsonPointer<S extends string> = S extends '' ? true : S extends `/${infer Rest}` ? ValidateTokens<Rest> : false

export type JsonPointer<S extends string> = IsJsonPointer<S> extends true ? S : `Error: invalid JSON pointer "${S}"`
