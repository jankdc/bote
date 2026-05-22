// Worker process for stress.ts. Runs under the tight V8 heap caps set
// on its node invocation, walks the doc end-to-end, exits 0 on success.
// V8 OOM aborts the process with a non-zero status, which stress.ts
// interprets as a failure.

import { open, fromFile } from 'bote'

const path = process.argv[2]
if (!path) {
  console.error('stress-worker: doc path required as argv[2]')
  process.exit(2)
}

await using cursor = await open(fromFile(path))
let count = 0
for await (const child of cursor.walk('/items')) {
  const name = await child.get('/name')
  // Touch the resolved value so the optimizer can't elide the read.
  if (typeof name !== 'string' || !name.startsWith('item-')) {
    console.error(`stress-worker: unexpected value at item ${count}: ${JSON.stringify(name)}`)
    process.exit(2)
  }
  count += 1
}
console.error(`stress-worker: walked ${count} items OK`)
