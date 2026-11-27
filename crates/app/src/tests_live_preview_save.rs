//! Live: a previewed file saved the way editors do it, on the real file system with the real
//! watcher (#434). Only this test's own `arke-test-presave-*` temp folder is touched.
//!
//! Run with `cargo test -p arke-app --bin arke live_preview_save -- --ignored --nocapture --test-threads 1`.

use std::fs;

use arke_core::fs::RealFs;
use gpui::px;

use super::*;

/// A folder under %TEMP%; removed when dropped.
struct Folder(
    PathBuf,
    #[allow(dead_code, reason = "held for its Drop, which deletes the folder")] tempfile::TempDir,
);

impl Folder {
    fn new(tag: &str) -> Self {
        let guard = crate::win::long_tempdir(&format!("arke-test-presave-{tag}-"));
        Self(guard.path().to_path_buf(), guard)
    }
}

fn shown_text(v: &Explorer) -> Option<String> {
    v.preview.as_ref()?.body.text()
}

fn size_row(v: &Explorer) -> Option<String> {
    let label = t!("col.size").to_string();
    v.preview_info_for_test()
        .into_iter()
        .find_map(|(k, value)| (k == label).then_some(value))
}

/// Notepad writing in place: truncates, then writes.
fn in_place(file: &Path, text: &str) {
    fs::write(file, text).expect("write in place");
}

/// A temp file moved over the original (`MoveFileEx` with `REPLACE_EXISTING`, as `rename` does).
fn temp_then_rename(file: &Path, text: &str) {
    let tmp = file.with_file_name("note.txt.tmp");
    fs::write(&tmp, text).expect("write the temp file");
    fs::rename(&tmp, file).expect("rename over the original");
}

/// Word: a temp file, the original renamed to a backup, the temp renamed to the name, the backup
/// deleted. The name has a new file under it.
fn word_dance(file: &Path, text: &str) {
    let dir = file.parent().expect("a folder");
    let tmp = dir.join("~WRL0001.tmp");
    let backup = dir.join("~WRD0002.tmp");
    fs::write(&tmp, text).expect("write the temp file");
    fs::rename(file, &backup).expect("original to backup");
    fs::rename(&tmp, file).expect("temp to the name");
    fs::remove_file(&backup).expect("delete the backup");
}

/// `ReplaceFileW`, with no backup file.
fn replace_file(file: &Path, text: &str) {
    use ::windows::Win32::Storage::FileSystem::{REPLACE_FILE_FLAGS, ReplaceFileW};
    use ::windows::core::{HSTRING, PCWSTR};
    let tmp = file.with_file_name("note.txt.new");
    fs::write(&tmp, text).expect("write the replacement");
    let (to, from) = (
        HSTRING::from(file.as_os_str()),
        HSTRING::from(tmp.as_os_str()),
    );
    // SAFETY: both paths are NUL-terminated strings that outlive the call; the optional
    // arguments are null.
    unsafe {
        ReplaceFileW(
            &to,
            &from,
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    }
    .expect("ReplaceFileW");
}

#[allow(
    clippy::print_stderr,
    reason = "a live test run by hand reports what it did"
)]
fn save_and_check(tag: &str, save: fn(&Path, &str), cx: &mut TestAppContext) {
    let folder = Folder::new(tag);
    let file = folder.0.join("note.txt");
    fs::write(&file, "old").expect("the first version");
    let services = Services {
        fs: Arc::new(RealFs),
        shell: Arc::new(FakeShell::default()),
        remotes: crate::remote_shell::test_remotes(),
    };
    let here = folder.0.to_str().expect("a UTF-8 path").to_owned();
    let (ws, cx) = setup_workspace_services(cx, services, folder.0);
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(&here));
    cx.simulate_resize(gpui::size(px(1200.), px(900.)));
    view.update(cx, |v, cx| {
        v.preview_pane = true;
        cx.notify();
    });
    cx.simulate_keystrokes("down");
    wait_until(cx, &view, "previewed", |v| {
        shown_text(v).is_some_and(|t| t.contains("old"))
    });
    let before = view.update(cx, |v, _| {
        (size_row(v), v.preview.as_ref().map(|p| p.entry.modified))
    });
    // Different write times even on a coarse clock.
    std::thread::sleep(Duration::from_millis(50));
    let text = "the saved text, longer than before";
    let saved_at = Instant::now();
    save(&file, text);
    wait_until(cx, &view, "the saved text", |v| {
        shown_text(v).is_some_and(|t| t.contains(text))
    });
    let after = view.update(cx, |v, _| {
        (size_row(v), v.preview.as_ref().map(|p| p.entry.modified))
    });
    assert_ne!(before.0, after.0, "{tag}: the size row stayed");
    assert_ne!(before.1, after.1, "{tag}: the modified time stayed");
    eprintln!(
        "{tag}: preview followed the save in {:?}",
        saved_at.elapsed()
    );
}

#[gpui::test]
#[ignore = "real disk and watcher (a temp folder of its own)"]
#[allow(
    clippy::print_stderr,
    reason = "a live test run by hand reports what it did"
)]
fn written_in_place(cx: &mut TestAppContext) {
    save_and_check("in-place", in_place, cx);
}

#[gpui::test]
#[ignore = "real disk and watcher (a temp folder of its own)"]
#[allow(
    clippy::print_stderr,
    reason = "a live test run by hand reports what it did"
)]
fn temp_file_renamed_over_it(cx: &mut TestAppContext) {
    save_and_check("temp-rename", temp_then_rename, cx);
}

#[gpui::test]
#[ignore = "real disk and watcher (a temp folder of its own)"]
#[allow(
    clippy::print_stderr,
    reason = "a live test run by hand reports what it did"
)]
fn word_rename_dance(cx: &mut TestAppContext) {
    save_and_check("word", word_dance, cx);
}

#[gpui::test]
#[ignore = "real disk and watcher (a temp folder of its own)"]
#[allow(
    clippy::print_stderr,
    reason = "a live test run by hand reports what it did"
)]
fn replace_file_w(cx: &mut TestAppContext) {
    save_and_check("replacefile", replace_file, cx);
}
