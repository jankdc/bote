import { open, fromFile } from '@botejs/core';
import { User } from './schemas';

await using cursor = await open(fromFile('./users.json'));

// `iter` yields one item at a time. pass an options object as the last argument
// to tune what comes back. (for the raw fetch arrays, append `.raw()`.)
for await (const [key, user] of cursor.iter('users', {
  // how many items cross the native boundary per fetch, which also bounds resident
  // memory. larger reads fewer times but holds more at once; the item loop still
  // yields one at a time. (number, default: native)
  batch: 100,
  // project a sub-path out of each item instead of the whole value. a single
  // segment/path narrows to one value; a record reshapes each item into
  // { field: value }, scanning only those paths.
  // (Segment | Path | Record<string, Path>, default: none)
  select: { id: 'id', email: ['contact', 'email'] },
  // validate each item (after select) and infer its type. see validation below.
  // (Standard Schema, default: none)
  schema: User,
  // what to do with an item that fails schema: surface the error, or silently
  // drop it. ('throw' | 'skip', default: 'throw')
  onInvalid: 'skip',
  // yield [key, value] tuples instead of bare values. key is the member name for
  // an object, the element's zero-based index for an array. (boolean, default: false)
  withKey: true,
})) {
  console.log(key, user.id, user.email);
}
