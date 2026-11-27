# Keyboard guide

Everything in Arke can be reached from the keyboard. This page lists the default keys, grouped by
the part of the window you are in. It is written to be read with a screen reader.

- Every shortcut below can be changed in **Settings › Keyboard shortcuts**. The page also lets you
  search for a command or a key.
- **Ctrl+Shift+P** opens the command palette: type part of a command's name and press Enter to run
  it. Each row shows its current key. This also works for commands that have no key by default.
- Where this guide says "Enter", a screen reader's default action (the key that clicks the item
  under the screen reader cursor) does the same. That is how a screen reader opens menus and
  dropdowns, and expands or collapses sections in Settings.

## Screen readers

Arke exposes its window through Windows UI Automation, which is what Narrator and NVDA use (JAWS
reads the same interface). So far it has been checked with UI Automation tools only; nobody has
used it with a screen reader day to day yet. If you try it, please tell us what works and what
does not (see the end of this page).

Arke's default shortcuts do not use Insert or Caps Lock, the modifier keys of NVDA and JAWS.

## Moving between areas

| Keys | Action |
| --- | --- |
| F6 | Go to the next area: file list, sidebar, path bar, toolbar, search box, then back to the list |
| Shift+F6 | Go to the previous area |
| Tab · Shift+Tab | Go to the next · previous stop: each pane's file list (the other pane of a split view is the next stop), the sidebar, the path bar, each button of the toolbar, the search box and its panel, then back to the list. On the Home page each section (favorites, drives, network, recent files) is a stop. Menus, dialogs, rename and the Settings pages keep their own Tab |
| Ctrl+B | Show or hide the sidebar (F6 skips the sidebar while it is hidden) |
| Ctrl+L or Alt+D | Go straight to the path bar |
| Ctrl+F | Go straight to the search box |
| Esc | In the sidebar, the toolbar or the search panel: go back toward the file list |

## File list

Moving and selecting:

| Keys | Action |
| --- | --- |
| Up, Down | Previous or next item |
| Left, Right | In icon views: one item sideways. In details view with "Expand folders in the list" on: collapse or expand a folder |
| Page Up, Page Down | Move a page up or down |
| Home, End | First or last item |
| Shift+Up, Down, Page Up, Page Down, Home, End | Extend the selection |
| Ctrl+A | Select all |
| Ctrl+Space | Add or remove the item under the cursor from the selection |
| Type letters | Jump to the next item whose name starts with what you typed (type-to-jump). Space is part of the name while you are typing |
| Esc | Close Quick look if it is open, otherwise clear the filter, otherwise clear the selection |

Opening and navigating:

| Keys | Action |
| --- | --- |
| Enter | Open the item (a folder opens in the same tab) |
| Ctrl+Enter | Open the folder in a new tab |
| Shift+Enter | Open file location (from search results, go to the folder that holds the item) |
| Alt+Enter | Properties |
| Backspace or Alt+Up | Go up to the parent folder |
| Alt+Left, Alt+Right | Back, forward |
| F5 or Ctrl+R | Refresh |
| Space | Quick look (preview) of the selected item; press it again or Esc to close |
| Shift+F10 or the Menu key | Open the context menu for the selection |

Changing files:

| Keys | Action |
| --- | --- |
| F2 | Rename (with several items selected, rename them all; Tab and Shift+Tab move between names) |
| Delete | Move to the Recycle Bin |
| Shift+Delete | Delete permanently |
| Ctrl+C, Ctrl+X, Ctrl+V | Copy, cut, paste |
| Ctrl+Shift+C | Copy the path |
| Ctrl+Shift+N | New folder |
| Ctrl+Z | Undo the last file operation |
| Ctrl+Y or Ctrl+Shift+Z | Redo |
| Ctrl+Shift+A | Add to shelf |
| Ctrl+H | Show or hide hidden items |

In confirmation dialogs, Enter confirms and Esc cancels.

## Views

| Keys | Action |
| --- | --- |
| Ctrl+Shift+6 | Details view |
| Ctrl+Shift+1, 2, 3, 4 | Icons: extra large, large, medium, small |
| Ctrl+= , Ctrl+- , Ctrl+0 | Zoom the list in, out, reset |

Columns view and "Expand folders in the list" have no key by default; use the command palette or
assign a key in Settings.

## Sidebar

Press F6 to move into the sidebar. A cursor starts on the row for the folder you are in, and a
screen reader announces the row under it.

| Keys | Action |
| --- | --- |
| Up, Down | Previous or next row (favorites, Recycle Bin, section headings, folder tree) |
| Home, End | First or last row |
| Right | Expand a folder, or step into it when it is already expanded |
| Left | Collapse a folder, or go up to its parent |
| Enter or Space | Open the row; the keyboard then goes to the file list |
| Esc | Go back to the file list without opening anything |

## Toolbar

