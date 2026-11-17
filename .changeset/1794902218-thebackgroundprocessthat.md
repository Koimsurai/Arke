---
default: patch
---

The background process that reads the Windows context menu is replaced after 100 menus, so threads that some Windows extensions leave behind on every menu (Work Folders) no longer pile up while Arke runs for days.

<!-- zh-TW -->
讀取 Windows 右鍵選單的背景程序每讀 100 次選單會換新，某些 Windows 擴充（工作資料夾）每次留下的執行緒，不會再在 Arke 連續開好幾天時越積越多。

<!-- zh-CN -->
读取 Windows 右键菜单的后台进程每读 100 次菜单会换新，某些 Windows 扩展（工作文件夹）每次留下的线程，不会再在 Arke 连续开好几天时越积越多。
