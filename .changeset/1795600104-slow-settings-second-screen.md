---
default: patch
---

Settings no longer go back to the defaults when the settings file is read slowly (such as `%APPDATA%` on a network drive) and a folder was opened meanwhile. Windows on a second screen reopen there instead of on the main screen.

<!-- zh-TW -->
設定檔讀取較慢時（例如 `%APPDATA%` 在網路磁碟上）先開了資料夾，設定不會再被還原成預設值。在第二個螢幕上的視窗重新開啟時會回到那個螢幕，不會跑到主螢幕。

<!-- zh-CN -->
设置文件读取较慢时（例如 `%APPDATA%` 在网络驱动器上）先打开了文件夹，设置不会再被还原为默认值。在第二个屏幕上的窗口重新打开时会回到那个屏幕，不会跑到主屏幕。
