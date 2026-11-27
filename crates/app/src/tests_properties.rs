//! GPUI tests for the Properties window (`properties.rs`): what it shows for one item, several
//! items, folders and drives, and that every change applies at once and reports failures.

use gpui::Modifiers;

use super::*;
use crate::properties::{Properties, Tri};
use crate::shell::{ATTR_HIDDEN, ATTR_READONLY};

pub(super) const PLACEHOLDER: u32 = 0x40_0000;

pub(super) fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

pub(super) fn services(fs: &Arc<FakeFs>, shell: &Arc<FakeShell>) -> Services {
    Services {
        fs: Arc::clone(fs) as Arc<dyn arke_core::fs::Fs>,
        shell: Arc::clone(shell) as Arc<dyn Shell>,
        remotes: crate::remote_shell::test_remotes(),
    }
}

/// A Properties window for `targets` (path, is a folder).
pub(super) fn props<'a>(
    cx: &'a mut TestAppContext,
    fs: &Arc<FakeFs>,
    shell: &Arc<FakeShell>,
    targets: &[(&str, bool)],
) -> (Entity<Properties>, &'a mut VisualTestContext) {
    arke_core::fs::mark_ui_thread();
    rust_i18n::set_locale("en");
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::text_input::bind_keys(cx);
        crate::explorer::bind_keys(cx);
    });
    let services = services(fs, shell);
    let targets: Vec<(PathBuf, bool)> = targets
        .iter()
        .map(|(p, dir)| (PathBuf::from(p), *dir))
        .collect();
    cx.add_window_view(move |window, cx| Properties::new(services, targets, window, cx))
}

pub(super) fn wait_for(
    cx: &mut VisualTestContext,
    view: &Entity<Properties>,
    what: &str,
    pred: impl Fn(&Properties) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if view.update(cx, |v, _| pred(v)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn ready(cx: &mut VisualTestContext, view: &Entity<Properties>) {
    wait_for(
        cx,
        view,
        "the items to be read",
        Properties::is_loaded_for_test,
    );
}

pub(super) fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} should be drawn"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

pub(super) fn info(
    cx: &mut VisualTestContext,
    view: &Entity<Properties>,
    id: &str,
) -> Option<String> {
    view.update(cx, |v, _| {
        v.info_for_test()
            .into_iter()
            .find(|(i, _, _)| *i == id)
            .map(|(_, _, value)| value)
    })
}

pub(super) fn errors(cx: &mut VisualTestContext, view: &Entity<Properties>) -> Vec<String> {
    view.update(cx, |v, _| {
        v.errors_for_test()
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect()
    })
}

pub(super) fn file_fs() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\p",
        vec![
            Entry::new("a.txt", EntryKind::File, 5000, Some(at(1_600_000_000)), 0)
                .with_created(Some(at(1_500_000_000))),
            Entry::new(
                "b.txt",
                EntryKind::File,
                100,
                Some(at(1_600_000_000)),
                ATTR_HIDDEN,
            ),
            Entry::new("c.txt", EntryKind::File, 200, Some(at(1_600_000_000)), 0),
            dir("sub"),
        ],
    );
    fs
}

/// One file: name, type, where it is, both sizes, the three times and the program it opens with,
/// all from the listing and the shell; nothing is changed by looking.
#[gpui::test]
fn a_file_shows_its_details(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);

    assert_eq!(
        info(cx, &view, "props-location").as_deref(),
        Some(r"C:\p"),
        "the folder it is in"
    );
    assert_eq!(
        info(cx, &view, "props-size").as_deref(),
        Some("4.9 KB (5,000 bytes)")
    );
    assert_eq!(
        info(cx, &view, "props-ondisk").as_deref(),
        Some("8.0 KB (8,192 bytes)"),
        "Windows' own number for the space it takes"
    );
    for id in ["props-created", "props-modified", "props-accessed"] {
        assert_eq!(
            info(cx, &view, id).as_deref(),
            Some("2026-01-01 00:00"),
            "{id}"
        );
    }
    assert_eq!(
        view.update(cx, |v, _| v.app_for_test()).as_deref(),
        Some("Notepad")
    );
    let name = view.update(cx, |v, cx| {
        v.name_input_for_test().read(cx).text().to_owned()
    });
    assert_eq!(name, "a.txt");
    assert!(cx.debug_bounds("props-change-app").is_some());
    assert!(cx.debug_bounds("props-readonly").is_some());
    assert!(
        shell.attribute_calls.lock().is_empty(),
        "looking changes nothing"
    );
    assert!(shell.ops().is_empty());
}

