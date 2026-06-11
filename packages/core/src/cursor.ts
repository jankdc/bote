import type { Cursor as NativeCursor } from '@botejs/native';

import { validatePath } from './path.ts';
import { parseValue, deserializeError } from './decode.ts';
import { makeStream, type IterStream } from './stream.ts';

import { runStandardSchema, validateItem, type Path, type Segment, type StandardSchemaV1 } from './validate.ts';

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
  hop(...path: Segment[]): Promise<Cursor | null>;

  has(...path: Segment[]): Promise<boolean>;
  has(...args: [...Segment[], StandardSchemaV1]): Promise<boolean>;

  get(...path: Segment[]): Promise<unknown>;
  get<Sch extends StandardSchemaV1>(...args: [...Segment[], Sch]): Promise<InferOutput<Sch>>;

  count(...path: Segment[]): Promise<number>;

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

/** Throw a uniform error for any operation on a closed cursor, so use-after-close
 *  is one defined contract regardless of source (some readers' reads keep working
 *  after close, others throw an opaque I/O error). */
export function ensureOpen(state: CursorState): void {
  if (state.closed) {
    throw new Error('bote: cursor is closed');
  }
}

export function wrap(native: NativeCursor, state: CursorState): Cursor {
  const cursor = {
    async hop(...path: Segment[]): Promise<Cursor | null> {
      ensureOpen(state);
      validatePath(path);
      let child: NativeCursor | null;
      try {
        child = await native.hop(path);
      } catch (err) {
        throw deserializeError(err, path);
      }
      return child ? wrap(child, state) : null;
    },
    async has(...args: VariadicPathArgs<StandardSchemaV1>): Promise<boolean> {
      ensureOpen(state);
      const { path, tail: schema } = splitArgs<StandardSchemaV1>(args);
      if (schema !== undefined && !isSchema(schema)) {
        throw new TypeError('has: expected a Standard Schema as the trailing argument');
      }
      if (!schema) {
        return native.has(path);
      }
      if (!(await native.has(path))) {
        return false;
      }
      const text = await native.get(path);
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
      let value: unknown;
      try {
        const text = await native.get(path);
        value = text === undefined ? undefined : parseValue(text, path);
      } catch (err) {
        throw deserializeError(err, path);
      }
      if (!schema) {
        return value;
      }
      return runStandardSchema(schema, value, path);
    },
    async count(...path: Segment[]): Promise<number> {
      ensureOpen(state);
      validatePath(path);
      try {
        return await native.count(path);
      } catch (err) {
        throw deserializeError(err, path);
      }
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
      throw deserializeError(err, path);
    }
  }
  return makeStream(batches, batchSize);
}
