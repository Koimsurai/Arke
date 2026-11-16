---
default: patch
---

When many files change, the search index folds the changes in instead of reading the whole drive again, which on a busy drive used to take tens of seconds about every hour. The log now says why each full rebuild happens.

<!-- zh-TW -->
大量檔案變動時，搜尋索引會直接併入這些變動，不再整碟重讀；在忙碌的磁碟上，這原本約每小時要花數十秒。每次整碟重建的原因也會寫進 log。

<!-- zh-CN -->
大量文件变动时，搜索索引会直接并入这些变动，不再整盘重读；在繁忙的磁盘上，这原本约每小时要花数十秒。每次整盘重建的原因也会写进日志。
