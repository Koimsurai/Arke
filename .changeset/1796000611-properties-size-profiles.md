---
default: patch
---

Properties counts `C:\Users\Default` instead of showing 0, and a count that misses other people's profiles you can't read (a drive, `C:\Users`, someone else's profile) now says "at least". On a spinning disk, folders are counted from the drive's file table in one sweep instead of walking them first (walked only when the table can't count them), and folders the file table can't count are no longer read from it for nothing. "Where the space goes" is there from the moment Properties opens, filled in when the count is done, instead of appearing all at once.

<!-- zh-TW -->
內容視窗現在會計算 `C:\Users\Default` 的大小，不再顯示 0；計算時若漏掉讀不到的其他使用者設定檔（整個磁碟、`C:\Users` 或別人的設定檔），現在會標示「至少」。在傳統硬碟上，資料夾改為一次讀完磁碟的檔案表來計算，不再先逐一走訪（只有檔案表算不了時才走訪）；檔案表算不了的資料夾也不再白白讀取檔案表。「空間用在哪裡」在內容視窗一打開時就在，算完再填入，不再算完才突然出現。

<!-- zh-CN -->
属性窗口现在会计算 `C:\Users\Default` 的大小，不再显示 0；计算时若漏掉读不到的其他用户配置文件（整个磁盘、`C:\Users` 或别人的配置文件），现在会标示“至少”。在机械硬盘上，文件夹改为一次读完磁盘的文件表来计算，不再先逐一遍历（只有文件表算不了时才遍历）；文件表算不了的文件夹也不再白白读取文件表。“空间用在哪里”在属性窗口一打开时就在，算完再填入，不再算完才突然出现。
