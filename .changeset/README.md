# Changesets

One file per user-visible change (same format as changesets and knope):

```md
---
default: minor
---

Tabs can be reordered by dragging
```

`default` is `patch` (fix), `minor` (feature) or `major` (breaking). Create one with
`cargo xtask change minor "..."`; `cargo xtask version` writes them into CHANGELOG.md and
deletes them.
