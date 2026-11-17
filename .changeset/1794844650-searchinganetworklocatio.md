---
default: patch
---

Searching a network location no longer walks a NAS's snapshot, recycle bin and thumbnail folders (such as Synology's `#snapshot`, `#recycle` and `@eaDir`), and stops at 2 million items. On a share with snapshots, indexing could run for hours, keeping a CPU core busy and Arke's memory growing.

<!-- zh-TW -->
搜尋網路位置時，不再走進 NAS 的快照、資源回收筒和縮圖資料夾（例如 Synology 的 `#snapshot`、`#recycle` 和 `@eaDir`），而且最多索引 200 萬個項目。之前在有快照的分享上，索引可能跑上好幾個小時，一直佔著一個 CPU 核心，Arke 的記憶體也越用越多。

<!-- zh-CN -->
搜索网络位置时，不再进入 NAS 的快照、回收站和缩略图文件夹（例如 Synology 的 `#snapshot`、`#recycle` 和 `@eaDir`），而且最多索引 200 万个项目。之前在有快照的共享上，索引可能运行好几个小时，一直占着一个 CPU 核心，Arke 的内存也越用越多。
