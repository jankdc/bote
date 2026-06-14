import type { Cursor as NativeCursor } from '@botejs/native';

import { deserializeNativeError, ClosedCursorError, MalformedJsonError } from './error.ts';
import { validatePath, type Path, type Segment } from './path.ts';
import { makeStream, type IterStream } from './stream.ts';

import { runStandardSchema, validateItem, type StandardSchemaV1 } from './validate.ts';

import {
  splitArgs,
  isSchema,
  serializeSelect,
  normalizeIterTail,
  type IterOptions,
  type VariadicPathArgs,
} from './args.ts';

type InferOutput<Sch> = Sch extends StandardSchemaV1<unknown, infer O> ? O : never;

type SelectMapShape<S> = { -readonly [K in keyof S]: unknown };

export type IterKey = string | number;

export const DEFAULT_ITER_BATCH = 1000;
export const MAX_ITER_BATCH = 1_000_000;

export interface Cursor {
  /**
   * Resolve `path` to a container and return a new cursor anchored there, or
   * `null` if it is absent. Child cursors share the root's source and lifetime;
   * closing the root closes them too.
   *
   * @example
   * const user = await root.hop('users', 0);
   * const name = await user?.get('name');
   */
  hop(...path: Segment[]): Promise<Cursor | null>;

  /**
   * Report whether a value exists at `path`. With a trailing Standard Schema,
   * also require the value to validate against it (a parse/validation miss
   * yields `false` rather than throwing).
   *
   * @example
   * await root.has('users', 0, 'email');
   * await root.has('users', 0, 'age', z.number());
   */
  has(...path: Segment[]): Promise<boolean>;
  has(...args: [...Segment[], StandardSchemaV1]): Promise<boolean>;

  /**
   * Read and decode the value at `path`, or `undefined` if absent. With a
   * trailing Standard Schema, validate and return its parsed output, throwing
   * on failure.
   *
   * @example
   * const name = await root.get('users', 0, 'name');
   * const age = await root.get('users', 0, 'age', z.number());
   */
  get(...path: Segment[]): Promise<unknown>;
  get<Sch extends StandardSchemaV1>(...args: [...Segment[], Sch]): Promise<InferOutput<Sch>>;

  /**
   * Stream the members of the array or object at `path` as an async iterable.
   * A trailing Standard Schema validates each item; a trailing {@link IterOptions}
   * object tunes the iteration (see its fields for the available knobs).
   *
   * @example
   * for await (const user of root.iter('users')) {
   *   console.log(user);
   * }
   *
   * for await (const [i, name] of root.iter('users', { withKey: true, select: ['name'] })) {
   *   console.log(i, name);
   * }
   */
  iter(...path: Segment[]): IterStream<unknown>;
  iter<Sch extends StandardSchemaV1>(...args: [...Segment[], Sch]): IterStream<InferOutput<Sch>>;
  iter<Sch extends StandardSchemaV1>(
    ...args: [...Segment[], IterOptions & { withKey: true; schema: Sch }]
  ): IterStream<[IterKey, InferOutput<Sch>]>;
  iter<Sch extends StandardSchemaV1>(
    ...args: [...Segment[], IterOptions & { schema: Sch }]
  ): IterStream<InferOutput<Sch>>;
  iter<S extends Record<string, Segment | Path>>(
    ...args: [...Segment[], IterOptions & { withKey: true; select: S }]
  ): IterStream<[IterKey, SelectMapShape<S>]>;
  iter<S extends Record<string, Segment | Path>>(
    ...args: [...Segment[], IterOptions & { select: S }]
  ): IterStream<SelectMapShape<S>>;
  iter(...args: [...Segment[], IterOptions & { withKey: true }]): IterStream<[IterKey, unknown]>;
  iter(...args: [...Segment[], IterOptions]): IterStream<unknown>;
}

export interface RootCursor extends Cursor, AsyncDisposable {
  /** Close the underlying source. Idempotent. */
  close(): Promise<void>;
}

export type CursorState = { closed: boolean };

