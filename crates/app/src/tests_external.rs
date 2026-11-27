//! GPUI tests for the external copy and preview programs (`external.rs`): copies and moves, and
//! Space, go to the program the settings name, started through the shell's launcher (the fake one
//! here records it).

use super::*;
use crate::settings::SettingsStore;

fn set_programs(cx: &mut VisualTestContext, copy: &str, preview: &str) {
    let (copy, preview) = (copy.to_owned(), preview.to_owned());
    cx.update(|_, cx| {
        SettingsStore::global(cx).update(cx, |s, cx| {
            s.update(
                |st| {
                    st.external_copy = copy;
                    st.external_preview = preview;
                },
                cx,
            );
        });
    });
}

fn runs(shell: &FakeShell) -> Vec<(PathBuf, String)> {
    shell.program_runs.lock().clone()
}

/// Waits until the fake shell has started `n` programs.
fn wait_runs(cx: &mut VisualTestContext, shell: &FakeShell, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while shell.program_runs.lock().len() < n && Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::yield_now();
    }
    assert_eq!(shell.program_runs.lock().len(), n, "programs started");
}

/// Copies (or cuts) `a.txt` in `C:\q` and pastes it into `C:\q2`, then waits for the job to end.
fn paste_one(view: &Entity<Explorer>, cx: &mut VisualTestContext, shell: &FakeShell, cut: bool) {
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\q"), NavKind::New, cx);
    });
    wait_until(cx, view, "load q", loaded(r"C:\q"));
    cx.simulate_keystrokes(if cut { "home ctrl-x" } else { "home ctrl-c" });
    wait_shell("put on clipboard", || {
        shell.clipboard().is_some_and(|c| c.cut == cut)
    });
    view.update(cx, |v, cx| {
        v.navigate(PathBuf::from(r"C:\q2"), NavKind::New, cx);
    });
    wait_until(cx, view, "load q2", loaded(r"C:\q2"));
    let started = (shell.ops().len(), runs(shell).len());
    cx.simulate_keystrokes("ctrl-v");
    wait_until(cx, view, "paste done", |v| {
        v.ops_running == 0 && (shell.ops().len(), runs(shell).len()) != started
    });
}

