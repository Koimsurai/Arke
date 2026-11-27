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

- `src/platform.rs` `Platform::drag_threshold`: reports `SM_CXDRAG` and `SM_CYDRAG` (at 96 DPI, so
  logical pixels) for the drag threshold of gpui-pre (see there). Usually 4 px; GPUI's own was 2 px,
  twice as sensitive as File Explorer.
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
- Quieter log (`src/window.rs` `calculate_window_bounds` and `mouse_position`): a failed
  `GetWindowPlacement` is logged only while the window still exists (it fails for every window
  being closed), and a failed `GetCursorPos` (the screen is locked) is a debug line, not an
  error.
- Touchpad gestures over an idle window (`src/events.rs` `handle_dm_pointer_hit_test` and
  `draw_window`, `src/direct_manipulation.rs` `interacting`): Direct Manipulation's updates are
  only read in `draw_window`, and an idle window doesn't draw (see Idle vsync above). A touchpad
  contact now wakes the window, and the window keeps drawing while the gesture runs or glides
  on (`RUNNING`, `INERTIA`). Before, a two-finger scroll over a window that hadn't been clicked
  read one update and stopped until a click woke the window. (#258)
- `src/window.rs` `impl Drop for WindowsWindow`: a window closed from its title bar has already
  been destroyed by Windows when GPUI drops it, so hiding it, revoking its drop target and
  destroying it again each failed and logged an "invalid window handle" error (four per closed
  Properties window). They are skipped when `IsWindow` says the window is gone.
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
- Window classes and the integration API (`src/window.rs` `WINDOW_CLASS_NAME`, `POPUP_CLASS_NAME`,
  `window_class_for`, `register_window_class`, `set_copydata_handler`, `answer_copydata`,
  `window_procedure`; `src/gpui_windows.rs` exports). Upstream registers every window as
  `Zed::Window`. Normal windows are now `ArkeWindow`, the class other programs find Arke by
  (`ahk_class ArkeWindow`), and every other kind (menus, tooltips, dialogs) is
  `ArkeWindow.Popup`, so such a search never lands on a menu. The window procedure answers
  `WM_COPYDATA` tagged `COPYDATA_ID` ("ARKE") through the handler the app sets: it gets the
  request bytes, and its reply goes straight back to the sender's window (`wParam`) as a
  `WM_COPYDATA` with the same tag (`SendMessageTimeoutW`, so a hung sender can't hang Arke)
  before the message returns `TRUE`. This runs before the window state is looked at, and the
  handler must not touch GPUI (Arke answers from a snapshot, see `crates/app/src/api.rs` and
  `docs/integration.md`). Other `WM_COPYDATA` messages get the default handling. (#193)
- `src/direct_write.rs` `get_name`: a font with no name in the UI locale or in en-us (a Chinese
  font named only in zh-TW, on a zh-CN system) takes its first name instead of failing, so it is
  no longer left out of the font list with a "No localised string" error per font. (#343)
- `src/window.rs` `begin_modal_drag`, `DispatchingInput` and `on_input`, and `src/events.rs`
  `handle_hit_test_msg`: while an input callback runs (the app is borrowed then) or while Arke runs
  Windows' modal drag loop, `WM_NCHITTEST` doesn't ask the app where the window controls are.
  Asking could only fail: during a drag out of Arke it logged a "RefCell already borrowed" error on
  every mouse move. Arke's `crates/app/src/win_fileops.rs` `drag_out` holds the drag guard; the
  input count is kept by the wrapper `on_input` puts around the callback. (#338, #343)
- `src/window.rs` `a11y_init`: the accessibility adapter starts out knowing the window is focused
  when it is the active window (`GetActiveWindow`, what `WM_ACTIVATE` reports), not only when it
  is the foreground window. The first `WM_ACTIVATE` comes before the adapter exists, and the
  window isn't always the foreground one yet at that point; when it becomes so, no second
  `WM_ACTIVATE` follows. The adapter then thought the window never had focus, so screen readers
  were given no focused element: NVDA read nothing in the main window or Settings. (#322)
- `src/window.rs` `a11y_init` (second change) and `src/destination_list.rs`: some stripped
  Windows installs lack DLLs that Arke delay-loads, and a failed delayed load ends the process.
  `a11y_init` returns without making the adapter when `uiautomationcore.dll` can't be mapped from
  the system folder, so there is no screen reader support but also no crash (every other a11y
  path does nothing without an adapter). `create_destination_list` counts the description's
  UTF-16 units itself instead of calling ICU's `u_strlen`, so `icuuc.dll` isn't needed. (#462)
- Windows 10 top border, ported from Zed PR #63821 (not merged upstream yet; every hunk is marked
  `zed#63821`): `src/util.rs` `is_windows_10`, `top_border_color` and `configure_dwm_dark_mode`;
  `src/events.rs` `top_border_height`, `needs_win10_top_border`, `update_top_border`,
  `update_frame_margins`, `handle_size_msg`, `handle_activate_msg`, `handle_create_msg` and
  `handle_system_theme_changed`; `src/directx_renderer.rs` `DirectComposition`, `TopBorderVisual`,
  `draw`, `draw_top_border`, `set_top_border_visible` and `resize`; `src/window.rs`
  `WindowsWindowState::active`, `WindowsWindow::new` and `draw`. A window that hides its title bar
  gives the whole top edge to its client area, and Windows 10 (build < 22000) then draws no top
  border at all. The DWM frame is extended 1 px into the client area and the renderer clears that
  row, so DWM draws the active border there; while the window is inactive, a DirectComposition
  visual (one premultiplied pixel, `[43, 43, 43, 128]`, stretched to the window's width) draws it
  instead. These windows ask DWM for a dark frame in either theme, or the border is drawn white.
  None of it runs on Windows 11 except the root visual the scene now hangs off. Arke's own
  change to it: anchored popups (menus) are left out, since they have no frame and are never
  active. Re-apply on every GPUI upgrade until upstream has it, then take upstream's version.
  (#420)
- DirectX devices made ahead on another thread (`src/directx_devices.rs` `PendingDevices`;
  `src/platform.rs` `new`, `pending_devices`, `attach_pending`, `connect_initially`,
  `finish_attach`, `attach_gpu`, `detach_gpu`, `is_windowed`, `run` and `open_window`). Upstream
  makes the devices in `run` on the UI thread before the app's launch callback, and loading the
  driver and making the shaders took about 220 ms of a 370 ms cold start. `WindowsPlatform::new`
  now starts a `DirectXDevices` thread when the default windowed start can show windows (the
  check `connect_initially` makes). It makes the devices with the same `DirectXDevices::new` (same
  adapter loop from 0, same flags), hands them over, then makes the renderer's shaders while
  holding the device's shader cache. `run` no longer waits for them: the platform counts as
  windowed, and the first `open_window` (or the end of the launch callback, if it opened none)
  takes them and does the rest of `attach_gpu` (DirectWrite's GPU state, vsync) as before. So the
  work overlaps DirectWrite, the platform window and the app's launch callback, and the first
  window waits only for what isn't ready yet. If the app starts headless after all, the devices
  are dropped unused; if the thread can't start or panics, they are made on the UI thread. A
  failure still panics with "failed to initialize DirectX", now at the first window instead of
  before the launch callback. Device lost is handled as before. The log says how long the devices
  took and how long the UI thread waited for them.
- Shaders shared per device (`src/directx_renderer.rs` `PipelineShader`, `PipelineShaders`,
  `ShaderCache`, `PipelineState::new`, `DirectXRenderPipelines::new`, `DirectXRenderer::new`,
  `handle_device_lost_impl`; `src/directx_devices.rs` `DirectXDevices::shaders`). Upstream every
  window's renderer made the eight pipelines' sixteen shaders and their blend states again, so
  each menu level (an `AnchoredPopup` window) did too, and debug builds compiled them from
  source each time. They are made once per device and shared; each window only makes its
  instance buffers. New devices after a device lost start with an empty cache, which the first
  window recovering onto them fills. `RUST_LOG=gpui_windows=debug` logs each window renderer's
  creation time.

  Both worth offering upstream.
- Text locale (`src/direct_write.rs` `DirectWriteState::layout_locale`, `set_locale`,
  `layout_line_without_separators`, and the user locale read in `DirectWriteTextSystem::new`):
  text formats take the locale given to `PlatformTextSystem::set_locale` (see gpui-pre below)
  instead of always the user's (`GetUserDefaultLocaleName`, which `None` restores). DirectWrite's
  system font fallback picks fonts for characters missing from the UI font by that locale, so Han
  characters shared by Chinese, Japanese and Korean get the shapes of the app's UI language
  rather than the system's. This replaced Arke's hard-coded fallback list (YaHei, JhengHei, Yu
  Gothic in an order per UI language), which covered no other language. Font names are still read in the user's locale, and no font cache depends on the layout
  locale. The user locale is kept without the buffer's trailing NULs, so it compares equal to the
  same name passed in. Worth offering upstream with the gpui-pre half.

## gpui-pre 0.3.11

Zed's GPUI (Apache-2.0), unchanged except for:

- `src/platform.rs` `Platform::drag_threshold` (default method, 2 px per axis), `src/app.rs`
  `App::drag_threshold`, `src/elements/div.rs` (the check that turns a press into a drag) and
  `src/platform/test/platform.rs`, `src/app/test_context.rs` (`TestAppContext::set_drag_threshold`):
  how far the pointer must move before `on_drag` starts is asked of the platform, per axis (a
  rectangle centered on the press, like Windows' `SM_CXDRAG`/`SM_CYDRAG`), instead of the fixed 2 px
  radius. Other platforms keep 2 px. Arke's own thresholds (marquee, right-button drag) use the same
  value. Worth offering upstream.
- `src/elements/div.rs` `InteractiveElement::on_focused_bounds` (`Interactivity::focused_bounds_listener`,
  called in `Interactivity::prepaint`): a listener given the element's bounds each time it is
  prepainted while it has the focus. Arke's Settings scroll the control Tab moved to into view with
  it (`crates/app/src/keynav.rs`); upstream has no way to learn where an implicitly focusable element
  is. (#322)
- `src/elements/div.rs` `a11y_click_listeners` and the accessibility block of `Interactivity::paint`:
  an assistive technology's Click on an element with click listeners runs them, as Enter on the
  focused element does (`ClickEvent::Keyboard`). Upstream, Click falls back to a synthetic mouse
  click at the element's position: a button scrolled out of view (the sidebar's "Add network
  location…" in a short window, a Settings button below the fold) couldn't be pressed with a
  screen reader, and the click could land on whatever was drawn there instead. Elements that
  handle Click themselves (`on_a11y_action`) are left alone. (#322)
- `src/elements/div.rs` `StatefulInteractiveElement::a11y_key` and `a11y_parent` (applied in the
  accessibility block of `Interactivity::prepaint`) and `src/window/a11y.rs` `A11yNodeBuilder`
  `name_current`, `move_current_under` and `apply_moves` (run in `finalize`): an element's node
  can be put under a node named earlier in the frame instead of its nearest ancestor's, like
  `aria-owns`. Arke's sidebar draws its folder tree as a flat list of rows, so every row was a
  sibling of the others under the tree, and NVDA, which counts a tree item's level from the tree
  items above it rather than reading its level, read every folder as "level 1". Each row now goes
  under its parent folder's row (`crates/app/src/a11y.rs` `a11y_tree_row`), with no change to
  layout. (#322)
- `src/platform.rs` `PlatformTextSystem::set_locale` (default: unsupported, returns `false`) and
  `src/text_system.rs` `TextSystem::set_locale`: the app sets the locale text is laid out in, which
  picks locale-specific glyphs and fallback fonts (Han characters in Traditional or Simplified
  Chinese, Japanese or Korean shapes). Upstream always uses the user's locale, so an app whose UI
  language differs from it can't get its language's glyphs without naming fallback fonts. On a
  change, cached line layouts are dropped (the font generation `add_fonts` also bumps) and the
  line wrappers' char width caches are emptied (the pool's entries stay, since handles out of it
  put their cache back under the same key). Arke calls it with `win::text_locale` when the UI
  language is applied (`crates/app/src/main.rs` `apply_language`).

The test platform, which only builds with the `test-support` feature:

- `src/platform/test/window.rs`: `TestWindow` keeps the accessibility callbacks `a11y_init` is
  given (upstream it ignores them, so a test window never builds an accessibility tree), and adds
  `simulate_a11y_activation`, `simulate_a11y_deactivation` and `simulate_a11y_action`.
- `src/app/test_context.rs`: `TestAppContext::simulate_window_a11y_activation` and
  `simulate_window_a11y_action`, and `VisualTestContext::simulate_a11y_activation` and
  `simulate_a11y_action`, which run until parked.

With them, tests read what a screen reader is given through `Window::debug_a11y_tree_json`
(`crates/app/src/tests_a11y.rs`, #322).

The `examples`, `tests` and `benches` folders and their
`Cargo.toml` entries are left out. Offered upstream; drop this copy once a gpui-pre has it.
