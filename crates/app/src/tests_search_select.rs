//! GPUI tests for selecting in search results (#413, `explorer_search_select.rs`): Ctrl+A,
//! Ctrl and Shift clicks and keys, the marquee, and the actions on a selection from several
//! folders (delete, copy, the menu, the shelf, drag); and for opening a hit's folder from its
//! Location (#398).

use gpui::{Modifiers, MouseButton, point, px};

use super::*;
use crate::shelf::ShelfStore;

/// Opens `C:\s` on `fs`, searches its subfolders for `query` and waits for `count` hits, then
/// sorts them by name so they are in a known order.
fn search<'a>(
    cx: &'a mut TestAppContext,
    fs: Arc<FakeFs>,
    shell: Arc<FakeShell>,
    query: &str,
    count: usize,
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let (view, cx) = setup(cx, fs, shell, r"C:\s");
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.dispatch_action(FocusSearch);
    cx.simulate_input(query);
    wait_until(cx, &view, "search results", |v| {
        final_hits(v).len() == count
    });
    view.update(cx, |v, cx| {
        v.click_search_header(arke_core::SearchColumn::Name, cx);
    });
    cx.run_until_parked();
    (view, cx)
}

/// Four hits with different names, in three folders: by name `a\alpha_x.txt`, `b\beta_x.txt`,
/// `b\deep\delta_x.txt`, `gamma_x.txt`.
fn four_hits() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\s",
        vec![
            dir("a"),
            dir("b"),
            file("gamma_x.txt", 1),
            file("other.txt", 1),
        ],
    );
    fs.insert_dir(r"C:\s\a", vec![file("alpha_x.txt", 1)]);
    fs.insert_dir(r"C:\s\b", vec![dir("deep"), file("beta_x.txt", 1)]);
    fs.insert_dir(r"C:\s\b\deep", vec![file("delta_x.txt", 1)]);
    fs
}

const ALPHA: &str = r"a\alpha_x.txt";
const BETA: &str = r"b\beta_x.txt";
const DELTA: &str = r"b\deep\delta_x.txt";
const GAMMA: &str = "gamma_x.txt";

/// The same name in two folders, and one more hit: `docs\report.txt`, `notes\report.txt`,
/// `report_top.txt`.
fn same_names() -> Arc<FakeFs> {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\s",
        vec![dir("docs"), dir("notes"), file("report_top.txt", 1)],
    );
    fs.insert_dir(r"C:\s\docs", vec![file("report.txt", 1)]);
    fs.insert_dir(r"C:\s\notes", vec![file("report.txt", 1)]);
    fs
}

/// The two `report.txt` from [`same_names`].
fn both_reports() -> Vec<PathBuf> {
    vec![
        PathBuf::from(r"C:\s\docs\report.txt"),
        PathBuf::from(r"C:\s\notes\report.txt"),
    ]
}