export function wrap(native: NativeCursor, state: CursorState): Cursor {
  const cursor = {
    async hop(...path: Segment[]): Promise<Cursor | null> {
      ensureOpen(state);
      validatePath(path);
      const child = await withPath(path, () => native.hop(path));
      return child ? wrap(child, state) : null;
    },
    async has(...args: VariadicPathArgs<StandardSchemaV1>): Promise<boolean> {
      ensureOpen(state);
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args);
      if (schema !== undefined && !isSchema(schema)) {
        throw new TypeError('has: expected a Standard Schema as the trailing argument');
      }
      if (!schema) {
        return withPath(path, () => native.has(path));
      }
      if (!(await withPath(path, () => native.has(path)))) {
        return false;
      }
      const text = await withPath(path, () => native.get(path));
      const value = text === undefined ? undefined : parseValue(text, path);
      const result = await validateItem(schema, value, path, 'skip');
      return !('skip' in result);
    },
    async get(...args: VariadicPathArgs<StandardSchemaV1>): Promise<unknown> {
      ensureOpen(state);
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args);
      if (schema !== undefined && !isSchema(schema)) {
        throw new TypeError('get: expected a Standard Schema as the trailing argument');
      }
      const text = await withPath(path, () => native.get(path));
      const value = text === undefined ? undefined : parseValue(text, path);
      if (!schema) {
        return value;
      }
      return runStandardSchema(schema, value, path);
    },
    iter(...args: VariadicPathArgs<StandardSchemaV1 | IterOptions>): IterStream<unknown> {
      ensureOpen(state);
      const { path, tail } = splitArgs<StandardSchemaV1 | IterOptions>(args);
      const { schema, select, batch, onInvalid, withKey } = normalizeIterTail(tail);
      if (batch !== undefined && (!Number.isInteger(batch) || batch <= 0 || batch > MAX_ITER_BATCH)) {
        throw new RangeError(`iter: batch must be an integer in 1..=${MAX_ITER_BATCH}, got ${batch}`);
      }
      if (withKey !== undefined && typeof withKey !== 'boolean') {
        throw new TypeError(`iter: withKey must be a boolean, got ${typeof withKey}`);
      }
      if (onInvalid !== undefined && onInvalid !== 'throw' && onInvalid !== 'skip') {
        throw new RangeError(`iter: onInvalid must be "throw" or "skip", got ${JSON.stringify(onInvalid)}`);
      }

      const resolvedBatch = batch ?? DEFAULT_ITER_BATCH;
      const selectIr = select !== undefined ? serializeSelect(select) : undefined;
      const wantKey = withKey ?? false;
      const nativeWithKey = wantKey || schema !== undefined;
      const inner = native.iter(path, { selectIr, batch: resolvedBatch, withKey: nativeWithKey });

      if (!schema) {
        return nativeStream(inner, path, resolvedBatch, (raw) => parseValue(raw, path) as unknown[]);
      }
      const policy = onInvalid ?? 'throw';
      return nativeStream(inner, path, resolvedBatch, async (raw) => {
        const out: unknown[] = [];
        for (const [key, value] of parseValue(raw, path) as Array<[IterKey, unknown]>) {
          const result = await validateItem(schema, value, [...path, key], policy);
          if ('skip' in result) {
            continue;
          }
          out.push(wantKey ? [key, result.value] : result.value);
        }
        return out;
      });
    },
  };

  return cursor as Cursor;
}

export function ensureOpen(state: CursorState): void {
  if (state.closed) {
    throw new ClosedCursorError();
  }
}

/** Run a native call, retyping any addon error as the matching {@link BoteError}
 *  anchored to `path`. The single funnel every cursor operation passes through,
 *  so native faults surface uniformly. */
async function withPath<T>(path: Path, op: () => Promise<T>): Promise<T> {
  try {
    return await op();
  } catch (err) {
    throw deserializeNativeError(err, path);
  }
}

function nativeStream(
  inner: AsyncIterable<string>,
  path: Path,
  batchSize: number,
  mapBatch: (raw: string) => unknown[] | Promise<unknown[]>,
): IterStream<unknown> {
  async function* batches(): AsyncGenerator<unknown[]> {
    try {
      for await (const raw of inner) {
        yield await mapBatch(raw);
      }
    } catch (err) {
      throw deserializeNativeError(err, path);
    }
  }
  return makeStream(batches, batchSize);
}

function parseValue(text: string, path: Path): unknown {
  try {
    return JSON.parse(text);
  } catch (cause) {
    throw new MalformedJsonError(path, 'malformed_json', { cause });
  }
}
