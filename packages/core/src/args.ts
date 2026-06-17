import { validatePath, type Path, type Segment } from './path.ts';
import type { StandardSchemaV1 } from './validate.ts';

/** Trailing options object for `Cursor.iter`, tuning how the iteration yields items. */
export interface IterOptions {
  /** Project each member before it is yielded. A single segment or path picks a
   *  sub-value; a field map (`{ name: 'name', city: ['address', 'city'] }`)
   *  builds an object from several sub-paths. */
  select?: Segment | Path | Record<string, Segment | Path>;
  /** Validate each yielded item against this schema (after `select`). */
  schema?: StandardSchemaV1;
  /** Yield `[key, value]` tuples instead of bare values. `key` is the member
   *  name for objects and the zero-based index for arrays. */
  withKey?: boolean;
  /** Policy for items failing `schema`. Default `'throw'`; `'skip'` drops them. */
  onInvalid?: 'throw' | 'skip';
  /** Upper bound on items that cross the native boundary per fetch. A fetch
   *  flushes when it reaches this many items or `maxBatchBytes`, whichever binds
   *  first, so neither alone is guaranteed - both are caps the fetch tries to
   *  fill up to. Also sets the array size yielded by `IterStream.raw()`. The
   *  default item loop drains each fetch one item at a time, so this only changes
   *  how much is fetched and held at once, not what item iteration yields.
   *
   * Default is `1000`. */
  maxBatchCount?: number;
  /** Upper bound on serialized bytes held per fetch. Keeps peak memory bounded
   *  when items are large (e.g. records with big nested arrays) regardless of
   *  `maxBatchCount`: the fetch flushes once its buffer reaches this size. At
   *  least one item is always fetched, so a single item larger than this still
   *  makes progress. Must be a positive integer; to let `maxBatchCount` dominate,
   *  set this higher.
   *
   * Default is `262144` (256 KiB). */
  maxBatchBytes?: number;
}

export type VariadicPathArgs<TTail> = [...Segment[]] | [...Segment[], TTail];

export function splitArgs<TTail>(args: VariadicPathArgs<TTail>): { path: Segment[]; tail: TTail | undefined } {
  let pathArgs: unknown[];
  let tail: TTail | undefined;
  if (args.length === 0) {
    pathArgs = [];
    tail = undefined;
  } else {
    const last = args[args.length - 1];
    if (last !== null && typeof last === 'object' && !Array.isArray(last)) {
      pathArgs = args.slice(0, -1);
      tail = last as TTail;
    } else {
      pathArgs = args as unknown[];
      tail = undefined;
    }
  }
  validatePath(pathArgs);
  return { path: pathArgs as Segment[], tail };
}

export function isSchema(value: unknown): value is StandardSchemaV1 {
  return typeof value === 'object' && value !== null && '~standard' in value;
}

export function normalizeIterTail(tail: StandardSchemaV1 | IterOptions | undefined): IterOptions {
  if (!tail) {
    return {};
  }
  if (isSchema(tail)) {
    return { schema: tail };
  }
  return tail;
}

export function serializeSelect(select: Segment | Path | Record<string, Segment | Path>): string {
  if (typeof select === 'string' || typeof select === 'number') {
    const one = [select];
    validatePath(one);
    return JSON.stringify({ one });
  }
  if (Array.isArray(select)) {
    validatePath(select);
    if (select.length === 0) {
      throw new RangeError('iter: select sub-path must have at least one segment');
    }
    return JSON.stringify({ one: select });
  }
  if (select === null || typeof select !== 'object') {
    throw new TypeError(`iter: select must be a segment, path, or field map, got ${describeSelect(select)}`);
  }
  const entries = Object.entries(select).map(([k, sub]) => {
    const path = typeof sub === 'string' || typeof sub === 'number' ? [sub] : sub;
    if (!Array.isArray(path)) {
      throw new TypeError(
        `iter: select field ${JSON.stringify(k)} must be a segment or path, got ${describeSelect(sub)}`,
      );
    }
    validatePath(path);
    if (path.length === 0) {
      throw new RangeError(`iter: select field ${JSON.stringify(k)} sub-path must have at least one segment`);
    }
    return [k, path] as const;
  });
  if (entries.length === 0) {
    throw new RangeError('iter: select must have at least one field');
  }
  return JSON.stringify({ map: entries });
}

function describeSelect(value: unknown): string {
  if (value === null) {
    return 'null';
  }
  return Array.isArray(value) ? 'array' : typeof value;
}
