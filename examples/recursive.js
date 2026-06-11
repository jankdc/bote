import { open, fromFile } from '@botejs/core'

await using cursor = await open(fromFile('./thread.json'))

async function* memberKeys(node, ...path) {
  for await (const [key] of node.iter(...path, { withKey: true, select: 'id' })) yield key
}

// descend example

for await (const id of memberKeys(cursor, 'thread')) {
  const root = await cursor.hop('thread', id)
  for await (const comment of descend(id, root)) {
    const indent = '  '.repeat(comment.depth)
    console.log(`${indent}${comment.author}: ${comment.text}  (${comment.path.join(' > ')})`)
  }
}

async function* descend(id, node, trail = []) {
  const path = [...trail, id]
  yield { depth: trail.length, path, author: await node.get('author'), text: await node.get('text') }

  if (await node.has('replies')) {
    for await (const childId of memberKeys(node, 'replies')) {
      const reply = await node.hop('replies', childId)
      yield* descend(childId, reply, path)
    }
  }
}

// find-first example

for await (const id of memberKeys(cursor, 'thread')) {
  const root = await cursor.hop('thread', id)
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
    for await (const childId of memberKeys(node, 'replies')) {
      const reply = await node.hop('replies', childId)
      const hit = await findFirst(reply, predicate)
      if (hit) {
        return hit
      }
    }
  }
  return null
}