/// Several items: the total of their sizes, check boxes that say "some" when the items differ, and
/// a click that sets the attribute on all of them (and only that attribute).
#[gpui::test]
fn several_items_share_attributes(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(
        cx,
        &fs,
        &shell,
        &[
            (r"C:\p\a.txt", false),
            (r"C:\p\b.txt", false),
            (r"C:\p\c.txt", false),
        ],
    );
    ready(cx, &view);
    wait_for(cx, &view, "the total", |v| !v.is_counting_for_test());

    assert_eq!(
        info(cx, &view, "props-size").as_deref(),
        Some("5.2 KB (5,300 bytes)")
    );
    assert_eq!(info(cx, &view, "props-location").as_deref(), Some(r"C:\p"));
    assert!(
        cx.debug_bounds("props-name").is_none(),
        "several items have no name to edit"
    );
    assert!(cx.debug_bounds("props-change-app").is_none());
    assert_eq!(view.update(cx, |v, _| v.attribute(ATTR_HIDDEN)), Tri::Mixed);
    assert_eq!(view.update(cx, |v, _| v.attribute(ATTR_READONLY)), Tri::Off);

    click(cx, "props-hidden");
    wait_for(cx, &view, "the change", |v| {
        // The change runs on its own thread: wait for it to reach the shell too.
        v.attribute(ATTR_HIDDEN) == Tri::On && !shell.attribute_calls.lock().is_empty()
    });
    assert_eq!(
        *shell.attribute_calls.lock(),
        vec![(
            vec![
                PathBuf::from(r"C:\p\a.txt"),
                PathBuf::from(r"C:\p\b.txt"),
                PathBuf::from(r"C:\p\c.txt"),
            ],
            ATTR_HIDDEN,
            0
        )],
        "only the attribute that was clicked, on every item"
    );

    click(cx, "props-hidden");
    wait_for(cx, &view, "the change back", |v| {
        v.attribute(ATTR_HIDDEN) == Tri::Off && shell.attribute_calls.lock().len() == 2
    });
    assert_eq!(
        shell.attribute_calls.lock().last().map(|c| (c.1, c.2)),
        Some((0, ATTR_HIDDEN))
    );
    click(cx, "props-readonly");
    assert_eq!(view.update(cx, |v, _| v.attribute(ATTR_READONLY)), Tri::On);
    wait_for(cx, &view, "the read-only change", |_| {
        shell.attribute_calls.lock().len() == 3
    });
    assert_eq!(
        shell.attribute_calls.lock().last().map(|c| (c.1, c.2)),
        Some((ATTR_READONLY, 0))
    );
    assert!(errors(cx, &view).is_empty());
}

/// A refused change puts the check box back and says why under it.
#[gpui::test]
fn a_refused_attribute_shows_the_error(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    shell.fail_attributes.store(true, Ordering::Relaxed);
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);

    click(cx, "props-readonly");
    wait_for(cx, &view, "the error", |v| v.errors_for_test()[1].is_some());
    assert_eq!(view.update(cx, |v, _| v.attribute(ATTR_READONLY)), Tri::Off);
    let shown = errors(cx, &view);
    assert_eq!(shown.len(), 1);
    assert!(shown[0].contains("access denied"), "{shown:?}");
    assert!(
        cx.debug_bounds("props-attr-error").is_some(),
        "drawn under the check boxes"
    );
}

