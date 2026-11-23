---
default: patch
---

Removing Arke keeps its settings folder when handing folders back to File Explorer failed, even when told to delete the settings: the backup of what to put back is in it, so running the removal again can finish instead of leaving folders pointing at the removed Arke (#360)

<!-- zh-TW -->
移除 Arke 時，如果把資料夾還給檔案總管失敗，就算指定不保留設定也會留下設定資料夾：裡面有還原用的備份，再執行一次移除就能完成，不會讓資料夾一直指向已移除的 Arke (#360)

<!-- zh-CN -->
移除 Arke 时，如果把文件夹还给资源管理器失败，即使指定不保留设置也会留下设置文件夹：里面有还原用的备份，再执行一次移除就能完成，不会让文件夹一直指向已移除的 Arke (#360)
