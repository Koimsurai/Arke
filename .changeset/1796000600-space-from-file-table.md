---
default: patch
---

When the search index doesn't cover a drive, Properties counts the drive from its file table through the index service, the way WizTree does (C: drops from over a minute to seconds). For a big folder, the file table takes over a walk that has been running as long as reading the table took last time on that drive, and whichever is done first is shown; the other stops at once, so a small folder never waits for the table. On a hard disk the walk hands over to the table instead of both reading at once.

<!-- zh-TW -->
全電腦搜尋的索引沒涵蓋某顆磁碟時，內容視窗改透過索引服務讀磁碟的檔案表來算，做法和 WizTree 一樣（C: 從一分多鐘降到幾秒）。大資料夾則先掃描；掃描時間超過上次讀那顆磁碟檔案表的時間，就同時改讀檔案表，哪個先算完就顯示哪個，另一個立刻停止，小資料夾不會等檔案表。傳統硬碟上則改由檔案表接手，不會兩邊同時讀。

<!-- zh-CN -->
全电脑搜索的索引没覆盖某个磁盘时，属性窗口改通过索引服务读取磁盘的文件表来计算，做法和 WizTree 一样（C: 从一分多钟降到几秒）。大文件夹则先扫描；扫描时间超过上次读取该磁盘文件表的时间，就同时改读文件表，哪个先算完就显示哪个，另一个立刻停止，小文件夹不会等文件表。机械硬盘上则改由文件表接手，不会两边同时读。
