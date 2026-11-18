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
- `src/events.rs` `WM_MOUSEACTIVATE` (`mouse_activate_answer`, `handle_mouse_activate_msg`,
  `handle_mouse_down_msg`, `handle_mouse_up_msg`) and `src/window.rs` `activate_on_release`: a left
  press in the client area of an inactive window answers `MA_NOACTIVATE`, and the window is
  activated when that button is released over it. Upstream activates on every press, so dragging a
  file from a maximized Arke behind a chat window brought Arke to the front, covering the window
  the file was meant for. File Explorer stays behind in the same case. Title bar, caption buttons,
  borders and the other buttons still activate at once. That press is reported with `first_mouse`
  set.
