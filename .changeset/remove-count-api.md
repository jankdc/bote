---
'@botejs/native': minor
'@botejs/core': minor
---

Remove the `.count` API (breaking).

Counting the members of a container is rare enough that it doesn't warrant a
dedicated native scan, and it composes from the existing streaming API:

```js
let n = 0;
for await (const _ of cursor.iter('items')) n++;
```

This also drops the now-unused `child_count` field from the structural-index
cache, which only ever served repeat `count` calls.
