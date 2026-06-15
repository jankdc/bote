import { open, fromFile } from '@botejs/core';
import { Age } from './schemas';

await using cursor = await open(fromFile('./users.json'));

// `get` reads and decodes the value at a path, returning a real JS value. The
// path is a varargs list of segments: strings index object members, non-negative
// integers index array elements.
const name = await cursor.get('users', 0, 'name');
console.log(name);

// The decoded value is whatever lived there: scalar, array, or object. Reading a
// whole container materializes all of it in memory, so prefer `iter` for large
// arrays/objects and reserve `get` for scalars or small sub-trees.
const tags = await cursor.get('users', 0, 'tags'); // -> string[]
const address = await cursor.get('users', 0, 'address'); // -> { ... }
console.log(tags, address);

// An absent path yields `undefined`. note this differs from a present `null`:
// `undefined` means "no such member", `null` means "the member holds the JSON `null` value".
const missing = await cursor.get('users', 0, 'nope'); // -> undefined
const explicitNull = await cursor.get('users', 0, 'deletedAt'); // -> null
console.log(missing, explicitNull);

// With no segments, `get` decodes the whole document from the root.
const everything = await cursor.get();
console.log(everything);

// Pass a Standard Schema (zod, valibot, arktype, ...) as the trailing argument to
// validate and parse the decoded value. the return type is inferred from the
// schema's output, and a validation miss throws a `ValidationError`.
const age = await cursor.get('users', 0, 'age', Age);
console.log(age);
