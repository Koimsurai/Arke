//! GPUI tests for "Similar results" in search results (#396, `explorer_similar.rs`): names that
//! only have the letters in order wait behind one closed row, or show under a note when nothing
//! matches as typed.

use super::*;
use crate::explorer::scope::Scope;
use crate::explorer::similar::SimilarRow;
use crate::global_search::{GlobalSearch, Roots};

/// Opens "This PC" search on a fake C: with `files` in `C:\tools`, and types `query`.
fn whole_pc<'a>(
    cx: &'a mut TestAppContext,
    files: Vec<Entry>,
    query: &str,
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\t", vec![]);
    fs.insert_dir(r"C:\tools", files);
    let shell = Arc::new(FakeShell::default());
    let (ws, cx) = setup_workspace_with(cx, Arc::clone(&fs), Arc::clone(&shell), r"C:\t");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\t"));
    cx.update(|_, cx| {
        GlobalSearch::init(
            fs as Arc<dyn arke_core::Fs>,
            shell as Arc<dyn Shell>,
            Roots::Fixed(vec![PathBuf::from(r"C:\")]),
            None,
            cx,
        );
        GlobalSearch::enable(cx);
    });
    cx.dispatch_action(FocusFilter);
    cx.simulate_input(query);
    cx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.set_scope(Scope::Computer, cx);
            v.keep_filter_focus(window, cx);
            v.run_global(cx);
        });
    });
    (view, cx)
}

/// The count at the start of the status bar.
fn shown_count(view: &Entity<Explorer>, cx: &mut VisualTestContext) -> String {
    view.update(cx, |v, cx| v.info_parts(cx))
        .first()
        .cloned()
        .unwrap_or_default()
}

/// The reporter's case: one file with that name, plenty sharing a few letters. Only the real
/// hit is listed and counted, with a closed row under it; opening it lists the similar names
/// after the exact one (in any sort order), closing it takes them away again.
#[gpui::test]
fn whole_pc_lists_names_as_typed_and_similar_ones_on_request(cx: &mut TestAppContext) {
    let exact = r"C:\tools\validrive.exe";
    let (view, cx) = whole_pc(
        cx,
        vec![
            file("validrive.exe", 1),
            file("vacation_list_drive.txt", 1),
            file("valid_drive_notes.md", 1),
            file("other.txt", 1),
        ],
        "validrive",
    );
    wait_until(cx, &view, "the exact hit", |v| final_hits(v) == [exact]);
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), Some((1, SimilarRow::Closed)));
        assert_eq!(v.search_row_count(), 2, "the hit and the closed row");
    });
    assert_eq!(shown_count(&view, cx), tn!("info.matched", 1));

    // Open: the similar names come after the exact one, and count.
    view.update(cx, Explorer::toggle_similar);
    wait_until(cx, &view, "similar results", |v| final_hits(v).len() == 3);
    let hits = view.update(cx, |v, _| final_hits(v));
    assert_eq!(hits[0], exact);
    let mut similar = hits[1..].to_vec();
    similar.sort();
    assert_eq!(
        similar,
        [
            r"C:\tools\vacation_list_drive.txt",
            r"C:\tools\valid_drive_notes.md"
        ]
    );
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), Some((1, SimilarRow::Open)));
        assert_eq!(v.search_results.as_ref().map(|r| r.similar), Some(2));
        assert_eq!(v.search_row_count(), 4);
    });
    assert_eq!(shown_count(&view, cx), tn!("info.matched", 3));

    // Sorted by name either way, the exact hit stays above the similar ones.
    for _ in 0..2 {
        view.update(cx, |v, cx| {
            v.click_search_header(arke_core::SearchColumn::Name, cx);
        });
        let hits = view.update(cx, |v, _| final_hits(v));
        assert_eq!(hits[0], exact, "{hits:?}");
        assert_eq!(hits.len(), 3);
    }
    view.update(cx, |v, cx| {
        v.click_search_header(arke_core::SearchColumn::Name, cx);
    });

    // The cursor on a similar hit moves up to the last exact one when they close.
    view.update(cx, |v, cx| {
        v.select_index(2, cx);
        v.toggle_similar(cx);
    });
    view.update(cx, |v, _| {
        assert_eq!(final_hits(v), [exact]);
        assert_eq!(v.similar_row(), Some((1, SimilarRow::Closed)));
        assert_eq!(v.search_cursor, Some(0));
    });
    assert_eq!(shown_count(&view, cx), tn!("info.matched", 1));
    // Closed stays closed when the results come in again (the index changed).
    view.update(cx, Explorer::run_global);
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(50));
    cx.run_until_parked();
    view.update(cx, |v, _| assert_eq!(final_hits(v), [exact]));
}

