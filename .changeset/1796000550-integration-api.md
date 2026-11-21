---
default: minor
---

Launchers and scripts (ALTRun, Listary, AutoHotkey, PowerShell) can read Arke's windows, tabs, folders and selection, open a folder with items selected, and run commands, through one JSON interface: `WM_COPYDATA` to a window of class `ArkeWindow`, or `arke.exe --api`. Commands that change files are not available. See docs/integration.md. (#193)

<!-- zh-TW -->
啟動器與腳本（ALTRun、Listary、AutoHotkey、PowerShell）可以透過同一套 JSON 介面讀取 Arke 的視窗、分頁、資料夾與選取，開啟資料夾並選取項目，以及執行指令：對類別為 `ArkeWindow` 的視窗送 `WM_COPYDATA`，或用 `arke.exe --api`。不開放會變更檔案的指令。說明見 docs/integration.md。（#193）

<!-- zh-CN -->
启动器与脚本（ALTRun、Listary、AutoHotkey、PowerShell）可以通过同一套 JSON 接口读取 Arke 的窗口、标签页、文件夹与选择，打开文件夹并选中项目，以及执行命令：向类名为 `ArkeWindow` 的窗口发送 `WM_COPYDATA`，或使用 `arke.exe --api`。不开放会更改文件的命令。说明见 docs/integration.md。（#193）
