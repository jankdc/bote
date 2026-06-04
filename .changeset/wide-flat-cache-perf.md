---
'@botejs/native': patch
'@botejs/core': patch
---

Fix a severe slowdown on repeated deep reads into a very wide object. The structural-index cache built its object member table with a linear scan (O(members²)) and could mint a table larger than the cache budget, only to evict it immediately and rebuild it on the next read. Member tables are now hash-backed (O(1) lookup/dedup) and clamped to the budget.
