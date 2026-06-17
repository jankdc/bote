/**
 * A lazy, single-pass async pipeline over the items produced by `Cursor.iter`.
 * The transforms (`map`/`filter`/`take`/`drop`) build a new pipeline without
 * pulling anything; work happens only when a terminal step (`for await`,
 * `toArray`, `reduce`, etc.) drains it. Every callback may be async. The stream
 * is single-use: consuming it (or any chained terminal) exhausts the source, so
 * iterate or collect it once.
 */
export interface IterStream<T> extends AsyncIterable<T> {
  /**
   * Yield the underlying fetch batches (arrays of items) instead of individual
   * items, exposing the batch boundary set by {@link IterOptions.maxBatchCount}
   * (and {@link IterOptions.maxBatchBytes}). Used
   * for more advanced use cases or if you simply don't like all the sugar.
   */
  raw(): AsyncIterable<T[]>;

  /** Like `Array.prototype.map`, but lazy and streaming; `fn` may be async. */
  map<U>(fn: (item: T, index: number) => U | Promise<U>): IterStream<U>;
  /** Like `Array.prototype.filter`, but lazy and streaming; narrows `T` to `U` via the type guard. */
  filter<U extends T>(fn: (item: T, index: number) => item is U): IterStream<U>;
  /** Like `Array.prototype.filter`, but lazy and streaming; `fn` may be async. */
  filter(fn: (item: T, index: number) => boolean | Promise<boolean>): IterStream<T>;
  /** Keep at most the first `limit` items, then stop pulling from the source. Like `Array.prototype.slice(0, limit)`. */
  take(limit: number): IterStream<T>;
  /** Skip the first `limit` items, then yield the rest. Like `Array.prototype.slice(limit)`. */
  drop(limit: number): IterStream<T>;

  /** Drain the stream into an array. Like `Array.prototype` spreading, but buffers every item - avoid on unbounded sources. */
  toArray(): Promise<T[]>;
  /** Like `Array.prototype.forEach`, but awaits the stream and each (possibly async) `fn`. */
  forEach(fn: (item: T, index: number) => void | Promise<void>): Promise<void>;
  /** Like `Array.prototype.reduce` with a required `init`, but streaming; `fn` may be async. */
  reduce<A>(fn: (acc: A, item: T, index: number) => A | Promise<A>, init: A): Promise<A>;
  /** Like `Array.prototype.find`, but streaming and short-circuiting; stops pulling once `fn` matches. */
  find(fn: (item: T, index: number) => boolean | Promise<boolean>): Promise<T | undefined>;
  /** Like `Array.prototype.some`, but streaming and short-circuiting; stops at the first match. */
  some(fn: (item: T, index: number) => boolean | Promise<boolean>): Promise<boolean>;
  /** Like `Array.prototype.every`, but streaming and short-circuiting; stops at the first failure. */
  every(fn: (item: T, index: number) => boolean | Promise<boolean>): Promise<boolean>;
}

