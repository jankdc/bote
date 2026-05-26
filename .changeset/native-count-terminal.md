---
'@botejs/native': minor
'@botejs/core': minor
---

Add `Cursor.count(pointer)` - a bounded terminal returning the number of children (array elements or object members) of the container at `pointer`, with no materialization. A missing pointer or a non-container value returns `0`. Cost is O(children) via the depth-0 comma-bitmap popcount; resident memory stays constant in document size.
