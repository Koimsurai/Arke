//! GPUI tests for a previewed file that is saved again: the preview and the details follow the
//! file, whichever way the editor saved it (#434).

use super::*;
use crate::explorer::{PreviewBody, Saved, saved_since};
use gpui::px;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000 + secs)
}

fn note(size: u64, secs: u64) -> Entry {
    Entry::new("note.txt", EntryKind::File, size, Some(at(secs)), 0)
}

/// The text the preview shows, once it has loaded.
fn shown_text(v: &Explorer) -> Option<String> {
    v.preview.as_ref()?.body.text()
}

fn size_row(v: &Explorer) -> Option<String> {
    let label = t!("col.size").to_string();
    v.preview_info_for_test()
        .into_iter()
        .find_map(|(k, value)| (k == label).then_some(value))
}

/// `note.txt` in `C:\r`, selected and previewed with `old` as its text.
fn previewed_note<'a>(
    cx: &'a mut TestAppContext,
    fs: &Arc<FakeFs>,
    old: &str,
) -> (Entity<Explorer>, &'a mut VisualTestContext) {
    let path = Path::new(r"C:\r");
    fs.insert_dir(path, vec![note(old.len() as u64, 0)]);
    fs.set_content(r"C:\r\note.txt", old.as_bytes().to_vec());
    let (view, cx) = setup(cx, Arc::clone(fs), Arc::new(FakeShell::default()), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", |v| {
        shown_text(v).is_some_and(|t| t.contains(old))
    });
    (view, cx)
}

fn wait_text(cx: &mut VisualTestContext, view: &Entity<Explorer>, want: &'static str) {
    wait_until(cx, view, "the new text", move |v| {
        shown_text(v).is_some_and(|t| t.contains(want))
    });
}

/// Written in place (Notepad): same name, new size and write time.
#[gpui::test]
fn written_in_place_updates_the_preview_and_the_size(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let (view, cx) = previewed_note(cx, &fs, "old");
    let before = view.update(cx, |v, _| size_row(v));
    fs.set_content(r"C:\r\note.txt", b"a longer new text".to_vec());
    fs.add_entry(Path::new(r"C:\r"), note(17, 5));
    fs.emit_changed(Path::new(r"C:\r"), &["note.txt"]);
    wait_text(cx, &view, "a longer new text");
    let after = view.update(cx, |v, _| size_row(v));
    assert_ne!(before, after);
    let modified = view.update(cx, |v, _| v.preview.as_ref().and_then(|p| p.entry.modified));
    assert_eq!(modified, Some(at(5)));
}

/// Replaced under the same name with the same size (a rename over it, ReplaceFile): only the write
/// time says it is another file.
#[gpui::test]
fn replaced_with_the_same_size_updates_the_preview(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let (view, cx) = previewed_note(cx, &fs, "aaa");
    fs.set_content(r"C:\r\note.txt", b"bbb".to_vec());
    fs.add_entry(Path::new(r"C:\r"), note(3, 9));
    fs.emit_changed(
        Path::new(r"C:\r"),
        &["note.txt", "~WRL0001.tmp", "~WRD0002.tmp"],
    );
    wait_text(cx, &view, "bbb");
}

/// A save that comes while the old text is still being read: the late old result is dropped.
#[gpui::test]
fn a_save_during_a_slow_load_does_not_keep_the_old_text(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let path = Path::new(r"C:\r");
    fs.insert_dir(path, vec![note(3, 0)]);
    fs.set_content(r"C:\r\note.txt", b"old".to_vec());
    fs.set_delay(r"C:\r\note.txt", Duration::from_millis(600));
    let (view, cx) = setup(cx, Arc::clone(&fs), Arc::new(FakeShell::default()), r"C:\r");
    wait_until(cx, &view, "load", loaded(r"C:\r"));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    // The first read is under way; the file is saved now.
    wait_until(cx, &view, "loading", |v| {
        matches!(
            v.preview.as_ref().map(|p| &p.body),
            Some(PreviewBody::Loading(_))
        )
    });
    fs.set_content(r"C:\r\note.txt", b"new".to_vec());
    fs.add_entry(path, note(3, 4));
    fs.emit_changed(path, &["note.txt"]);
    wait_text(cx, &view, "new");
    // Let the first read finish too: it must not put "old" back.
    std::thread::sleep(Duration::from_millis(900));
    cx.run_until_parked();
    assert_eq!(
        view.update(cx, |v, _| shown_text(v)).as_deref(),
        Some("new")
    );
}

/// A change that leaves the file as it was (an event with nothing new) reads nothing again.
#[gpui::test]
fn an_unchanged_entry_keeps_the_preview(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let (view, cx) = previewed_note(cx, &fs, "same");
    let gen_before = view.update(cx, |v, _| v.preview_gen);
    fs.emit_changed(Path::new(r"C:\r"), &["note.txt"]);
    std::thread::sleep(Duration::from_millis(400));
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| v.preview_gen), gen_before);
    assert_eq!(
        view.update(cx, |v, _| shown_text(v)).as_deref(),
        Some("same")
    );
}

#[test]
fn saved_since_tells_content_from_details() {
    let base = note(10, 1);
    assert_eq!(saved_since(&base, &note(10, 1)), Saved::Same);
    assert_eq!(saved_since(&base, &note(11, 1)), Saved::Content);
    assert_eq!(saved_since(&base, &note(10, 2)), Saved::Content);
    let mut hidden = note(10, 1);
    hidden.attributes = 2;
    assert_eq!(saved_since(&base, &hidden), Saved::Details);
    let mut no_time = note(10, 1);
    no_time.modified = None;
    assert_eq!(saved_since(&base, &no_time), Saved::Content);
    // A creation time that came late is no change of content.
    assert_eq!(
        saved_since(&base, &note(10, 1).with_created(Some(at(0)))),
        Saved::Same
    );
}

/// Deleted first and made again a moment later, in another batch (Word's save on a slow disk):
/// the item is selected again and its preview shows the new text.
#[gpui::test]
fn deleted_then_made_again_is_selected_and_previewed_again(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    let (view, cx) = previewed_note(cx, &fs, "old");
    let dir = Path::new(r"C:\r");
    fs.remove(dir, "note.txt");
    fs.emit_changed(dir, &["note.txt"]);
    wait_until(cx, &view, "the file gone", |v| names(v).is_empty());
    fs.set_content(r"C:\r\note.txt", b"made again".to_vec());
    fs.add_entry(dir, note(10, 7));
    fs.emit_changed(dir, &["note.txt"]);
    wait_text(cx, &view, "made again");
    assert_eq!(view.update(cx, |v, _| v.selection.len()), 1);
}
