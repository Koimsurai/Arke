---
default: minor
---

Network shares in This PC search are much faster to index and to search: folders are listed several at once in large batches, a daily rewalk lists again only the folders that may have changed, and searching names or file contents below an indexed share takes the files from the index instead of walking the share. Results show up while the first walk is still running. A NAS's snapshots are skipped wherever they show up, and a share opened both as a mapped drive and as `\\server\share` is indexed once.

<!-- zh-TW -->
「本機」搜尋的網路共用資料夾，建立索引和搜尋都快很多：同時讀取多個資料夾、每次向伺服器取回更多項目；每天的重新整理只重讀可能有變動的資料夾；在已建立索引的共用資料夾底下搜尋檔名或檔案內容，直接用索引裡的檔案清單，不再逐一走訪整個共用資料夾。第一次建立索引還沒完成時就能搜到已找到的項目。NAS 的快照不論出現在哪裡都會略過，同一個共用資料夾同時以對應磁碟機和 `\\伺服器\共用` 開過時只建立一次索引。

<!-- zh-CN -->
「此电脑」搜索的网络共享文件夹，建立索引和搜索都快很多：同时读取多个文件夹、每次从服务器取回更多项目；每天的重新整理只重读可能有变动的文件夹；在已建立索引的共享文件夹下搜索文件名或文件内容，直接用索引里的文件列表，不再逐一遍历整个共享文件夹。第一次建立索引还没完成时就能搜到已找到的项目。NAS 的快照无论出现在哪里都会跳过，同一个共享文件夹同时以映射驱动器和 `\\服务器\共享` 打开过时只建立一次索引。
