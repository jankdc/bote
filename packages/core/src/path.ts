import type { Segment } from './validate.ts';

/** Upper bound on numeric segments (napi takes them as `u32`). 2^32 - 1
 *  comfortably covers any in-memory JSON array. */
export const MAX_ARRAY_INDEX = 0xffffffff;

export function validatePath(path: readonly unknown[]): asserts path is readonly Segment[] {
  for (let i = 0; i < path.length; i++) {
    const s = path[i];
    if (typeof s === 'string') {
      continue;
    }
    if (typeof s === 'number' && Number.isInteger(s) && s >= 0 && s <= MAX_ARRAY_INDEX) {
      continue;
    }
    throw new TypeError(
      `path segment ${i}: expected string or non-negative integer (<= ${MAX_ARRAY_INDEX}), got ${describeBadSegment(s)}`,
    );
  }
}

function describeBadSegment(s: unknown): string {
  if (typeof s === 'number') {
    return `${s}`;
  }
  if (s === null) {
    return 'null';
  }
  return typeof s;
}
