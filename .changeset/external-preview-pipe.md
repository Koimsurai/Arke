---
default: patch
---

The external preview program can also be reached through a named pipe, so a program that's already running opens previews instantly; a command line after `||` starts it when it isn't running. QuickLook settings from 0.9 are converted to this

<!-- zh-TW -->
外部預覽程式也可以改用具名管道傳送，已在執行的程式能立刻開啟預覽；程式沒在執行時，會改用 `||` 後面的命令列啟動它。0.9 的 QuickLook 設定會自動轉成這種寫法

<!-- zh-CN -->
外部预览程序也可以改用命名管道发送，已在运行的程序能立刻打开预览；程序没在运行时，会改用 `||` 后面的命令行启动它。0.9 的 QuickLook 设置会自动转换成这种写法
