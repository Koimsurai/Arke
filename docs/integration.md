# Integration API

Launchers, dialog helpers and scripts (ALTRun, Listary, AutoHotkey, PowerShell...) can read what
Arke shows and ask it to open a folder or run a command. This page is the whole interface: one
JSON format, reachable two ways.

## Principles

- There is only this one interface. Arke has no code for any single tool.
- Two things are guaranteed: the JSON format (version 1) and the command ids, which are the same
  ids Settings › Keyboard shortcuts stores, so they don't change anyway.
- Additions only: later versions add fields, location kinds, commands and error codes, never
  change or remove them. Ignore fields you don't know. Only a change that can't be made that way
  gets a version 2, and version 1 keeps working alongside it for a while.
- Nothing that changes files is available: no delete, move, paste, new folder, undo or redo.
- Local only: no network, no settings to turn it on.

## Finding Arke's windows

- Every Arke window has the window class `ArkeWindow`. Menus, tooltips and other popups have a
  different class (`ArkeWindow.Popup`), so a search for `ArkeWindow` never finds one.
- Windows are in Windows' Z-order. The Arke window used last is the frontmost `ArkeWindow`
  (`WinExist("ahk_class ArkeWindow")` in AutoHotkey, `FindWindow(L"ArkeWindow", NULL)` in C).

## Two channels, the same JSON

### 1. `WM_COPYDATA` (fast, for dialog jumps and hotkeys)

- Request: send `WM_COPYDATA` to an Arke window, with `wParam` = your own window and
  `COPYDATASTRUCT`:
  - `dwData` = `0x454B5241` ("ARKE")
  - `lpData` / `cbData` = the request, UTF-8 JSON (a terminating NUL is optional)
- Reply: while it handles the message, Arke sends a `WM_COPYDATA` back to your window (the
  `wParam` you passed), with `dwData` = `0x454B5241` and the UTF-8 JSON reply (no terminating
  NUL), then returns `TRUE`. It is the way Total Commander answers; no process is started.
- Use `SendMessageTimeout` with `SMTO_ABORTIFHUNG`, so a busy Arke never holds you up.
- `WM_COPYDATA` with any other `dwData` is not answered (the message returns `FALSE`).
- `open` and `run` act on the window the message was sent to. A message sent to a window that
  isn't one of Arke's main windows (Properties) acts on the window used last.
- Windows doesn't let a program send `WM_COPYDATA` to a program running as administrator unless
  it runs as administrator too.

### 2. The command line (for scripts)

```
arke.exe --api "{\"v\":1,\"cmd\":\"state\"}"
echo {"v":1,"cmd":"state"} | arke.exe --api -
```

- `--api -` reads the request from standard input (cmd and most shells), which avoids quoting it
  on the command line.
- The reply is printed to standard output as one line of UTF-8 JSON. The exit code is 0, or 1
  for an error reply.
- When Arke is already running, the command asks it through Arke's single instance pipe.
- When no Arke is running, `state` replies `{"v":1,"windows":[]}` and `commands` is answered
  without starting Arke. `open` and `run` start Arke in the background first, and then do what
  was asked.
- `open` and `run` act on the Arke window used last.
- A relative `path` is taken from the folder the command runs in.
- `arke.exe "D:\Folder"` keeps working as before (in single window mode it opens a tab in the
  window used last).
- `arke.exe --search "text"` opens a new window that searches the whole computer for `text`
  (the same as Ctrl+K and typing it). It starts Arke when none is running.

## Requests

Every request is one JSON object: `{"v":1,"cmd":"...", ...parameters}`. Unknown fields are
ignored.

| `cmd` | Parameters | What it does |
| --- | --- | --- |
| `state` | none | Replies with every window, its tabs and panes, what each shows and what is selected |
| `open` | `path` (required), `new_tab` (default `false`), `select` (array of names, optional) | Shows `path` in the active pane of the window (or in a new tab with `new_tab`) and brings the window to the front; `select` selects those items once the folder is listed |
| `run` | `id` (a command id) | Runs a command palette command in the window's active pane |
| `commands` | none | Lists the ids and names `run` takes, so tools don't have to hard-code them |

`open` details:

