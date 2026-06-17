---
'@botejs/core': patch
---

Ship `@standard-schema/spec` as a regular dependency instead of declaring it as
both an optional dependency and an optional peer dependency. The two declarations
pulled in opposite directions; consolidating to a plain dependency means the
validation types resolve out of the box with nothing extra to install, while
runtime stays fully decoupled (bote only imports the spec as a type, and detects
schemas structurally via `~standard`).
