import { open, fromFile } from '@botejs/core';

await using cursor = await open(fromFile('./users.json'));

// `hop` resolves a path to a container and hands back a new cursor anchored there,
// so further get/has/iter/hop run relative to it instead of repeating the prefix.
// It returns `null` when nothing lives at the path.
const user = await cursor.hop('users', 0);
if (user) {
  console.log(await user.get('name'));
  console.log(await user.has('email'));
  for await (const order of user.iter('orders')) {
    console.log(order.id);
  }
}

// A child cursor shares the root's source and lifetime, so there is nothing to
// close on its own; closing the root (or its `await using`) closes it too.

// Hop again from a child to descend further; every path is relative to the anchor.
const address = await user?.hop('address');
console.log(await address?.get('city'));

// Anchoring a subtree lets a helper work against it without knowing where it sits
// in the document.
async function fullName(node) {
  return `${await node.get('first')} ${await node.get('last')}`;
}

console.log(await fullName(user));

// Pairs nicely with `iter(..., { withKey: true })`: take each member's key,
// then hop back in for deeper random access into that element.
for await (const [index, name] of cursor.iter('users', { withKey: true, select: 'name' })) {
  const orders = await cursor.hop('users', index, 'orders');
  if (!orders) {
    continue;
  }
  for await (const order of orders.iter()) {
    console.log(name, order.id);
  }
}
