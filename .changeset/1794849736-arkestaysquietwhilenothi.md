---
default: patch
---

Arke stays quiet while nothing changes: an idle, minimized or tray-only Arke no longer redraws on every display refresh (it used to wake up more than a thousand times a second). With no window open it also frees its thumbnail memory and runs at background priority, and the progress ring of file operations costs less to draw.

<!-- zh-TW -->
畫面沒有變化時 Arke 會保持安靜：閒置、最小化或只剩系統匣時，不再跟著螢幕每次更新重畫（以前每秒會喚醒上千次）。沒有開著的視窗時，也會釋放縮圖記憶體並以背景優先權執行；檔案作業的進度圈也更省資源。

<!-- zh-CN -->
画面没有变化时 Arke 会保持安静：空闲、最小化或只剩系统托盘时，不再跟着屏幕每次刷新重绘（以前每秒会唤醒上千次）。没有打开的窗口时，也会释放缩略图内存并以后台优先级运行；文件操作的进度圈也更省资源。
