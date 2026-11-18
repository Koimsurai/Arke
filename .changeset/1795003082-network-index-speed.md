---
default: minor
---

Indexing a network share for This PC search is about 2.5 times faster, and searching a share's subfolders or file contents finds results much sooner: folders are listed and files read several at once, in large batches. A NAS's snapshots are skipped wherever they show up, and a share opened both as a mapped drive and as `\\server\share` is indexed once.

<!-- zh-TW -->
「本機」搜尋為網路共用資料夾建立索引快了約 2.5 倍，搜尋網路共用資料夾的子資料夾或檔案內容也更快出現結果：同時讀取多個資料夾和檔案，每次向伺服器取回更多項目。NAS 的快照不論出現在哪裡都會略過，同一個共用資料夾同時以對應磁碟機和 `\\伺服器\共用` 開過時只建立一次索引。

<!-- zh-CN -->
「此电脑」搜索为网络共享文件夹建立索引快了约 2.5 倍，搜索网络共享文件夹的子文件夹或文件内容也更快出现结果：同时读取多个文件夹和文件，每次从服务器取回更多项目。NAS 的快照无论出现在哪里都会跳过，同一个共享文件夹同时以映射驱动器和 `\\服务器\共享` 打开过时只建立一次索引。
