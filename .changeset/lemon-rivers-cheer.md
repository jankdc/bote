---
'@botejs/native': patch
'@botejs/core': patch
---

Harden error handling: a synchronous throw inside the JS `read` fn now surfaces as a rejected promise instead of crashing the host process, and `PathError` carries a stable `code` (`PathFaultCode`) you can branch on, with the human-readable message owned by the facade.
