//! GPUI tests for remembering the view per folder (`folder_views.rs`, `explorer_folder_view.rs`).

use gpui::AppContext;

use super::*;
use crate::explorer::ViewMode;
use crate::folder_views::{FolderView, FolderViewStore, MAX_FOLDERS};
use crate::settings::SettingsStore;

fn view_mode(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> ViewMode {
    view.update(cx, |v, _| v.view_mode)
}

fn sort_of(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> (SortColumn, bool) {
    view.update(cx, |v, _| (v.sort.column, v.sort.ascending))
}

fn remembered(cx: &mut VisualTestContext, path: &str) -> Option<FolderView> {
    cx.update(|_, cx| FolderViewStore::global(cx).read(cx).peek(Path::new(path)))
}

fn remembered_count(cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| FolderViewStore::global(cx).read(cx).len())
}

fn change_settings(cx: &mut VisualTestContext, f: impl FnOnce(&mut crate::settings::Settings)) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

fn go(view: &Entity<Explorer>, cx: &mut VisualTestContext, path: &str) {
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(path), NavKind::New, cx);
    });
    wait_until(cx, view, path, loaded(path));
}

fn by_size_descending() -> SortSpec {
    SortSpec::by(SortColumn::Size, false, false)
}

fn two_folders() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![file("small.txt", 1), file("big.txt", 9)]);
    fs.insert_dir(r"C:\b", vec![file("one.txt", 1)]);
    fs
}

/// Change the view in A, go to B (defaults), come back to A (remembered), forward to B again
/// (still defaults: leaving A recorded nothing for B).
#[gpui::test]
fn view_and_sort_are_remembered_per_folder(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, two_folders(), Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    assert_eq!(view_mode(&view, cx), ViewMode::Details);

    cx.simulate_keystrokes("ctrl-shift-2");
    view.update(cx, |v, cx| v.set_sort(by_size_descending(), cx));
    assert_eq!(
        remembered(cx, r"C:\a"),
        Some(FolderView {
            view: ViewMode::Icons,
            icons: crate::explorer::IconSize::LARGE,
            column: SortColumn::Size,
            ascending: false,
            then: [None; 2],
        })
    );

    go(&view, cx, r"C:\b");
    assert_eq!(view_mode(&view, cx), ViewMode::Details, "B has no view yet");
    assert_eq!(sort_of(&view, cx), (SortColumn::Name, true));
    assert_eq!(remembered(cx, r"C:\b"), None, "leaving A records nothing");

    cx.simulate_keystrokes("alt-left");
    wait_until(cx, &view, "back to a", loaded(r"C:\a"));
    assert_eq!(view_mode(&view, cx), ViewMode::Icons);
    assert_eq!(sort_of(&view, cx), (SortColumn::Size, false));
    wait_until(cx, &view, "listed by size", |v| {
        names(v) == ["big.txt", "small.txt"]
    });

    cx.simulate_keystrokes("alt-right");
    wait_until(cx, &view, "forward to b", loaded(r"C:\b"));
    assert_eq!(view_mode(&view, cx), ViewMode::Details);
    assert_eq!(remembered_count(cx), 1);
}

/// A folder opened in a new tab comes up the way it was left in another tab.
#[gpui::test]
fn remembered_view_follows_the_folder_into_new_tabs(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![dir("one"), dir("two")]);
    fs.insert_many(r"C:\t\one", 1);
    fs.insert_many(r"C:\t\two", 2);
    let (ws, cx) = setup_workspace(cx, fs, r"C:\t");
    let first = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &first, "load", loaded(r"C:\t"));
    cx.simulate_keystrokes("ctrl-shift-2");
    assert_eq!(view_mode(&first, cx), ViewMode::Icons);

    // Ctrl+T, Enter: a new tab in the current folder
    cx.simulate_keystrokes("ctrl-t");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let second = ws.update(cx, |ws, _| ws.active_view().clone());
    assert_ne!(second.entity_id(), first.entity_id());
    wait_until(cx, &second, "new tab", loaded(r"C:\t"));
    assert_eq!(view_mode(&second, cx), ViewMode::Icons);

    // Another folder in a new tab: the defaults
    first.update(cx, |_, cx| {
        cx.emit(ExplorerEvent::OpenInNewTab {
            path: PathBuf::from(r"C:\t\two"),
            activate: true,
        });
    });
    cx.run_until_parked();
    let third = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &third, "two in a new tab", loaded(r"C:\t\two"));
    assert_eq!(view_mode(&third, cx), ViewMode::Details);
}

