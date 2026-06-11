export type Segment = string | number;
export type Path = readonly Segment[];

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

export function formatPath(path: Path): string {
  if (path.length === 0) {
    return '(root)';
  }
  let out = '';
  for (let i = 0; i < path.length; i++) {
    const seg = path[i];
    if (typeof seg === 'number') {
      out += `[${seg}]`;
      continue;
    }
    if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(seg)) {
      out += i === 0 ? seg : `.${seg}`;
    } else {
      out += `[${JSON.stringify(seg)}]`;
    }
  }
  return out;
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
