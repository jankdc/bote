// `walk` yields a *[key, cursor]* pair per object member, and that cursor is
// first-class: it outlives the loop and you can `walk` it again. that's what lets you
// descend a tree of unknown depth. something `iter` can't express, since it hands back
// materialized values, not cursors to recurse into. here, a comment thread keyed by id
// where every node may carry its own `replies` object of the same shape:
//
//   { "c1": { "id": "c1", "author": "ada", "text": "...",
//             "replies": { "c2": { "id": "c2", ... } } }, ... }
//
// `descend` streams the whole tree depth-first, and `findFirst` shows early exit,
// `break` mid-walk and the returned cursor stays usable.

import { open, fromFile } from '@botejs/core'

await using cursor = await open(fromFile('./thread.json'))

// descend example

for await (const [id, root] of cursor.walk('thread')) {
  for await (const comment of descend(id, root)) {
    const indent = '  '.repeat(comment.depth)
    console.log(`${indent}${comment.author}: ${comment.text}  (${comment.path.join(' > ')})`)
  }
}

// the walk key *is* each comment's id, so `descend` takes it instead of re-`get`ing it.
async function* descend(id, node, trail = []) {
  const path = [...trail, id]
  yield { depth: trail.length, path, author: await node.get('author'), text: await node.get('text') }

  if (await node.has('replies')) {
    for await (const [childId, reply] of node.walk('replies')) {
      yield* descend(childId, reply, path)
    }
  }
}

// find-first example

for await (const [, root] of cursor.walk('thread')) {
  const unresolved = await findFirst(root, (n) => n.has('flag', 'unresolved'))
  if (unresolved) {
    console.log('first unresolved:', await unresolved.get('id'), await unresolved.get('text'))
    break
  }
}

async function findFirst(node, predicate) {
  if (await predicate(node)) {
    return node
  }
  if (await node.has('replies')) {
    for await (const [, reply] of node.walk('replies')) {
      const hit = await findFirst(reply, predicate)
      if (hit) {
        return hit
      }
    }
  }
  return null
}
