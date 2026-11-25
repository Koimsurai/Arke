---
default: patch
---

Folders reached through a junction, such as Scoop's `current`, open in an Arke the installer started after installing or updating, and in the terminals it opens, instead of failing with error 448. Files whose app was installed with Scoop open in that app instead of asking which app to use (#406, #392)

<!-- zh-TW -->
安裝或更新後由安裝程式啟動的 Arke，以及從它開啟的終端機，都能開啟經由連接點（例如 Scoop 的 `current`）到達的資料夾，不會再出現錯誤 448。用 Scoop 安裝的程式開檔時也會直接開啟，不會跳出「選擇應用程式」(#406, #392)

<!-- zh-CN -->
安装或更新后由安装程序启动的 Arke，以及从它打开的终端，都能打开经由联接点（例如 Scoop 的 `current`）到达的文件夹，不会再出现错误 448。用 Scoop 安装的程序打开文件时也会直接打开，不会弹出“选择应用”(#406, #392)