/// A folder is counted in the background: the numbers arrive, with files, folders and the space
/// they take.
#[gpui::test]
fn a_folder_is_counted_in_the_background(cx: &mut TestAppContext) {
    let fs = file_fs();
    fs.insert_dir(
        r"C:\p\sub",
        vec![
            dir("deep"),
            Entry::new("x", EntryKind::File, 4097, None, 0),
            Entry::new("cloud", EntryKind::File, 9000, None, PLACEHOLDER),
        ],
    );
    fs.insert_dir(r"C:\p\sub\deep", vec![file("y", 10)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\sub", true)]);
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());

    assert_eq!(
        info(cx, &view, "props-size").as_deref(),
        Some("13 KB (13,107 bytes)")
    );
    // 4097 -> 8192, a cloud file that is not here -> 0, 10 -> 4096 (4 KB clusters)
    assert_eq!(
        info(cx, &view, "props-ondisk").as_deref(),
        Some("12 KB (12,288 bytes)")
    );
    assert_eq!(
        info(cx, &view, "props-contains").as_deref(),
        Some("3 files, 1 folder")
    );
    assert!(
        cx.debug_bounds("props-stop").is_none(),
        "nothing left to stop"
    );
}

/// While a folder is being walked its size counts up; Stop ends it and what arrives late is
/// ignored.
#[gpui::test]
fn counting_can_be_stopped(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\big", vec![dir("slow"), file("top", 1000)]);
    fs.insert_dir(r"C:\big\slow", vec![file("late", 5)]);
    fs.hang(r"C:\big\slow");
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\big", true)]);
    wait_for(cx, &view, "the first count", |v| {
        v.usage_for_test().is_some()
    });

    assert!(view.update(cx, |v, _| v.is_counting_for_test()));
    assert_eq!(
        view.update(cx, |v, _| v.usage_for_test().map(|u| u.bytes)),
        Some(1000),
        "what is counted so far shows while the rest is walked"
    );
    let size = info(cx, &view, "props-size").unwrap();
    assert!(size.ends_with('…'), "{size}");
    click(cx, "props-stop");
    assert!(view.update(cx, |v, _| v.is_stopped_for_test()));

    fs.release(Path::new(r"C:\big\slow"));
    std::thread::sleep(Duration::from_millis(100));
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| v.usage_for_test().map(|u| u.bytes)),
        Some(1000),
        "the walk ended"
    );
    assert!(view.update(cx, |v, _| v.is_stopped_for_test()));
    assert!(cx.debug_bounds("props-stop").is_none());
}

/// A cloud file that is not on this device: no icon is read for it, no program is asked about its
/// content, and it takes no space on disk.
#[gpui::test]
fn a_cloud_placeholder_is_never_opened(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\cloud",
        vec![Entry::new(
            "setup.exe",
            EntryKind::File,
            7_000_000,
            Some(at(1)),
            PLACEHOLDER,
        )],
    );
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\cloud\setup.exe", false)]);
    ready(cx, &view);
    wait_for(cx, &view, "the count", |v| !v.is_counting_for_test());

    assert!(
        shell.icons.lock().is_empty(),
        "an .exe placeholder gets no icon read from it"
    );
    assert_eq!(
        info(cx, &view, "props-ondisk").as_deref(),
        Some("0 bytes"),
        "even if the shell offered a number"
    );
    assert_eq!(
        info(cx, &view, "props-size").as_deref(),
        Some("6.7 MB (7,000,000 bytes)")
    );
}