/// Nothing matches as typed: the similar names show right away, under a note instead of the
/// row. A query starting with `'` asks for exact matches only: no row, and no similar names.
#[gpui::test]
fn whole_pc_shows_similar_names_when_none_match_as_typed(cx: &mut TestAppContext) {
    let (view, cx) = whole_pc(
        cx,
        vec![file("vacation_list_drive.txt", 1), file("other.txt", 1)],
        "validrive",
    );
    wait_until(cx, &view, "similar results", |v| {
        final_hits(v) == [r"C:\tools\vacation_list_drive.txt"]
    });
    view.update(cx, |v, cx| {
        assert_eq!(v.similar_row(), Some((0, SimilarRow::Only)));
        assert_eq!(v.search_row_count(), 2, "the note and the hit");
        // The note can't be closed.
        v.toggle_similar(cx);
        assert_eq!(v.similar_row(), Some((0, SimilarRow::Only)));
    });
    assert_eq!(shown_count(&view, cx), tn!("info.matched", 1));

    // Exact only.
    cx.simulate_keystrokes("home");
    cx.simulate_input("'");
    view.update(cx, |v, _| assert_eq!(v.query, "'validrive"));
    wait_until(cx, &view, "no similar names", |v| {
        v.search_results
            .as_ref()
            .is_some_and(|r| r.hits.is_empty() && !r.searching)
    });
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), None);
        assert_eq!(v.search_row_count(), 0);
    });
}

/// Subfolders work the same way: the walk matches names as typed, similar names once the row is
/// opened (the folders aren't walked again), and right away under the note when nothing matches
/// as typed.
#[gpui::test]
fn subfolder_search_lists_similar_names_after_exact_ones(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\s", vec![dir("sub"), file("notes.txt", 1)]);
    fs.insert_dir(
        r"C:\s\sub",
        vec![file("n_o_t_e_s.md", 1), file("vacation_list_drive.txt", 1)],
    );
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::default(), r"C:\s");
    wait_until(cx, &view, "load", loaded(r"C:\s"));
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("notes");
    wait_until(cx, &view, "the exact hit", |v| {
        final_hits(v) == ["notes.txt"]
    });
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), Some((1, SimilarRow::Closed)));
    });
    let listed = fs.read_dir_calls();
    view.update(cx, Explorer::toggle_similar);
    wait_until(cx, &view, "similar results", |v| {
        final_hits(v) == ["notes.txt", r"sub\n_o_t_e_s.md"]
    });
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), Some((1, SimilarRow::Open)));
        assert_eq!(v.search_results.as_ref().map(|r| r.similar), Some(1));
    });
    assert_eq!(fs.read_dir_calls(), listed, "opening didn't walk again");

    // Typing on closes them: the new text matches as typed again.
    cx.simulate_input(".");
    wait_until(cx, &view, "exact again", |v| final_hits(v) == ["notes.txt"]);
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), Some((1, SimilarRow::Closed)));
    });

    // Nothing as typed: similar names right away, under the note.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("validrive");
    view.update(cx, |v, _| assert_eq!(v.query, "validrive"));
    wait_until(cx, &view, "similar results", |v| {
        final_hits(v) == [r"sub\vacation_list_drive.txt"]
    });
    view.update(cx, |v, _| {
        assert_eq!(v.similar_row(), Some((0, SimilarRow::Only)));
    });
}

