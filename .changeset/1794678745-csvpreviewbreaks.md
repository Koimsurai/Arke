---
default: patch
---

Text and CSV previews no longer come up empty when the file breaks lines with a lone CR (like old Mac Excel exports), or has form feeds, vertical tabs or NEL characters in it. Those now start a new line.

<!-- zh-TW -->
文字和 CSV 預覽在檔案只用 CR 換行（例如舊版 Mac Excel 匯出的檔案），或含有換頁、垂直定位、NEL 字元時，不再顯示空白，這些字元現在會換到下一行。

<!-- zh-CN -->
文本和 CSV 预览在文件只用 CR 换行（例如旧版 Mac Excel 导出的文件），或含有换页、垂直制表、NEL 字符时，不再显示空白，这些字符现在会换到下一行。
