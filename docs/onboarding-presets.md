# Setup guide presets

The setup guide's first step ("Where are you coming from?") sets existing Arke settings to the
out-of-the-box behavior of another file manager. The table for each preset lists every setting it
sets, the value, and what the value is based on. The code is `crates/app/src/onboarding.rs`; a
preset never sets anything Settings doesn't have.

Most of these programs don't state their defaults in their documentation; their option pages list
the option, and the default is what a fresh install does. The "Basis" column says which it is:

- **Documented**: the program's own documentation or source code states the value.
- **Option documented**: the documentation describes the option; the default is the fresh-install
  behavior, and needs checking on a clean install.
- **Third party**: forums or articles, linked.

Where the two programs of a preset (Total Commander and Double Commander, Directory Opus and
XYplorer) might differ, or a default couldn't be backed at all, the setting is left out of the
preset rather than guessed.

## Windows File Explorer (Windows 11)

| Arke setting | Value | Basis |
| --- | --- | --- |
| Open items with a single click | Off | Fresh-install behavior: Folder Options › "Double-click to open an item (single-click to select)" |
| Folders | Before files | Fresh-install behavior: folders come before files going up and after them going down; Explorer has no option to mix them or to keep them first both ways |
| Sort by | Name, ascending | Fresh-install behavior |
| View | Details | Fresh-install behavior for general items folders (Documents, Downloads) |
| Show hidden items | Off | Documented: hidden items are off until turned on under View › Show ([Microsoft](https://support.microsoft.com/en-us/windows/view-hidden-files-and-folders-in-windows-97fbc472-c603-9d90-91d0-1166d1d9f4b5)) |
| Show file name extensions | On | **Deliberate deviation.** Explorer hides extensions out of the box ("Hide extensions for known file types", option at View › Show › File name extensions, [Microsoft](https://support.microsoft.com/en-us/windows/common-file-name-extensions-in-windows-da4a4430-8e76-89c5-59f7-1cdbbc75cb01)); the preset keeps them shown, since a hidden extension hides what a file really is |
| Date format | Full date and time | Fresh-install behavior: Date modified shows date and time |
| File size units | 1 KB = 1024 bytes | Fresh-install behavior (Arke's default already says "like Windows") |
| Tabs and sidebar | Top | Fresh-install behavior: Windows 11 22H2 and later have tabs above the address bar |
| Ctrl+T | New tab with Home | Checked on Windows 11 build 28000: Ctrl+T opens a Home tab; a new tab opens where "Open File Explorer to" points, Home by default ([Microsoft Q&A](https://learn.microsoft.com/en-us/answers/a/12283558)) |
| New windows open to | Home | Documented as above: File Explorer opens to Home by default ([Microsoft Q&A](https://learn.microsoft.com/en-us/answers/a/12283558)) |
| Back and forward | Within the current tab | Fresh-install behavior: each tab has its own history |
| Open This PC to the current folder | Off | Fresh-install behavior: navigation pane "Expand to open folder" is off (same reason as Arke's own default, #416) |
| Item check boxes | Off | Fresh-install behavior on a desktop PC (touch devices turn them on) |
| Ask before moving to the Recycle Bin | Off | Fresh-install behavior since Windows 8: "Display delete confirmation dialog" is off |

## Total Commander / Double Commander

| Arke setting | Value | Basis |
| --- | --- | --- |
| Open items with a single click | Off | Documented for DC: "A double click is necessary to launch the action (default)" ([DC manual](https://doublecmd.github.io/doc/en/configuration.html)); TC: option documented ([TC wiki, Options](https://www.ghisler.ch/wiki/index.php/Options)) |
| Folders | Always first | Documented for DC: `gSortFolderMode := sfmSortLikeFileShowFirst` ([DC source, uglobs.pas](https://raw.githubusercontent.com/doublecmd/doublecmd/master/src/uglobs.pas)); TC: option documented ("Sort directories: Always by name / Like files", [TC wiki, Options](https://www.ghisler.ch/wiki/index.php/Options)); directories are listed above files either way |
| Sort by | Name, ascending | Fresh-install behavior of both |
| View | Details | Fresh-install behavior: the "Full" (columns) view |
| Show hidden items | Off | Documented for DC: `gShowSystemFiles := False` ([uglobs.pas](https://raw.githubusercontent.com/doublecmd/doublecmd/master/src/uglobs.pas)); TC: author on the forum ([TC forum](https://ghisler.ch/board/viewtopic.php?p=276079)) |
| Show file name extensions | Off | The name column shows the name without its extension, which has a column of its own ([TC forum](https://www.ghisler.ch/board/viewtopic.php?p=461195), third party) |
| Extension column | On | As above: default columns Name, Ext, Size, Date, Attr |
| Type column | Off | As above: there is no Type column |
| Attributes column | On | As above |
| Date format | Full date and time | Fresh-install behavior: the Date column shows date and time |
| File size units | 1 KB = 1024 bytes | Fresh-install behavior |
| Tabs and sidebar | Top | Documented for DC: `gDirTabPosition := tbpos_top` ([uglobs.pas](https://raw.githubusercontent.com/doublecmd/doublecmd/master/src/uglobs.pas)); TC: tabs above each panel |
| Ctrl+T | New tab here | TC: Ctrl+T opens a new tab on the current folder ([Softpanorama](https://softpanorama.org/OFM/Total_commander/folder_tabs.shtml), third party) |
| Ask before moving to the Recycle Bin | On | Documented for DC: delete confirmations on, "maximum secure behavior is chosen by default" ([DC manual](https://doublecmd.github.io/doc/en/configuration.html), [uglobs.pas](https://raw.githubusercontent.com/doublecmd/doublecmd/master/src/uglobs.pas)); TC asks before deleting out of the box |

Not mapped: dual pane (Arke has no setting that opens new tabs split; Ctrl+Shift+S and the tab
menu still split a tab), the F-key layout (Arke has no keyboard scheme presets; F5 copying to the
other pane would clash with Refresh), and where TC starts (it varies with "Save on exit").

## Directory Opus / XYplorer

Only what both do out of the box.

| Arke setting | Value | Basis |
| --- | --- | --- |
| Open items with a single click | Off | Option documented (single-click is an option in both: [Opus](https://www.gpsoft.com.au/help/opus12/Documents/Folder_Options.htm)); off out of the box |
| Folders | Always first | Option documented: Opus "sort folders alphabetically" / XYplorer "Sort folders apart" ([Opus](https://www.gpsoft.com.au/help/opus12/documents/FolderOptions/Display1.htm), [XYplorer forum](https://www.xyplorer.com/xyfc/viewtopic.php?p=213988)); folders first out of the box |
| Sort by | Name, ascending | Fresh-install behavior of both |
| View | Details | Fresh-install behavior of both |
| Show file name extensions | On | Fresh-install behavior of both |
| Date format | Full date and time | Fresh-install behavior of both |
| Tabs and sidebar | Top | Documented for Opus: folder tabs above the file display ([Opus, Tabs](https://www.gpsoft.com.au/help/opus12/Documents/Tabs.htm)); XYplorer's tab bar is above the list |

Not mapped: hidden files (the two may differ), Ctrl+T (Opus's new tab shows the current folder,
[Opus, Tabs](https://www.gpsoft.com.au/help/opus12/Documents/Tabs.htm); XYplorer's couldn't be
backed), delete confirmation, dual pane (no Arke setting).

## macOS Finder

| Arke setting | Value | Basis |
| --- | --- | --- |
| Open items with a single click | Off | Fresh-install behavior: Finder opens with a double click |
| Folders | Mixed with files | Option documented: Finder settings › "Keep folders on top", off out of the box ([Apple](https://support.apple.com/guide/mac-help/mchlp2803/mac), [OSXDaily](https://osxdaily.com/2018/02/26/keep-folders-on-top-sorting-name-mac-finder/)) |
| Sort by | Name, ascending | Fresh-install behavior of list view |
| View | Large icons | Fresh-install behavior: icon view |
| Show hidden items | Off | Option documented: Command-Shift-Period shows them ([Apple](https://support.apple.com/guide/mac-help/mchlp2803/mac)) |
| Treat names starting with a dot as hidden | On | macOS hides dot files in Finder |
| Show file name extensions | Off | Option documented: Finder settings › Advanced › "Show all filename extensions", off out of the box ([Apple](https://support.apple.com/guide/mac-help/mchlp2803/mac)) |
| Date format | Short ("Today 07:49") | Fresh-install behavior: Finder writes "Today at 10:23" |
| File size units | 1 KB = 1000 bytes | Documented: macOS since 10.6 counts in decimal units ([Apple](https://support.apple.com/en-us/102119)) |
| Calculate folder sizes | Off | Fresh-install behavior: list view's "Calculate all sizes" is off |
| Tabs and sidebar | Top | Fresh-install behavior: Finder tabs are under the toolbar |
| Ctrl+T | New tab with Home | Fresh-install behavior: a new tab shows the "New Finder windows show" folder, Recents by default; Arke's Home page lists recent files |
| New windows open to | Home | As above |
| Ask before moving to the Recycle Bin | Off | Fresh-install behavior: Move to Trash doesn't ask |

## Checking

Values marked "fresh-install behavior" or "option documented" should be confirmed on a clean
install of each program before release; change `onboarding.rs` and this file together.
`onboarding.rs` has tests that each preset sets exactly the settings listed here.