/// Enter renames through the same file operation as the list (so it can be undone), after the
/// name was checked; a bad name is explained under the box and nothing runs.
#[gpui::test]
fn the_name_is_edited_in_place(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    let input = view.update(cx, |v, _| v.name_input_for_test());
    let focus = input.read_with(cx, gpui::Focusable::focus_handle);
    cx.update(|window, cx| window.focus(&focus, cx));

    input.update(cx, |i, cx| i.set_text("a/b.txt", cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.ops().is_empty(), "a name with a slash is not used");
    assert_eq!(errors(cx, &view).len(), 1);
    assert!(cx.debug_bounds("props-name-error").is_some());

    // Esc puts the old name back and the message goes.
    cx.simulate_keystrokes("escape");
    assert_eq!(input.read_with(cx, |i, _| i.text().to_owned()), "a.txt");
    assert!(errors(cx, &view).is_empty());

    input.update(cx, |i, cx| i.set_text("renamed.txt", cx));
    cx.simulate_keystrokes("enter");
    wait_for(cx, &view, "the new name", |v| {
        v.path_for_test() == Some(PathBuf::from(r"C:\p\renamed.txt"))
    });
    assert_eq!(
        shell.ops(),
        vec![FileOp::Rename {
            path: PathBuf::from(r"C:\p\a.txt"),
            new_name: "renamed.txt".into()
        }]
    );
    assert!(errors(cx, &view).is_empty());
}

/// The same name is no change: nothing runs.
#[gpui::test]
fn an_unchanged_name_runs_nothing(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    let input = view.update(cx, |v, _| v.name_input_for_test());
    let focus = input.read_with(cx, gpui::Focusable::focus_handle);
    cx.update(|window, cx| window.focus(&focus, cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.ops().is_empty());
}

/// "Windows properties..." hands the items to Windows' own window; "Change..." opens Windows'
/// "Choose another app" and shows the program picked.
#[gpui::test]
fn windows_own_dialogs_are_one_button_away(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(
        cx,
        &fs,
        &shell,
        &[(r"C:\p\a.txt", false), (r"C:\p\c.txt", false)],
    );
    ready(cx, &view);
    click(cx, "props-native");
    wait_for(cx, &view, "the call", |_| {
        !shell.native_properties.lock().is_empty()
    });
    assert_eq!(
        *shell.native_properties.lock(),
        vec![vec![
            PathBuf::from(r"C:\p\a.txt"),
            PathBuf::from(r"C:\p\c.txt")
        ]]
    );
}

#[gpui::test]
fn the_program_can_be_changed(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    assert_eq!(
        view.update(cx, |v, _| v.app_for_test()).as_deref(),
        Some("Notepad")
    );
    click(cx, "props-change-app");
    wait_for(cx, &view, "the new program", |v| {
        v.app_for_test().as_deref() == Some("Paint")
    });
    assert_eq!(*shell.chose_app.lock(), vec![PathBuf::from(r"C:\p\a.txt")]);
    assert!(shell.native_properties.lock().is_empty());
}

/// A shortcut shows where it points.
#[gpui::test]
fn a_shortcut_shows_its_target(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\p", vec![file("go.lnk", 900)]);
    let shell = Arc::new(FakeShell::default());
    shell.shortcuts.lock().push((
        PathBuf::from(r"C:\p\go.lnk"),
        PathBuf::from(r"D:\tools\app.exe"),
        false,
    ));
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\go.lnk", false)]);
    ready(cx, &view);
    assert_eq!(
        view.update(cx, |v, _| v.target_for_test()),
        Some(PathBuf::from(r"D:\tools\app.exe"))
    );
    assert!(cx.debug_bounds("props-target").is_some());
}

/// A drive: capacity bar with used and free space and the file system, no attributes.
#[gpui::test]
fn a_drive_shows_its_capacity(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let shell = Arc::new(FakeShell::default());
    shell
        .disk_free
        .lock()
        .push((PathBuf::from(r"C:\"), (250_000_000_000, 1_000_000_000_000)));
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\", true)]);
    ready(cx, &view);
    assert_eq!(
        view.update(cx, |v, _| v.drive_for_test()),
        Some((
            "Data".into(),
            "NTFS".into(),
            250_000_000_000,
            1_000_000_000_000
        ))
    );
    assert!(cx.debug_bounds("props-capacity").is_some());
    let used = cx.debug_bounds("props-capacity-used").unwrap();
    let whole = cx.debug_bounds("props-capacity").unwrap();
    let share = used.size.width / whole.size.width;
    assert!((share - 0.75).abs() < 0.02, "75% used, drawn {share}");
    assert!(cx.debug_bounds("props-file-system").is_some());
    assert!(
        cx.debug_bounds("props-readonly").is_none(),
        "a drive has no attributes here"
    );
    assert!(
        cx.debug_bounds("props-name").is_none(),
        "nor a name to edit"
    );
}

/// Esc closes the window.
#[gpui::test]
fn escape_closes_the_window(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\p\a.txt", false)]);
    ready(cx, &view);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.windows().is_empty(), "the window is gone");
}

/// A window cancels a count that is still running when it closes.
#[gpui::test]
fn closing_the_window_stops_the_count(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\big", vec![dir("slow"), file("top", 1)]);
    fs.insert_dir(r"C:\big\slow", vec![file("late", 5)]);
    fs.hang(r"C:\big\slow");
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = props(cx, &fs, &shell, &[(r"C:\big", true)]);
    wait_for(cx, &view, "the first count", |v| {
        v.usage_for_test().is_some()
    });
    let cancel = view.update(cx, |v, _| v.cancel_for_test());
    assert!(!cancel.load(Ordering::Relaxed));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.run_until_parked();
    assert!(cancel.load(Ordering::Relaxed));
    fs.release(Path::new(r"C:\big\slow"));
}

/// Alt+Enter in a folder opens a window for the selection, one per request; with nothing selected
/// it is the folder itself.
#[gpui::test]
fn alt_enter_opens_the_window(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "the folder", loaded(r"C:\p"));
    let windows = |cx: &mut VisualTestContext| {
        cx.windows()
            .into_iter()
            .filter_map(|w| w.downcast::<Properties>())
            .collect::<Vec<_>>()
    };
    assert!(windows(cx).is_empty());

    let row = cx.debug_bounds("name:a.txt").expect("the row is drawn");
    cx.simulate_click(row.center(), Modifiers::none());
    cx.run_until_parked();
    cx.simulate_keystrokes("alt-enter");
    cx.run_until_parked();
    let first = windows(cx);
    assert_eq!(first.len(), 1, "a window of its own");
    let shown = first[0].read_with(cx, |p, _| p.path_for_test()).unwrap();
    assert_eq!(shown, Some(PathBuf::from(r"C:\p\a.txt")));

    // The list still has the focus; a second request opens a second window.
    cx.simulate_keystrokes("alt-enter");
    cx.run_until_parked();
    assert_eq!(windows(cx).len(), 2, "several can be open");
}

/// #337: Alt+double-click opens the Properties window for the item, instead of opening it.
#[gpui::test]
fn alt_double_click_opens_the_window(cx: &mut TestAppContext) {
    use gpui::{MouseButton, MouseDownEvent, MouseUpEvent};
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "the folder", loaded(r"C:\p"));
    let row = cx
        .debug_bounds("name:a.txt")
        .expect("the row is drawn")
        .center();
    let alt = Modifiers {
        alt: true,
        ..Modifiers::default()
    };
    cx.simulate_click(row, alt);
    cx.simulate_event(MouseDownEvent {
        position: row,
        modifiers: alt,
        button: MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position: row,
        modifiers: alt,
        button: MouseButton::Left,
        click_count: 2,
    });
    cx.run_until_parked();
    let windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Properties>())
        .collect();
    assert_eq!(windows.len(), 1, "the Properties window");
    let shown = windows[0].read_with(cx, |p, _| p.path_for_test()).unwrap();
    assert_eq!(shown, Some(PathBuf::from(r"C:\p\a.txt")));
    assert!(shell.opened.lock().is_empty(), "not opened");
}

/// Presses and releases the left button on `at` with this click count and modifiers.
fn press_release(
    cx: &mut VisualTestContext,
    at: gpui::Point<gpui::Pixels>,
    m: Modifiers,
    n: usize,
) {
    use gpui::{MouseButton, MouseDownEvent, MouseUpEvent};
    cx.simulate_event(MouseDownEvent {
        position: at,
        modifiers: m,
        button: MouseButton::Left,
        click_count: n,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position: at,
        modifiers: m,
        button: MouseButton::Left,
        click_count: n,
    });
    cx.run_until_parked();
}

fn properties_windows(cx: &mut VisualTestContext) -> Vec<gpui::WindowHandle<Properties>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Properties>())
        .collect()
}