/// The selected hits (`rel`) in list order.
fn selected(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> Vec<String> {
    view.update(cx, |v, _| {
        v.selected_hits()
            .iter()
            .map(|(_, hit)| hit.rel.to_string())
            .collect()
    })
}

/// `paths` in a known order.
fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

/// Where hit `rel` is in the list.
fn hit_index(view: &Entity<Explorer>, cx: &mut VisualTestContext, rel: &str) -> usize {
    view.update(cx, |v, _| final_hits(v))
        .iter()
        .position(|h| h == rel)
        .expect("listed")
}

/// A click on what `selector` names (drawn in the last frame), with `m` held.
fn click(cx: &mut VisualTestContext, selector: &'static str, m: Modifiers) {
    let at = name_at(cx, selector);
    cx.simulate_click(at, m);
}

/// Selects both `report.txt` of [`same_names`] by their Location: a click, then Ctrl+click.
fn select_both_reports(view: &Entity<Explorer>, cx: &mut VisualTestContext) {
    click(cx, r"location:docs\report.txt", Modifiers::none());
    click(cx, r"location:notes\report.txt", Modifiers::control());
    assert_eq!(
        selected(view, cx),
        [r"docs\report.txt", r"notes\report.txt"]
    );
}

/// Until something is picked the first hit counts as selected, as it always did; Ctrl+A in the
/// list then selects every hit, the same name in two folders included.
#[gpui::test]
fn ctrl_a_selects_every_result(cx: &mut TestAppContext) {
    let (view, cx) = search(cx, same_names(), Arc::default(), "report", 3);
    let hits = view.update(cx, |v, _| final_hits(v));
    // The sort kept the cursor on the hit that came first.
    let cursor = view.update(cx, |v, _| v.search_cursor).expect("a hit");
    assert_eq!(selected(&view, cx), [hits[cursor].clone()]);

    cx.simulate_keystrokes("down ctrl-a");
    assert_eq!(selected(&view, cx), hits);
    assert_eq!(view.update(cx, |v, _| v.search_selected_count()), 3);

    // Ctrl+Space takes the one under the cursor out again.
    cx.simulate_keystrokes("ctrl-space");
    let mut rest = hits;
    rest.remove(cursor);
    assert_eq!(selected(&view, cx), rest);
}

/// Clicks with Ctrl and Shift, and Shift with the arrow keys, select as in a folder's list.
#[gpui::test]
fn ctrl_and_shift_clicks_select_results(cx: &mut TestAppContext) {
    let (view, cx) = search(cx, four_hits(), Arc::default(), "_x", 4);
    assert_eq!(
        view.update(cx, |v, _| final_hits(v)),
        [ALPHA, BETA, DELTA, GAMMA]
    );
    let ctrl_shift = Modifiers {
        control: true,
        shift: true,
        ..Modifiers::default()
    };

    click(cx, "name:beta_x.txt", Modifiers::none());
    assert_eq!(selected(&view, cx), [BETA]);
    click(cx, "name:gamma_x.txt", Modifiers::control());
    assert_eq!(selected(&view, cx), [BETA, GAMMA]);
    // Ctrl+click on a selected hit takes it out, and starts the next range there.
    click(cx, "name:beta_x.txt", Modifiers::control());
    assert_eq!(selected(&view, cx), [GAMMA]);
    click(cx, "name:alpha_x.txt", Modifiers::shift());
    assert_eq!(selected(&view, cx), [ALPHA, BETA]);
    // Ctrl+Shift adds the range to what is selected.
    click(cx, "name:gamma_x.txt", ctrl_shift);
    assert_eq!(selected(&view, cx), [ALPHA, BETA, DELTA, GAMMA]);
    // A plain click inside the selection keeps it until the release, then selects only that hit.
    click(cx, "name:delta_x.txt", Modifiers::none());
    assert_eq!(selected(&view, cx), [DELTA]);

    // Shift+arrows extend from there.
    cx.simulate_keystrokes("shift-down");
    assert_eq!(selected(&view, cx), [DELTA, GAMMA]);
    cx.simulate_keystrokes("shift-up shift-up");
    assert_eq!(selected(&view, cx), [BETA, DELTA]);
    cx.simulate_keystrokes("down");
    assert_eq!(selected(&view, cx), [DELTA]);
}

/// A drag from the empty space under the results selects the hits it reaches into; with Ctrl
/// the selection stays and the box adds to it; a click there clears it.
#[gpui::test]
fn marquee_selects_results(cx: &mut TestAppContext) {
    let (view, cx) = search(cx, four_hits(), Arc::default(), "_x", 4);
    let (bounds, row) = view.update(cx, |v, _| (v.list_bounds_for_test(), v.row_height()));
    let last = cx.debug_bounds("name:gamma_x.txt").expect("drawn");
    // Below the hits and the "Similar results" row, if there is one.
    let start = point(bounds.left() + px(200.), last.bottom() + row * 2.5);
    assert!(start.y < bounds.bottom(), "need empty space below the hits");
    let drag_to = |cx: &mut VisualTestContext, name: &'static str, m: Modifiers| {
        let end = point(start.x + px(40.), name_at(cx, name).y);
        cx.simulate_mouse_down(start, MouseButton::Left, m);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), m);
        cx.simulate_mouse_up(end, MouseButton::Left, m);
    };

    drag_to(cx, "name:beta_x.txt", Modifiers::none());
    assert_eq!(selected(&view, cx), [BETA, DELTA, GAMMA]);

    click(cx, "name:alpha_x.txt", Modifiers::none());
    drag_to(cx, "name:delta_x.txt", Modifiers::control());
    assert_eq!(selected(&view, cx), [ALPHA, DELTA, GAMMA]);

    cx.simulate_click(start, Modifiers::none());
    assert_eq!(selected(&view, cx), Vec::<String>::new());
}