/// With an external copy program set, a paste starts it with the filled-in command line instead
/// of copying in Arke; a move goes to it only when its arguments have `{op}`, and a setting that
/// can't be used leaves the job to Arke.
#[gpui::test]
fn copies_go_to_the_external_copy_program_when_set(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a.txt", 1)]);
    fs.insert_dir(r"C:\q2", vec![]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));
    let program = PathBuf::from(r"C:\Tools\copier.exe");

    set_programs(cx, r#""C:\Tools\copier.exe" {op} {sources} {dest}"#, "");
    paste_one(&view, cx, &shell, false);
    assert_eq!(
        runs(&shell),
        [(program.clone(), r#"Copy "C:\q\a.txt" "C:\q2""#.to_owned())]
    );
    assert!(shell.ops().is_empty(), "Arke didn't copy it as well");
    paste_one(&view, cx, &shell, true);
    assert_eq!(
        runs(&shell)[1],
        (program.clone(), r#"Move "C:\q\a.txt" "C:\q2""#.to_owned())
    );
    assert!(shell.ops().is_empty());

    // Without `{op}` the program couldn't tell a move from a copy: Arke moves.
    set_programs(cx, r"C:\Tools\copier.exe /y", "");
    paste_one(&view, cx, &shell, true);
    assert_eq!(
        shell.ops(),
        [FileOp::Move {
            sources: vec![PathBuf::from(r"C:\q\a.txt")],
            dest: PathBuf::from(r"C:\q2"),
        }]
    );
    paste_one(&view, cx, &shell, false);
    assert_eq!(
        runs(&shell)[2],
        (program, r#"/y "C:\q\a.txt" "C:\q2""#.to_owned()),
        "copies still go"
    );

    // A placeholder it doesn't know: Arke copies.
    set_programs(cx, r"C:\Tools\copier.exe {bogus}", "");
    paste_one(&view, cx, &shell, false);
    assert_eq!(runs(&shell).len(), 3);
    assert!(matches!(shell.ops().last(), Some(FileOp::Copy { .. })));

    // Off again: Arke copies.
    set_programs(cx, "", "");
    paste_one(&view, cx, &shell, false);
    assert_eq!(runs(&shell).len(), 3);
    assert_eq!(shell.ops().len(), 3);
}

/// With the setting empty Space opens Arke's Quick Look; set, Space starts the program with the
/// item every time (it toggles its own preview, so Space again closes it), and Arke's stays
/// closed.
#[gpui::test]
fn space_goes_to_the_external_preview_program_when_set(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a b.txt", 10)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));

    cx.simulate_keystrokes("down space");
    assert!(
        view.update(cx, |v, _| v.quick_look),
        "off: Arke's Quick Look"
    );
    cx.simulate_keystrokes("space");
    assert!(!view.update(cx, |v, _| v.quick_look));
    cx.run_until_parked();
    assert!(runs(&shell).is_empty());

    let exe = r"C:\Users\u\AppData\Local\Programs\QuickLook\QuickLook.exe";
    set_programs(cx, "", &format!("\"{exe}\" {{file}}"));
    cx.simulate_keystrokes("space");
    assert!(!view.update(cx, |v, _| v.quick_look), "on: not Arke's");
    wait_runs(cx, &shell, 1);
    let run = (PathBuf::from(exe), r#""C:\q\a b.txt""#.to_owned());
    assert_eq!(runs(&shell), std::slice::from_ref(&run));
    cx.simulate_keystrokes("space");
    wait_runs(cx, &shell, 2);
    assert_eq!(runs(&shell), [run.clone(), run]);
    assert!(!view.update(cx, |v, _| v.quick_look));
}

/// A preview program that can't be started (its exe is gone) or a setting that can't be read:
/// Space opens Arke's Quick Look instead, with no error shown.
#[gpui::test]
fn space_falls_back_to_quick_look_when_the_preview_program_fails(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a.txt", 10)]);
    let shell = Arc::new(FakeShell::default());
    shell.program_fails.store(true, Ordering::Relaxed);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));

    set_programs(cx, "", r"C:\Gone\viewer.exe {file}");
    cx.simulate_keystrokes("down space");
    wait_runs(cx, &shell, 1);
    wait_until(cx, &view, "Quick Look opened", |v| v.quick_look);
    view.update(cx, |v, _| {
        assert_eq!(v.error, None);
        assert_eq!(v.notice, None);
    });
    // Space again closes it, as always.
    cx.simulate_keystrokes("space");
    assert!(!view.update(cx, |v, _| v.quick_look));

    // A placeholder the setting doesn't have: Quick Look right away, nothing started.
    set_programs(cx, "", r"C:\Tools\viewer.exe {bogus}");
    cx.simulate_keystrokes("space");
    assert!(view.update(cx, |v, _| v.quick_look));
    cx.run_until_parked();
    assert_eq!(runs(&shell).len(), 1);
}

/// Waits until the fake shell has had `n` lines written to named pipes.
fn wait_pipe_writes(cx: &mut VisualTestContext, shell: &FakeShell, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while shell.pipe_writes.lock().len() < n && Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::yield_now();
    }
    assert_eq!(shell.pipe_writes.lock().len(), n, "pipe writes");
}

const PIPE_LINE: &str = r"pipe:\\.\pipe\arke-test-tool Toggle|{file}";

/// The pipe form of the preview setting writes one line to the pipe, as the item's path, and
/// starts nothing; Space again writes again (the program toggles).
#[gpui::test]
fn space_writes_to_the_preview_pipe_without_starting_a_program(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a b.txt", 10)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));

    set_programs(
        cx,
        "",
        &format!(r#"{PIPE_LINE} || "C:\Gone\viewer.exe" {{file}}"#),
    );
    cx.simulate_keystrokes("down space");
    wait_pipe_writes(cx, &shell, 1);
    let line = (
        r"\\.\pipe\arke-test-tool".to_owned(),
        "Toggle|C:\\q\\a b.txt\r\n".to_owned(),
    );
    assert_eq!(
        shell.pipe_writes.lock().clone(),
        std::slice::from_ref(&line)
    );
    cx.simulate_keystrokes("space");
    wait_pipe_writes(cx, &shell, 2);
    assert_eq!(shell.pipe_writes.lock().clone(), [line.clone(), line]);
    cx.run_until_parked();
    assert!(runs(&shell).is_empty(), "the pipe was there");
    assert!(!view.update(cx, |v, _| v.quick_look), "not Arke's");
}

/// No pipe (the program isn't running): its fallback command line starts it, and without one
/// Arke's Quick Look opens, as for a program that doesn't start.
#[gpui::test]
fn space_starts_the_fallback_when_the_pipe_is_missing(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\q", vec![file("a b.txt", 10)]);
    let shell = Arc::new(FakeShell::default());
    shell.pipe_missing.store(true, Ordering::Relaxed);
    let (view, cx) = setup(cx, fs, Arc::clone(&shell), r"C:\q");
    wait_until(cx, &view, "load", loaded(r"C:\q"));

    let exe = r"C:\Tools\viewer.exe";
    set_programs(cx, "", &format!(r#"{PIPE_LINE} || "{exe}" {{file}}"#));
    cx.simulate_keystrokes("down space");
    wait_runs(cx, &shell, 1);
    assert_eq!(
        shell.pipe_writes.lock().len(),
        1,
        "the pipe was tried first"
    );
    assert_eq!(
        runs(&shell),
        [(PathBuf::from(exe), r#""C:\q\a b.txt""#.to_owned())]
    );
    cx.run_until_parked();
    assert!(!view.update(cx, |v, _| v.quick_look), "the program took it");

    // Pipe alone: nothing to start, so Quick Look.
    set_programs(cx, "", PIPE_LINE);
    cx.simulate_keystrokes("space");
    wait_pipe_writes(cx, &shell, 2);
    wait_until(cx, &view, "Quick Look opened", |v| v.quick_look);
    assert_eq!(runs(&shell).len(), 1, "no program to start");
    view.update(cx, |v, _| {
        assert_eq!(v.error, None);
        assert_eq!(v.notice, None);
    });
}
