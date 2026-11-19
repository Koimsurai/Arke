---
default: patch
---

Extracting an archive that breaks off partway no longer leaves a damaged copy of the file it stopped in. On Windows 10 this happened with RAR archives that Windows' own reader can't fully decode; Arke now says so instead. Cancelling an extraction also removes the file it was writing. (#229)

<!-- zh-TW -->
解壓縮到一半中斷的壓縮檔，不會再留下停在那個檔案的損壞副本。Windows 10 上，Windows 內建的讀取器無法完整解開某些 RAR 檔時會發生這種情況，現在 Arke 會直接說明。取消解壓縮時，寫到一半的檔案也會一併移除。(#229)

<!-- zh-CN -->
解压到一半中断的压缩包，不会再留下停在那个文件的损坏副本。Windows 10 上，Windows 自带的读取器无法完整解开某些 RAR 文件时会出现这种情况，现在 Arke 会直接说明。取消解压时，写到一半的文件也会一并删除。(#229)