fn set_single_click(on: bool) {
    crate::prefs::apply(&crate::settings::Settings {
        single_click: on,
        ..crate::settings::Settings::default()
    });
}

/// #493: with "Open items with a single click", Alt+click opens the Properties window for the
/// item, without opening it.
#[gpui::test]
fn alt_click_opens_the_window_in_single_click_mode(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "the folder", loaded(r"C:\p"));
    set_single_click(true);
    let row = cx
        .debug_bounds("name:a.txt")
        .expect("the row is drawn")
        .center();
    let alt = Modifiers {
        alt: true,
        ..Modifiers::default()
    };
    press_release(cx, row, alt, 1);
    let windows = properties_windows(cx);
    set_single_click(false);
    assert_eq!(windows.len(), 1, "the Properties window");
    let shown = windows[0].read_with(cx, |p, _| p.path_for_test()).unwrap();
    assert_eq!(shown, Some(PathBuf::from(r"C:\p\a.txt")));
    assert!(shell.opened.lock().is_empty(), "not opened");
    assert!(view.update(cx, |v, _| loaded(r"C:\p")(v)), "not navigated");
}

/// #493: Alt+double-click in single-click mode opens one Properties window, not two, and does
/// not open the item.
#[gpui::test]
fn alt_double_click_opens_one_window_in_single_click_mode(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "the folder", loaded(r"C:\p"));
    set_single_click(true);
    let row = cx
        .debug_bounds("name:a.txt")
        .expect("the row is drawn")
        .center();
    let alt = Modifiers {
        alt: true,
        ..Modifiers::default()
    };
    press_release(cx, row, alt, 1);
    press_release(cx, row, alt, 2);
    let count = properties_windows(cx).len();
    set_single_click(false);
    assert_eq!(count, 1, "one Properties window");
    assert!(shell.opened.lock().is_empty(), "not opened");
}

