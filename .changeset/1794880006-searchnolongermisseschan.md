---
default: patch
---

Search no longer misses changes when its folder watch overflows soon after a rebuild, or when the search service loses its place while the index is being updated: the rebuild now follows instead of being skipped. When the change journal keeps getting deleted (some cleanup tools do this), rebuilds are spaced further apart.

<!-- zh-TW -->
搜尋不會再因為重建後不久資料夾監看溢位，或搜尋服務在更新索引時接不上而漏掉變更：該做的重建會接著做，不再被略過。變更日誌一再被刪除時（有些清理工具會這樣做），重建的間隔會逐次拉長。

<!-- zh-CN -->
搜索不会再因为重建后不久文件夹监视溢出，或搜索服务在更新索引时接不上而漏掉更改：该做的重建会接着做，不再被跳过。更改日志一再被删除时（有些清理工具会这样做），重建的间隔会逐次拉长。
