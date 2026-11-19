# Patched upstream crates

Copies of crates.io packages with Arke's fixes on top. `[patch.crates-io]` in the workspace
`Cargo.toml` uses them instead of the published version. They keep their own license
(`LICENSE-APACHE`) and are listed in `THIRD_PARTY_LICENSES.md` like any other dependency.

When gpui-pre is upgraded:
1. Copy the new version over the folder.
2. Re-apply each change below.
3. Drop a change once upstream has it.

## gpui-pre-windows 0.3.11

Zed's Windows platform crate (Apache-2.0), unchanged except for:

- `src/directx_renderer.rs`: the two path intermediate textures (one of them 4x MSAA) are made on
  the first frame that draws a path, and dropped when the window is resized or on the next frame
  without paths, instead of being kept for every window at all times. Together they take 20 of the
  28 bytes per window pixel. On GPUs that share system memory (integrated graphics), that memory
  counts as the process's private bytes: about 70 MB for a maximized window on a 2880x1800 screen.
  Arke itself draws no paths (the progress ring is SVG layers). (#95)
- Idle vsync (`src/frame_gate.rs`, new; `src/platform.rs` `begin_vsync_thread`; `src/window.rs`
  `frame_waker`, `schedule_frame` and `draw`; `src/events.rs` `draw_window`, `report_visibility`,
  and the resize failure path). Upstream, the `VSyncProvider` thread invalidates every window on
  every vblank, so an idle Arke woke its UI thread about twice per refresh (about 1,300 context
  switches a second at 164 Hz), minimized, or with no window at all in the tray. GPUI's core
  already calls a window's `frame_waker` and `schedule_frame` whenever it needs a frame, and
  macOS uses that to pause its display link; Windows returned no waker. With this change:
  - A window goes idle after a vsync tick in which GPUI neither presented nor asked for a frame.
    A present keeps it ticking one more time, which serves anything deferred meanwhile.
  - The vsync thread skips idle, hidden and minimized windows, and when none is left it waits on
    an event instead of `DwmFlush`. The frame waker, `schedule_frame`, showing or restoring a
    window, a re-entrant draw that had to be deferred, and a failed resize all wake it.
  - While it sleeps, `ID3D11Device4::RegisterDeviceRemovedEvent` wakes it on a lost device, so
    recovery doesn't wait for the next frame.

  Worth offering upstream (Zed has the same loop).
- `src/window.rs` `WindowsDragDropHandler::Drop`: the dropped files are asked for again when the
  drop happens, not only when the drag enters. 7-Zip and NanaZip list placeholder paths during the
  drag and only extract to a temp folder when asked after the button is released (and delete it
  once the drop returns), so dropping from them moved files that didn't exist (#168). When the
  list changed, the drag is entered again with the new list before the drop.
- `src/virtual_drop.rs` (new) and `src/window.rs` `dropped_paths`, `DragEnter` and `Drop`: drops of
  virtual files, which a source describes with `FileGroupDescriptorW` (or the ANSI
  `FileGroupDescriptor`) and hands over through `FileContents` instead of listing files on disk
  (Outlook attachments, browser images, zip folders, phones). Upstream accepts only `CF_HDROP`, so
  these were refused (#206). Such a drop lists one marker path (`VIRTUAL_DROP_ROOT`), and while it
  is dispatched the app takes the data object with `take_dropped_data_object` to read the files
  (Arke's `win_virtual_drop.rs`). `CF_HDROP` still wins when a source offers both. A drag offering
  neither logs its formats, so a report says what the source offered.
- `src/events.rs` `WM_MOUSEACTIVATE` (`mouse_activate_answer`, `handle_mouse_activate_msg`,
  `handle_mouse_down_msg`, `handle_mouse_up_msg`) and `src/window.rs` `activate_on_release`: a left
  press in the client area of an inactive window answers `MA_NOACTIVATE`, and the window is
  activated when that button is released over it. Upstream activates on every press, so dragging a
  file from a maximized Arke behind a chat window brought Arke to the front, covering the window
  the file was meant for. File Explorer stays behind in the same case. Title bar, caption buttons,
  borders and the other buttons still activate at once. That press is reported with `first_mouse`
  set.
- `WindowKind::AnchoredPopup` (`src/window.rs` `WindowsWindow::new`, `popup_rect`, `activate`,
  `set_popup_mouse_passthrough`; `src/platform.rs` `open_window`; `src/events.rs`
  `handle_mouse_activate_msg`, `handle_hit_test_msg`). Upstream rejects popups on Windows, so
  menus could only be drawn inside the window they belong to: near its edge they were squeezed,
  and none could reach past it. A popup is now a `WS_POPUP` window owned by
  `PopupOptions::parent` (it stays above it without being topmost over other programs), with
  `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` (no taskbar button, no Alt+Tab entry). It is made at the
  `anchor` point of `anchor_rect` plus `offset`, extending toward `gravity`, sized from
  `window_bounds` at that monitor's scale, so it gets that monitor's DPI from the start.
  `constraint_adjustment` and `grab` are not applied: Arke places its menus itself against the
  monitor's work area and moves them with `SetWindowPos` (`crates/app/src/popup.rs`). A popup is
  never activated: `WM_MOUSEACTIVATE` answers `MA_NOACTIVATE`, and `activate` only shows it, so
  focus and the keyboard stay in the parent. It is all client area to hit testing, or
  `HTTRANSPARENT` after `set_popup_mouse_passthrough` (a closed menu fading out lets presses
  through to the window below). It takes no drops.
- `src/events.rs` `handle_capture_changed_msg`, `button_bit`, `handle_mouse_down_msg`,
  `handle_mouse_up_msg`, `handle_mouse_move_msg` and `src/window.rs` `pressed_buttons`: when
  another program takes the mouse capture in the middle of a press, each button still down gets a
  `MouseUp` at a point far outside the window with `click_count` 0. GPUI keeps a press armed (a
  click, or a drag once the mouse moves) until it hears a mouse up, and upstream never sent one
  when the release went to another window: after a double-click opened a file in a program that
  came up under the cursor, the next move with no button down dragged the file and the next
  click dropped it. Capture moving to a window of the same thread (menus, OLE drag and drop, the
  move loop) is left alone. Arke's `crates/app/src/press_guard.rs` sends the same release when a
  move or press shows a button is up, and treats `click_count` 0 as "cancelled".
