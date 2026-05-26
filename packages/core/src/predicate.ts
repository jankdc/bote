// Predicates are data, not callbacks: each constructor builds a node of a
// small IR tree that is serialized to JSON and evaluated natively off the raw
// bytes (see packages/native/src/predicate.rs). The IR shape here mirrors the
// Rust `#[serde(tag = "t")]` enum exactly. `Predicate` is an opaque brand so
// callers can only build predicates through these constructors.

import type { JsonPointer } from './pointer.ts'

export type Predicate = { readonly __brand: 'Predicate' }

type Scalar = string | number | boolean | null

type CompareNode = { t: 'eq' | 'lt' | 'lte' | 'gt' | 'gte'; p: string; v: Scalar }
type ExistsNode = { t: 'exists'; p: string }
type AndNode = { t: 'and'; c: IRNode[] }
type IRNode = CompareNode | ExistsNode | AndNode

const toPredicate = (node: IRNode): Predicate => node as unknown as Predicate
const toNode = (predicate: Predicate): IRNode => predicate as unknown as IRNode

export function eq<S extends string>(pointer: JsonPointer<S>, value: Scalar): Predicate {
  return toPredicate({ t: 'eq', p: pointer as string, v: value })
}

export function lt<S extends string>(pointer: JsonPointer<S>, value: number | string): Predicate {
  return toPredicate({ t: 'lt', p: pointer as string, v: value })
}

export function lte<S extends string>(pointer: JsonPointer<S>, value: number | string): Predicate {
  return toPredicate({ t: 'lte', p: pointer as string, v: value })
}

export function gt<S extends string>(pointer: JsonPointer<S>, value: number | string): Predicate {
  return toPredicate({ t: 'gt', p: pointer as string, v: value })
}

export function gte<S extends string>(pointer: JsonPointer<S>, value: number | string): Predicate {
  return toPredicate({ t: 'gte', p: pointer as string, v: value })
}

export function exists<S extends string>(pointer: JsonPointer<S>): Predicate {
  return toPredicate({ t: 'exists', p: pointer as string })
}

export function and(...predicates: Predicate[]): Predicate {
  const children: IRNode[] = []
  for (const predicate of predicates) {
    const node = toNode(predicate)
    if (node.t === 'and') children.push(...node.c)
    else children.push(node)
  }
  return toPredicate({ t: 'and', c: children })
}

export function serializePredicate(predicate: Predicate): string {
  return JSON.stringify(toNode(predicate))
}
