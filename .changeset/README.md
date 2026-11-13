# Changesets

One file per user-visible change (the changesets and knope format), in English with a
Traditional and a Simplified Chinese translation, since Arke shows the notes in the user's
language (Settings › About › What's new):

```md
---
default: minor
---

Tabs can be reordered by dragging

<!-- zh-TW -->
分頁可以拖曳排序

<!-- zh-CN -->
标签页可以拖动排序
```

`default` is `patch` (fix), `minor` (feature) or `major` (breaking). Create one with
`cargo xtask change minor "..." --zh-TW "..." --zh-CN "..."`; `cargo xtask status` lists the ones
still missing a translation, and `cargo xtask version` (which needs them all) writes them into
`CHANGELOG.md`, `CHANGELOG.zh-TW.md` and `CHANGELOG.zh-CN.md` and deletes them.