/// Setting off: each tab keeps its own view and sort (carried from folder to folder, following
/// the defaults until changed by hand), nothing is recorded and remembered views aren't applied.
#[gpui::test]
fn setting_off_keeps_views_per_tab(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, two_folders(), Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    // Remembered while on
    cx.simulate_keystrokes("ctrl-shift-2");
    assert!(remembered(cx, r"C:\a").is_some());

    change_settings(cx, |s| s.remember_folder_views = false);
    go(&view, cx, r"C:\b");
    assert_eq!(
        view_mode(&view, cx),
        ViewMode::Icons,
        "the tab's view travels"
    );
    cx.simulate_keystrokes("ctrl-shift-6");
    view.update(cx, |v, cx| v.set_sort(by_size_descending(), cx));
    go(&view, cx, r"C:\a");
    assert_eq!(
        view_mode(&view, cx),
        ViewMode::Details,
        "A's view isn't applied"
    );
    assert_eq!(sort_of(&view, cx), (SortColumn::Size, false));
    assert_eq!(remembered(cx, r"C:\b"), None, "nothing recorded");
    assert_eq!(remembered_count(cx), 1);

    // Defaults: a tab changed by hand keeps its sort; the view it never changed from the
    // default (Details) follows.
    change_settings(cx, |s| {
        s.default_view = ViewMode::Columns;
        s.sort_by = crate::settings::SortBy::Modified;
    });
    assert_eq!(view_mode(&view, cx), ViewMode::Columns);
    assert_eq!(sort_of(&view, cx), (SortColumn::Size, false));
}

