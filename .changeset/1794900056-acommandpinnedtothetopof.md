---
default: patch
---

A command pinned to the top of the context menu from "Show more options" now shows right away instead of waiting for every extension in the Windows menu: its extension is read on its own. The first menu after pinning learns which extension that is. How long each extension takes is written to the log, to find a slow one. (#129)

<!-- zh-TW -->
從「顯示更多選項」釘到右鍵選單上方的指令，現在會馬上出現，不用等 Windows 選單裡所有擴充都讀完：只單獨讀它所屬的擴充。釘選後的第一次選單會記住是哪個擴充。每個擴充花多久會寫進 log，方便找出慢的那個。(#129)

<!-- zh-CN -->
从“显示更多选项”固定到右键菜单上方的命令，现在会马上出现，不用等 Windows 菜单里所有扩展都读完：只单独读它所属的扩展。固定后的第一次菜单会记住是哪个扩展。每个扩展花多久会写进 log，方便找出慢的那个。(#129)
