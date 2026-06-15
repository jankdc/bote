---
'@botejs/core': patch
---

Remove redundant 'seekable' property in Reader.

It's a bit of an oversight to have this since we only need to use
Source.seekable to get the kind of source that we're dealing with.

This removes the extra prop that someone has to write.

It's not technically breaking change as it leaves any prop hanging
about in previous versions to be a dud.