- `path` is a full path: a folder, a drive (`D:\`), a share (`\\server\share`) or a server
  location Arke can open (`sftp://...`).
- A file's path opens its folder with the file selected.
- `select` holds names of items in that folder (`"report.docx"`); a full path counts as its name.
- With no window open (only the notification area icon), a window opens on `path`.

## Replies

### `state`

```json
{
  "v": 1,
  "windows": [
    {
      "hwnd": "0x1A2B3C",
      "active": true,
      "tabs": [
        {
          "active": true,
          "panes": [
            {
              "active": true,
              "location": { "kind": "folder", "path": "D:\\Work" },
              "selection": ["D:\\Work\\report.docx", "D:\\Work\\notes.txt"]
            },
            {
              "active": false,
              "location": { "kind": "folder", "path": "E:\\Backup" },
              "selection": []
            }
          ]
        },
        {
          "active": false,
          "panes": [
            { "active": true, "location": { "kind": "home" }, "selection": [] }
          ]
        }
      ]
    }
  ]
}
```

- The structure is Arke's own: windows, their tabs, and each tab's panes (two in split view).
- Each level has exactly one `active: true`. `windows` are front to back (Z-order), so the first,
  active window is the one used last.
- `hwnd` is the window's handle in hex, for sending it `WM_COPYDATA` or activating it.
- `selection` holds full paths (URLs on a server), in list order. In search results it is the
  result under the cursor, also as a full path.
- Keys may come in any order.

`location.kind` (more kinds may be added):

| `kind` | Other fields | Meaning |
| --- | --- | --- |
| `folder` | `path` | A folder, drive or network share (`\\server\share`) |
| `remote` | `url` | An SFTP or WebDAV location |
| `search` | `query`, and `path` (what is searched; absent when searching the whole PC) | Search results |
| `home` | none | Home |
| `recycle_bin` | none | The Recycle Bin |
| `other` | none | Settings, What's new and other pages |

A tool that needs "a folder path to use" (a dialog jump, say) takes the active pane's `path` when
its `kind` is `folder` and treats every other kind as no folder.

### `open`, `run`

```json
{ "v": 1, "ok": true }
```

`ok` means the request was accepted: the path exists (or couldn't be checked in time, a share
that doesn't answer, say) and the command is known and allowed. Opening and running then happen
right after the reply. A `run` of a command that moves keyboard focus into Arke (the command
palette, the path bar, the search boxes, Settings...) also brings the window to the front; other
commands leave the foreground alone.

### `commands`

```json
{ "v": 1, "commands": [ { "id": "new_tab", "name": "New tab" }, { "id": "refresh", "name": "Refresh" } ] }
```

`name` is in Arke's interface language.

### Errors

```json
{ "v": 1, "error": { "code": "unknown_command", "message": "no command has the id \"x\"" } }
```

| `code` | When |
| --- | --- |
| `bad_request` | Not valid JSON, not an object, a field missing or of the wrong type, or an `open` path that isn't a full path |
| `unknown_command` | An unknown `cmd`, or a `run` id no command has |
| `unsupported_version` | `v` is not `1` |
| `not_allowed` | `run` of a command that changes files |
| `not_found` | `open` of a path that doesn't exist |
| `unavailable` | Command line only: Arke is running but didn't answer in time |

`message` is English text for people; tools should look at `code`.

## Commands `run` doesn't take

`paste`, `delete`, `delete_permanently`, `new_folder`, `undo`, `redo`, `copy_to_peer`,
`move_to_peer`. They change files, so `run` replies `not_allowed` and `commands` doesn't list
them. Commands added later that change files are left out the same way.

## Examples

### AutoHotkey v2

The current folder of the Arke window used last (`JSON.Load` is from any JSON library for
AutoHotkey v2):

```ahk
ArkeFolder() {
    hwnd := WinExist("ahk_class ArkeWindow")
    if !hwnd
        return ""
    state := JSON.Load(ArkeAsk(hwnd, '{"v":1,"cmd":"state"}'))
    for w in state["windows"]
        if w["active"]
            for t in w["tabs"]
                if t["active"]
                    for p in t["panes"]
                        if p["active"] && p["location"]["kind"] = "folder"
                            return p["location"]["path"]
    return ""
}

; Sends a request and returns the reply, as with Total Commander.
ArkeAsk(hwnd, request) {
    global ArkeReplyText := ""
    size := StrPut(request, "UTF-8") - 1
    data := Buffer(size + 1)
    StrPut(request, data, "UTF-8")
    cds := Buffer(A_PtrSize * 3, 0)
    NumPut("UPtr", 0x454B5241, cds, 0)
    NumPut("UInt", size, cds, A_PtrSize)
    NumPut("Ptr", data.Ptr, cds, A_PtrSize * 2)
    OnMessage(0x4A, ArkeReply)
    ; SMTO_ABORTIFHUNG, at most 5 seconds
    DllCall("SendMessageTimeoutW", "Ptr", hwnd, "UInt", 0x4A, "Ptr", A_ScriptHwnd
        , "Ptr", cds.Ptr, "UInt", 0x2, "UInt", 5000, "Ptr*", &result := 0)
    OnMessage(0x4A, ArkeReply, 0)
    return ArkeReplyText
}

ArkeReply(wParam, lParam, msg, hwnd) {
    global ArkeReplyText
    if NumGet(lParam, 0, "UPtr") != 0x454B5241
        return
    size := NumGet(lParam, A_PtrSize, "UInt")
    ArkeReplyText := StrGet(NumGet(lParam, A_PtrSize * 2, "Ptr"), size, "UTF-8")
    return true
}
```

Open a folder in Arke with two files selected:

```ahk
ArkeAsk(WinExist("ahk_class ArkeWindow")
    , '{"v":1,"cmd":"open","path":"D:\\Work","select":["report.docx","notes.txt"]}')
```

### PowerShell

Write the JSON as it is, in Windows PowerShell 5.1 and PowerShell 7 alike. (5.1 drops the
quotes inside an argument it passes to a program; Arke then reads the request from its raw
command line, where it is intact.)

The folders of every tab:

```powershell
[Console]::OutputEncoding = [Text.Encoding]::UTF8   # non-ASCII paths
$state = arke.exe --api '{"v":1,"cmd":"state"}' | ConvertFrom-Json
$state.windows.tabs.panes.location | Where-Object kind -eq 'folder' | Select-Object -ExpandProperty path
```

Build requests with `ConvertTo-Json` to get paths escaped right:

```powershell
$request = @{ v = 1; cmd = 'open'; path = 'D:\Work'; new_tab = $true } | ConvertTo-Json -Compress
arke.exe --api $request
```
