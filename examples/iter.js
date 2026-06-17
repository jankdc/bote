import { open, fromFile } from '@botejs/core';
import { User } from './schemas';

await using cursor = await open(fromFile('./users.json'));

// `iter` streams the members of the array or object at a path one item at a time,
// so a million-element array never lands in memory all at once. Pass an options
// object as the last argument to tune what comes back. Hover over the properties
// for more details.
for await (const [index, user] of cursor.iter('users', {
  maxBatchCount: 100,
  maxBatchBytes: 64 * 1024,
  select: { id: 'id', email: ['contact', 'email'] },
  schema: User,
  onInvalid: 'skip',
  withKey: true,
})) {
  console.log(user);
}

// With no options it yields each whole member. An empty path iterates the root
// container; iterating an object yields its values (use `withKey` for the names).
for await (const user of cursor.iter('users')) {
  console.log(user.name);
}

// Can shorthand a Standard Schema here too.
for await (const user of cursor.iter('users', User)) {
  console.log(user.id);
}

// The returned stream is also a lazy collection with array-like combinators
// (map/filter/take/drop/find/some/every/reduce/forEach/toArray). they compose
// without buffering the whole sequence and short-circuit where they can.
const firstFive = await cursor
  .iter('users', { select: 'name' })
  .filter((name) => name.startsWith('A'))
  .take(5)
  .toArray();

console.log(firstFive);

// `.raw()` hands back one fetch's worth of items at a time (up to `maxBatchCount`
// items, or fewer if `maxBatchBytes` binds first). mainly used for advanced usages
// and you don't like the sugar.
for await (const batch of cursor.iter('users', { maxBatchCount: 50, withKey: true }).raw()) {
  const userCursors = await Promise.all(batch.map(([index]) => cursor.hop('users', index)));
}