/// #493: a plain click in single-click mode still opens the item, and opens no Properties.
#[gpui::test]
fn plain_click_still_opens_in_single_click_mode(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "the folder", loaded(r"C:\p"));
    set_single_click(true);
    let row = cx
        .debug_bounds("name:a.txt")
        .expect("the row is drawn")
        .center();
    press_release(cx, row, Modifiers::default(), 1);
    let windows = properties_windows(cx).len();
    set_single_click(false);
    assert_eq!(windows, 0, "no Properties window");
    wait_shell_in(cx, "a.txt opened", || !shell.opened.lock().is_empty());
}

/// The folder background menu has Arke's own Properties, for the folder shown (as Alt+Enter with
/// nothing selected); Windows' one under "Show more options" opened Explorer's dialog.
#[gpui::test]
fn the_background_menu_opens_the_folders_window(cx: &mut TestAppContext) {
    let fs = file_fs();
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\p");
    wait_until(cx, &view, "the folder", loaded(r"C:\p"));
    right_click(&view, cx, None);
    let labels = menu_labels(&view, cx).unwrap_or_default();
    assert!(
        labels.contains(&t!("menu.properties").into_owned()),
        "{labels:?}"
    );
    run_command(&view, cx, MenuCommand::Properties);
    cx.run_until_parked();
    let windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Properties>())
        .collect();
    assert_eq!(windows.len(), 1);
    let shown = windows[0].read_with(cx, |p, _| p.path_for_test()).unwrap();
    assert_eq!(shown, Some(PathBuf::from(r"C:\p")));
}
