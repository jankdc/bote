import { open, fromFile } from '@botejs/core';
import { Email } from './schemas';

await using cursor = await open(fromFile('./users.json'));

// `has` reports whether a value exists at a path without decoding it, so it is
// the cheap way to probe shape before committing to a `get`. the path is the same
// varargs list `get` takes; the result is a boolean.
if (await cursor.has('users', 0, 'email')) {
  console.log(await cursor.get('users', 0, 'email'));
}

// Presence is about the member existing, not its contents: a member explicitly
// set to JSON `null` still counts as present.
console.log(await cursor.has('users', 0, 'deletedAt')); // -> true even if null
console.log(await cursor.has('users', 0, 'nope')); // -> false

// Array indices work as segments too; an out-of-range index is simply absent.
console.log(await cursor.has('users', 999)); // -> false on a shorter array

// Pass a Standard Schema as the trailing argument to also require the value to
// validate against it. Unlike `get` with a schema, a parse or validation miss
// here yields `false` instead of throwing.
if (await cursor.has('users', 0, 'email', Email)) {
  console.log('has a well-formed email');
}