export function makeStream<T>(batches: () => AsyncIterable<T[]>, batchSize: number, regroup = false): IterStream<T> {
  const derive = <U>(next: () => AsyncIterable<U[]>): IterStream<U> => makeStream(next, batchSize, true);
  const stream: IterStream<T> = {
    [Symbol.asyncIterator]() {
      return flatten(batches())[Symbol.asyncIterator]();
    },
    raw() {
      return regroup ? regroupBatches(batches(), batchSize) : batches();
    },
    map<U>(fn: (item: T, index: number) => U | Promise<U>): IterStream<U> {
      return derive(() => mapBatches(batches(), fn));
    },
    filter(fn: (item: T, index: number) => boolean | Promise<boolean>): IterStream<T> {
      return derive(() => filterBatches(batches(), fn));
    },
    take(limit: number): IterStream<T> {
      return derive(() => takeBatches(batches(), limit));
    },
    drop(limit: number): IterStream<T> {
      return derive(() => dropBatches(batches(), limit));
    },
    async toArray(): Promise<T[]> {
      const out: T[] = [];
      for await (const batch of batches()) {
        for (let i = 0; i < batch.length; i++) {
          out.push(batch[i]);
        }
      }
      return out;
    },
    async forEach(fn: (item: T, index: number) => void | Promise<void>): Promise<void> {
      let index = 0;
      for await (const batch of batches()) {
        for (let i = 0; i < batch.length; i++) {
          await fn(batch[i], index++);
        }
      }
    },
    async reduce<A>(fn: (acc: A, item: T, index: number) => A | Promise<A>, init: A): Promise<A> {
      let acc = init;
      let index = 0;
      for await (const batch of batches()) {
        for (let i = 0; i < batch.length; i++) {
          acc = await fn(acc, batch[i], index++);
        }
      }
      return acc;
    },
    async find(fn: (item: T, index: number) => boolean | Promise<boolean>): Promise<T | undefined> {
      let index = 0;
      for await (const batch of batches()) {
        for (let i = 0; i < batch.length; i++) {
          if (await fn(batch[i], index++)) {
            return batch[i];
          }
        }
      }
      return undefined;
    },
    async some(fn: (item: T, index: number) => boolean | Promise<boolean>): Promise<boolean> {
      let index = 0;
      for await (const batch of batches()) {
        for (let i = 0; i < batch.length; i++) {
          if (await fn(batch[i], index++)) {
            return true;
          }
        }
      }
      return false;
    },
    async every(fn: (item: T, index: number) => boolean | Promise<boolean>): Promise<boolean> {
      let index = 0;
      for await (const batch of batches()) {
        for (let i = 0; i < batch.length; i++) {
          if (!(await fn(batch[i], index++))) {
            return false;
          }
        }
      }
      return true;
    },
  };
  return stream;
}

async function* flatten<T>(batches: AsyncIterable<T[]>): AsyncGenerator<T> {
  for await (const batch of batches) {
    for (let i = 0; i < batch.length; i++) {
      yield batch[i];
    }
  }
}

async function* regroupBatches<T>(batches: AsyncIterable<T[]>, size: number): AsyncGenerator<T[]> {
  let buf: T[] = [];
  for await (const batch of batches) {
    for (let i = 0; i < batch.length; i++) {
      buf.push(batch[i]);
      if (buf.length >= size) {
        yield buf;
        buf = [];
      }
    }
  }
  if (buf.length > 0) {
    yield buf;
  }
}

async function* mapBatches<T, U>(
  batches: AsyncIterable<T[]>,
  fn: (item: T, index: number) => U | Promise<U>,
): AsyncGenerator<U[]> {
  let index = 0;
  for await (const batch of batches) {
    const out: U[] = new Array(batch.length);
    for (let i = 0; i < batch.length; i++) {
      const r = fn(batch[i], index++);
      out[i] = isThenable(r) ? await r : r;
    }
    yield out;
  }
}

async function* filterBatches<T>(
  batches: AsyncIterable<T[]>,
  fn: (item: T, index: number) => boolean | Promise<boolean>,
): AsyncGenerator<T[]> {
  let index = 0;
  for await (const batch of batches) {
    const out: T[] = [];
    for (let i = 0; i < batch.length; i++) {
      const item = batch[i];
      const r = fn(item, index++);
      if (isThenable(r) ? await r : r) {
        out.push(item);
      }
    }
    if (out.length > 0) {
      yield out;
    }
  }
}

async function* takeBatches<T>(batches: AsyncIterable<T[]>, limit: number): AsyncGenerator<T[]> {
  if (limit <= 0) {
    return;
  }
  let remaining = limit;
  for await (const batch of batches) {
    if (batch.length < remaining) {
      remaining -= batch.length;
      yield batch;
      continue;
    }
    yield batch.length === remaining ? batch : batch.slice(0, remaining);
    return;
  }
}

async function* dropBatches<T>(batches: AsyncIterable<T[]>, limit: number): AsyncGenerator<T[]> {
  let remaining = limit;
  for await (const batch of batches) {
    if (remaining === 0) {
      yield batch;
    } else if (remaining >= batch.length) {
      remaining -= batch.length;
    } else {
      yield batch.slice(remaining);
      remaining = 0;
    }
  }
}

function isThenable<T>(value: T | Promise<T>): value is Promise<T> {
  return value != null && typeof (value as { then?: unknown }).then === 'function';
}