/// New defaults reach tabs showing a folder without a remembered view, not remembered ones.
#[gpui::test]
fn defaults_reach_folders_without_a_remembered_view(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\a", vec![file("x.txt", 1)]);
    fs.insert_dir(r"C:\deep\b", vec![file("y.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    cx.simulate_keystrokes("ctrl-shift-2");
    go(&view, cx, r"C:\deep\b");

    change_settings(cx, |s| {
        s.default_view = ViewMode::Columns;
        s.sort_descending = true;
    });
    assert_eq!(
        view_mode(&view, cx),
        ViewMode::Columns,
        "B follows the defaults"
    );
    assert_eq!(sort_of(&view, cx), (SortColumn::Name, false));

    // Not a columns move (C:\a is neither parent, child nor sibling of C:\deep\b): A's own view
    go(&view, cx, r"C:\a");
    assert_eq!(view_mode(&view, cx), ViewMode::Icons);
    assert_eq!(sort_of(&view, cx), (SortColumn::Name, true));
    change_settings(cx, |s| {
        s.default_view = ViewMode::Details;
        s.sort_by = crate::settings::SortBy::Size;
    });
    assert_eq!(view_mode(&view, cx), ViewMode::Icons, "A keeps its view");
    assert_eq!(sort_of(&view, cx), (SortColumn::Name, true), "and its sort");
}

/// "Reset folder views": everything is forgotten and a tab showing a remembered view goes back to
/// the defaults right away.
#[gpui::test]
fn reset_forgets_every_folder(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, two_folders(), Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    cx.simulate_keystrokes("ctrl-shift-2");
    view.update(cx, |v, cx| v.set_sort(by_size_descending(), cx));
    assert!(view.update(cx, |v, _| v.folder_view_applied()));

    cx.update(|_, cx| FolderViewStore::global(cx).update(cx, FolderViewStore::clear));
    cx.run_until_parked();
    assert_eq!(remembered_count(cx), 0);
    assert_eq!(view_mode(&view, cx), ViewMode::Details);
    assert_eq!(sort_of(&view, cx), (SortColumn::Name, true));

    go(&view, cx, r"C:\b");
    go(&view, cx, r"C:\a");
    assert_eq!(view_mode(&view, cx), ViewMode::Details);
}

/// Past the cap the least recently used folder is forgotten and opens with the defaults again.
#[gpui::test]
fn oldest_folder_is_forgotten_past_the_cap(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, two_folders(), Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    cx.simulate_keystrokes("ctrl-shift-2");
    go(&view, cx, r"C:\b");
    let icons = FolderView {
        view: ViewMode::Icons,
        icons: crate::explorer::IconSize::LARGE,
        column: SortColumn::Name,
        ascending: true,
        then: [None; 2],
    };
    cx.update(|_, cx| {
        FolderViewStore::global(cx).update(cx, |s, cx| {
            for n in 0..MAX_FOLDERS {
                s.record(Path::new(&format!(r"D:\other{n}")), icons, cx);
            }
        });
    });
    assert_eq!(remembered_count(cx), MAX_FOLDERS);
    assert_eq!(remembered(cx, r"C:\a"), None);
    go(&view, cx, r"C:\a");
    assert_eq!(view_mode(&view, cx), ViewMode::Details);
}

/// The same folder written with another case or a trailing separator is one entry.
#[gpui::test]
fn folders_match_regardless_of_case_and_trailing_separators(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\Data", vec![file("x.txt", 1)]);
    fs.insert_dir(r"C:\Other", vec![file("y.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\Data");
    wait_until(cx, &view, "load", loaded(r"C:\Data"));
    cx.simulate_keystrokes("ctrl-shift-2");
    assert!(remembered(cx, r"c:\DATA\").is_some());

    // Recorded under an odd spelling, applied to the real one
    let columns = FolderView {
        view: ViewMode::Columns,
        icons: crate::explorer::IconSize::LARGE,
        column: SortColumn::Type,
        ascending: true,
        then: [None; 2],
    };
    cx.update(|_, cx| {
        FolderViewStore::global(cx)
            .update(cx, |s, cx| s.record(Path::new("c:/other//"), columns, cx));
    });
    go(&view, cx, r"C:\Other");
    assert_eq!(view_mode(&view, cx), ViewMode::Columns);
    assert_eq!(sort_of(&view, cx), (SortColumn::Type, true));
    assert_eq!(remembered_count(cx), 2);
}

/// Columns view browses: going into a subfolder and back up keeps the columns, and doesn't save
/// anything for the subfolder. Elsewhere, folders open their own way again.
#[gpui::test]
fn columns_view_keeps_columns_while_browsing(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("one"), dir("two")]);
    fs.insert_dir(r"C:\m\one", vec![dir("inner"), file("x.txt", 1)]);
    fs.insert_many(r"C:\m\one\inner", 1);
    fs.insert_many(r"C:\m\two", 1);
    fs.insert_many(r"C:\elsewhere", 1);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m\one");
    wait_until(cx, &view, "load", loaded(r"C:\m\one"));
    cx.dispatch_action(crate::explorer::ViewColumns);
    cx.simulate_keystrokes("down right");
    wait_until(cx, &view, "into inner", loaded(r"C:\m\one\inner"));
    assert_eq!(view_mode(&view, cx), ViewMode::Columns);
    cx.simulate_keystrokes("left");
    wait_until(cx, &view, "back up", loaded(r"C:\m\one"));
    // Sideways, as clicking a folder in the parent column does
    go(&view, cx, r"C:\m\two");
    assert_eq!(view_mode(&view, cx), ViewMode::Columns);
    assert_eq!(remembered(cx, r"C:\m\one\inner"), None);
    assert_eq!(remembered(cx, r"C:\m\two"), None);
    assert_eq!(remembered_count(cx), 1);

    go(&view, cx, r"C:\elsewhere");
    assert_eq!(view_mode(&view, cx), ViewMode::Details);
}

/// Search results aren't a folder: changing the view there records nothing.
#[gpui::test]
fn search_results_record_nothing(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\proj", vec![dir("docs"), file("notes.txt", 1)]);
    fs.insert_dir(r"C:\proj\docs", vec![file("report.md", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\proj");
    wait_until(cx, &view, "load", loaded(r"C:\proj"));
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("report");
    wait_until(cx, &view, "search results", |v| final_hits(v).len() == 1);
    assert!(view.update(cx, |v, _| v.search_mode));
    view.update(cx, |v, cx| v.set_sort(by_size_descending(), cx));
    assert_eq!(remembered_count(cx), 0);
}

/// A view picked while the folder is still opening belongs to that folder; a folder that fails to
/// open leaves the tab showing the previous folder its own way.
#[gpui::test]
fn picks_while_opening_and_failed_opens(cx: &mut TestAppContext) {
    let fs = two_folders();
    fs.hang(r"C:\b");
    let (view, cx) = setup(cx, fs.clone(), Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    view.update(cx, |v, cx| v.set_sort(by_size_descending(), cx));

    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\b"), NavKind::New, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        sort_of(&view, cx),
        (SortColumn::Name, true),
        "B's defaults show at once"
    );
    cx.simulate_keystrokes("ctrl-shift-2");
    fs.release_all();
    wait_until(cx, &view, "b opened", loaded(r"C:\b"));
    assert_eq!(
        remembered(cx, r"C:\b").map(|v| v.view),
        Some(ViewMode::Icons)
    );
    assert_eq!(
        remembered(cx, r"C:\a").map(|v| v.view),
        Some(ViewMode::Details),
        "A keeps what was picked there"
    );

    // Back in A (Details, by size), a folder that doesn't exist fails: A stays as it was
    go(&view, cx, r"C:\a");
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\missing"), NavKind::New, cx);
    });
    wait_until(cx, &view, "error shown", |v| v.error.is_some());
    wait_until(cx, &view, "a again", loaded(r"C:\a"));
    assert_eq!(view_mode(&view, cx), ViewMode::Details);
    assert_eq!(sort_of(&view, cx), (SortColumn::Size, false));
}

/// Saved to `folder_views.json` in the background and read back at the next start.
#[gpui::test]
fn remembered_views_survive_a_restart(cx: &mut TestAppContext) {
    let dir =
        std::env::temp_dir().join(format!("arke-folder-views-restart-{}", std::process::id()));
    let file = dir.join("folder_views.json");
    let _ = std::fs::remove_dir_all(&dir);
    cx.update(|cx| {
        FolderViewStore::init(Some(file.clone()), cx);
    });
    let (view, cx) = setup(cx, two_folders(), Arc::default(), r"C:\a");
    wait_until(cx, &view, "load a", loaded(r"C:\a"));
    cx.simulate_keystrokes("ctrl-shift-2");
    // Past the save delay; the writer thread then writes the file
    cx.executor().advance_clock(Duration::from_secs(1));
    let deadline = Instant::now() + Duration::from_secs(20);
    while crate::folder_views::load(&file).map_or(true, |v| v.is_empty()) {
        assert!(Instant::now() < deadline, "folder views were not saved");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }

    // "Restart": a fresh store reads the file, a new window opens A the remembered way
    cx.update(|_, cx| {
        FolderViewStore::init(Some(file.clone()), cx);
    });
    let services = Services {
        fs: two_folders(),
        shell: Arc::new(FakeShell::default()),
        remotes: crate::remote_shell::test_remotes(),
    };
    let again = cx.update(|window, cx| {
        cx.new(|cx| {
            Explorer::new(
                services,
                PathBuf::from(r"C:\a"),
                None,
                Instant::now(),
                window,
                cx,
            )
        })
    });
    wait_until(cx, &again, "load a after restart", loaded(r"C:\a"));
    assert_eq!(view_mode(&again, cx), ViewMode::Icons);
    let _ = std::fs::remove_dir_all(&dir);
}