/// A key pressed and let go (some actions wait for the release, see `tests_a11y.rs`).
fn press(cx: &mut VisualTestContext, key: &'static str) {
    cx.simulate_keystrokes(key);
    let keystroke = gpui::Keystroke::parse(key).unwrap();
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }),
            cx,
        );
    });
    cx.run_until_parked();
}

/// "Similar results" is a stop for the cursor keys like a row: Down from the last exact hit
/// lands on it (and stays there while it is closed), Enter and Space open and close it with the
/// cursor kept on it, so Down goes on to the first similar hit and Up comes back. On it, no hit
/// is under the cursor: Enter opens nothing and the count stays that of the hits.
#[gpui::test]
fn the_similar_results_row_is_a_keyboard_stop(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\k",
        vec![dir("sub"), file("notes.txt", 1), file("notes-old.txt", 1)],
    );
    fs.insert_dir(r"C:\k\sub", vec![file("n_o_t_e_s.md", 1)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\k");
    wait_until(cx, &view, "load", loaded(r"C:\k"));
    cx.dispatch_action(FocusSearch);
    cx.simulate_input("notes");
    wait_until(cx, &view, "the exact hits", |v| {
        final_hits(v).len() == 2 && v.similar_row() == Some((2, SimilarRow::Closed))
    });
    let at = |view: &Entity<Explorer>, cx: &mut VisualTestContext| {
        view.update(cx, |v, _| (v.search_cursor, v.similar_focused()))
    };

    // From the box into the list, onto the first hit; then the second, then the row.
    press(cx, "down");
    assert_eq!(at(&view, cx), (Some(0), false));
    press(cx, "down");
    assert_eq!(at(&view, cx), (Some(1), false));
    press(cx, "down");
    assert_eq!(at(&view, cx), (None, true), "on the row");
    press(cx, "down");
    assert_eq!(at(&view, cx), (None, true), "closed, it is the last stop");
    press(cx, "end");
    assert_eq!(at(&view, cx), (None, true));
    assert_eq!(shown_count(&view, cx), tn!("info.matched", 2));

    // Enter opens it; the cursor stays, Down goes to the first similar hit, Up comes back.
    press(cx, "enter");
    wait_until(cx, &view, "similar results", |v| {
        final_hits(v).len() == 3 && v.similar_row() == Some((2, SimilarRow::Open))
    });
    assert_eq!(at(&view, cx), (None, true), "the cursor stays on the row");
    assert!(
        shell.opened.lock().is_empty(),
        "Enter on the row opens nothing"
    );
    press(cx, "down");
    assert_eq!(at(&view, cx), (Some(2), false), "the first similar hit");
    assert_eq!(
        view.update(cx, |v, _| final_hits(v)[2].clone()),
        r"sub\n_o_t_e_s.md"
    );
    press(cx, "up");
    assert_eq!(at(&view, cx), (None, true));
    press(cx, "up");
    assert_eq!(at(&view, cx), (Some(1), false), "the last exact hit");

    // Space closes it, from the row.
    press(cx, "down");
    press(cx, "space");
    wait_until(cx, &view, "closed again", |v| {
        final_hits(v).len() == 2 && v.similar_row() == Some((2, SimilarRow::Closed))
    });
    assert_eq!(at(&view, cx), (None, true));
    assert_eq!(shown_count(&view, cx), tn!("info.matched", 2));
    // Up leaves it for the hits; Home goes to the first.
    press(cx, "up");
    assert_eq!(at(&view, cx), (Some(1), false));
    press(cx, "home");
    assert_eq!(at(&view, cx), (Some(0), false));
}