The toolbar is the row of buttons beside the path bar (Up, New folder, Sort, the views, Preview
pane, Split and the selection's actions). Press F6 until it is reached; it is one stop, and the
keyboard starts on its first button, which a screen reader announces.

| Keys | Action |
| --- | --- |
| Left, Right | Previous or next button |
| Home, End | First or last button |
| Space or Enter | Press the button |
| Esc | Close the Sort or View options panel if it is open, otherwise go back to the file list |

## Tabs and windows

| Keys | Action |
| --- | --- |
| Ctrl+T | Go to folder: a box with recent places, known locations and open tabs; Enter opens the choice in a new tab (or switches to the tab that already has it) |
| Ctrl+N | New window |
| Ctrl+W or Ctrl+F4 | Close the tab (in split view, the active side) |
| Ctrl+Shift+T | Reopen the closed tab |
| Ctrl+Tab or Ctrl+Page Down | Next tab |
| Ctrl+Shift+Tab or Ctrl+Page Up | Previous tab |
| Ctrl+1 … Ctrl+8 | Tab 1 to 8 |
| Ctrl+9 | Last tab |

"New tab", "Workspaces" and "Setup guide" have no key by default; use the command palette.

## Path bar

| Keys | Action |
| --- | --- |
| Ctrl+L or Alt+D | Edit the path |
| Type a path | Suggestions appear below |
| Up, Down | Move through the suggestions |
| Tab | Accept the highlighted suggestion |
| Enter | Go to the path (or to the highlighted suggestion) |
| Esc | Close the suggestions or leave the path bar |

## Search and filter

| Keys | Action |
| --- | --- |
| Ctrl+F | Filter or search this folder |
| Ctrl+Shift+F | Search this folder and its subfolders |
| Ctrl+K | Search this PC |
| Alt+S | Switch search mode |
| Down | From the box, move into the results |
| Tab, Shift+Tab | From the box into the search panel (scope and filters); inside it, row by row; back to the box |
| Up, Down, Left, Right, Home, End | Move inside the panel; the scopes are a radio group, Left and Right choose one |
| Enter or Space | Use the row under the cursor |
| Delete or Backspace | Remove the filter under the cursor |
| Esc | Close an open menu, then the panel |

The search syntax is described in the [README](../README.md#search-syntax).

## Go to folder and the command palette

Both open the same box.

| Keys | Action |
| --- | --- |
| Ctrl+T | Go to folder |
| Ctrl+Shift+P | Commands: the box starts with ">" and lists commands |
| Up, Down | Move through the rows |
| Enter | Open the folder, or run the command |
| Shift+Enter | Open the folder in a split beside the current tab |
| Esc | Close |

## Context menus and other menus

| Keys | Action |
| --- | --- |
| Up, Down | Move through the items |
| Right | Open a submenu |
| Left | Close the submenu |
| Enter | Run the item |
| Esc | Close the menu (or the submenu) |

Text boxes have a context menu too; open it with Shift+F10 or the Menu key and use the same keys.

## Split view

| Keys | Action |
| --- | --- |
| Tab · Shift+Tab | The other pane's file list is the next stop after this one's: Tab goes to it from the left pane, Shift+Tab from the right |
| Alt+C | Copy the selection to the other pane |
| Alt+M | Move the selection to the other pane |
| Ctrl+Shift+S | Synchronized browsing |
| Ctrl+Shift+D | Compare folders |

Open a folder in a split from Go to folder with Shift+Enter.

## Settings

| Keys | Action |
| --- | --- |
| Ctrl+, | Open Settings |
| Ctrl+F | Find a setting (the search box at the top) |
| Up, Down, Enter | In an open dropdown or the settings found: move and choose |
| Esc | Clear the search box, or close Settings |

Sections that expand and dropdowns open with Enter, or with the screen reader's default action.

## Recycle Bin, Properties and other pages

| Keys | Action |
| --- | --- |
| Up, Down | Move through the items (Recycle Bin) |
| Ctrl+A | Select all (Recycle Bin) |
| Enter | Confirm the highlighted choice |
| Delete | Delete the selected items permanently (Recycle Bin) |
| F5 | Reload (Recycle Bin) |
| Esc | Close the page or dialog (also the Properties window and the network connection dialog) |

## Text boxes

The path bar, search box, rename box and other text boxes use the usual Windows keys: Left and
Right, Ctrl+Left and Ctrl+Right by word, Home and End, Shift to select, Ctrl+A, Ctrl+C, Ctrl+X,
Ctrl+V (or Ctrl+Shift+V, which pastes plain text the same way), Ctrl+Z, Backspace, Delete, and Ctrl+Backspace or Ctrl+Delete by word. Enter confirms and
Esc cancels.

## Reporting a problem

If something cannot be reached or is not announced, open an
[issue](https://github.com/Koimsurai/Arke/issues) and say which screen reader and which version you
use, what you pressed and what you expected.