/// Copy and delete take every selected hit, from both folders; Delete recycles them with Undo
/// putting each back in its own folder, and the hit left after them is selected.
#[gpui::test]
fn copy_and_delete_a_selection_from_two_folders(cx: &mut TestAppContext) {
    use crate::shell::Undo;
    let fs = same_names();
    let shell = Arc::new(FakeShell::default());
    apply_to_fake_fs(&fs, &shell);
    let (view, cx) = search(cx, fs, Arc::clone(&shell), "report", 3);
    select_both_reports(&view, cx);

    cx.simulate_keystrokes("ctrl-c");
    wait_shell("put on clipboard", || shell.clipboard().is_some());
    let copied = shell.clipboard().expect("files");
    assert!(!copied.cut);
    assert_eq!(sorted(copied.paths), both_reports());

    cx.simulate_keystrokes("delete");
    wait_until(cx, &view, "both deleted", |v| {
        v.ops_running == 0 && final_hits(v) == ["report_top.txt"]
    });
    let deleted: Vec<PathBuf> = shell
        .ops()
        .into_iter()
        .filter_map(|op| match op {
            FileOp::Delete { paths, recycle } => {
                assert!(recycle, "to the Recycle Bin");
                Some(paths)
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(sorted(deleted), both_reports());
    assert_eq!(selected(&view, cx), ["report_top.txt"]);

    cx.simulate_keystrokes("ctrl-z");
    wait_until(cx, &view, "undo", |v| {
        v.ops_running == 0 && !shell.undone.lock().is_empty()
    });
    let undone = shell.undone.lock().clone();
    let [Undo::PutBack(items)] = undone.as_slice() else {
        panic!("one put-back: {undone:?}");
    };
    let back: Vec<PathBuf> = items.iter().map(|i| i.parent.join(&i.name)).collect();
    assert_eq!(sorted(back), both_reports());
}

/// Right-click inside a selection from two folders keeps it: the menu is for both, the Windows
/// menu is read for both, and a drag from either takes both.
#[gpui::test]
fn the_menu_and_drag_take_a_selection_from_two_folders(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = search(cx, same_names(), Arc::clone(&shell), "report", 3);
    select_both_reports(&view, cx);
    let notes = hit_index(&view, cx, r"notes\report.txt");
    let top = hit_index(&view, cx, "report_top.txt");

    right_click(&view, cx, Some(notes));
    assert_eq!(
        selected(&view, cx),
        [r"docs\report.txt", r"notes\report.txt"]
    );
    wait_until(cx, &view, "more options loaded", |v| {
        v.context_menu.as_ref().is_some_and(|m| m.shell_state().0)
    });
    assert!(
        shell
            .system_menus
            .lock()
            .iter()
            .any(|paths| sorted(paths.clone()) == both_reports()),
        "{:?}",
        shell.system_menus.lock()
    );
    run_command(&view, cx, MenuCommand::CopyPath);
    wait_shell("copy path", || shell.clipboard_text.lock().is_some());
    let text = shell.clipboard_text.lock().clone().unwrap_or_default();
    let mut lines: Vec<&str> = text.split("\r\n").collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [r#""C:\s\docs\report.txt""#, r#""C:\s\notes\report.txt""#]
    );

    let dragged = view.update(cx, |v, _| sorted(v.drag_paths(notes)));
    assert_eq!(dragged, both_reports());
    // A drag from a hit outside the selection takes only that hit.
    let other = view.update(cx, |v, _| v.drag_paths(top));
    assert_eq!(other, [PathBuf::from(r"C:\s\report_top.txt")]);

    // The command closed the menu (an Esc now would leave the search). Right-click outside the
    // selection selects only that hit.
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));
    right_click(&view, cx, Some(top));
    assert_eq!(selected(&view, cx), ["report_top.txt"]);
}

/// "Add to shelf" puts every selected hit on the shelf.
#[gpui::test]
fn the_shelf_takes_every_selected_result(cx: &mut TestAppContext) {
    let (view, cx) = search(cx, same_names(), Arc::default(), "report", 3);
    cx.simulate_keystrokes("down ctrl-a ctrl-shift-a");
    let shelved: Vec<PathBuf> = cx.update(|_, cx| {
        ShelfStore::global(cx)
            .read(cx)
            .shelf()
            .items()
            .iter()
            .map(|i| i.path.clone())
            .collect()
    });
    let mut all = both_reports();
    all.push(PathBuf::from(r"C:\s\report_top.txt"));
    assert_eq!(sorted(shelved), sorted(all));
    assert_eq!(
        view.update(cx, |v, _| v.notice.clone()),
        Some(tn!("shelf.added", 3).into_owned())
    );
}

/// A pressed and released double-click on what `selector` names.
fn double_click(cx: &mut VisualTestContext, selector: &'static str) {
    let at = name_at(cx, selector);
    cx.simulate_click(at, Modifiers::none());
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
    });
    cx.run_until_parked();
}

/// #398: a double-click on a hit's Location opens its folder with it selected; anywhere else on
/// the row it opens the file, as before. A single click on the Location only selects.
#[gpui::test]
fn double_click_on_location_opens_the_folder(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![dir("docs"), file("other.txt", 1)]);
    fs.insert_dir(r"C:\s\docs", vec![file("plan.txt", 1), file("zeta.txt", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = search(cx, fs, Arc::clone(&shell), "plan", 1);

    click(cx, r"location:docs\plan.txt", Modifiers::none());
    cx.run_until_parked();
    assert!(
        view.update(cx, |v, _| v.search_mode),
        "a click only selects"
    );
    assert_eq!(selected(&view, cx), [r"docs\plan.txt"]);
    past_double_click_time(cx);

    double_click(cx, "name:plan.txt");
    wait_shell("opened", || !shell.opened.lock().is_empty());
    assert_eq!(*shell.opened.lock(), [PathBuf::from(r"C:\s\docs\plan.txt")]);
    assert!(
        view.update(cx, |v, _| v.search_mode),
        "still in the results"
    );
    past_double_click_time(cx);

    double_click(cx, r"location:docs\plan.txt");
    wait_until(cx, &view, "the hit's folder", loaded(r"C:\s\docs"));
    view.update(cx, |v, _| {
        assert!(!v.search_mode);
        assert_eq!(selected_name(v).as_deref(), Some("plan.txt"));
    });
    assert_eq!(shell.opened.lock().len(), 1, "the file didn't open again");
}
