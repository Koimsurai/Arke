//! Live: Delete and Ctrl+Z through Arke itself, with the real file system, the real Windows
//! shell and the real Recycle Bin, for the fast Recycle Bin and its switch in Settings. Only
//! this test's own `arke-test-fastbin-*` temp folder is touched, and what it recycles comes
//! back (an interrupted run's leftovers are purged on the next run).
//!
//! Run with `cargo test -p arke-app --bin arke live_recycle -- --ignored --test-threads 1`.

use std::sync::atomic::Ordering;

use arke_core::fs::RealFs;

use gpui::px;

use super::*;
use crate::win::WinShell;
use crate::win_fast_recycle::test_support::{Sandbox, recycled_from};
use crate::win_fast_recycle::{TAKEN, enabled};

const FILES: usize = 3000;

fn fill(sb: &Sandbox, folder: &str, files: usize) {
    for i in 0..files {
        sb.file(
            &format!(r"{folder}\sub{}\f{i:04}.txt", i % 10),
            &format!("{folder} {i}"),
        );
    }
}

fn intact(sb: &Sandbox, folder: &str, files: usize) -> bool {
    (0..files).all(|i| {
        fs_read(&sb.0.join(format!(r"{folder}\sub{}\f{i:04}.txt", i % 10)))
            == Some(format!("{folder} {i}"))
    })
}

fn fs_read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Waits for the list to settle after an operation: nothing running and `pred` holds. The
/// shell works on real files here, so the deadline is longer than for FakeFs.
fn settle(
    cx: &mut VisualTestContext,
    view: &Entity<Explorer>,
    what: &str,
    pred: impl Fn(&Explorer) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        cx.run_until_parked();
        if view.update(cx, |v, _| v.ops_running == 0 && pred(v)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
#[allow(clippy::print_stderr)]
#[ignore = "uses the real Recycle Bin (only this test's own temp items, restored at the end)"]
fn delete_and_undo_through_arke_with_the_switch_on_and_off(cx: &mut TestAppContext) {
    let sb = Sandbox::new("arke");
    fill(&sb, "big", FILES);
    fill(&sb, "small", 50);
    let start = sb.0.clone();
    let services = Services {
        fs: Arc::new(RealFs),
        shell: Arc::new(WinShell),
        remotes: crate::remote_shell::test_remotes(),
    };
    let (ws, cx) = setup_workspace_services(cx, services, start.clone());
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    let here = start.to_str().unwrap();
    settle(cx, &view, "load", |v| {
        loaded(here)(v) && names(v) == ["big", "small"]
    });

    // A fresh install: the setting is on and applied.
    assert!(settings(cx).fast_recycle, "on by default");
    assert!(enabled(), "applied at startup");

    // Delete on the big folder: it goes the fast way, Windows lists it, and Ctrl+Z brings it
    // back whole.
    let taken = TAKEN.load(Ordering::Relaxed);
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["big"]);
    let started = Instant::now();
    cx.simulate_keystrokes("delete");
    settle(cx, &view, "big recycled", |v| names(v) == ["small"]);
    let took = started.elapsed();
    eprintln!("{FILES} files to the Recycle Bin through Arke: {took:?}");
    assert_eq!(
        TAKEN.load(Ordering::Relaxed),
        taken + 1,
        "taken by the fast path"
    );
    assert!(!sb.0.join("big").exists());
    let listed = recycled_from(&sb.0);
    assert_eq!(
        listed.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        ["big"],
        "Windows lists it"
    );
    cx.simulate_keystrokes("ctrl-z");
    settle(cx, &view, "big restored", |v| names(v) == ["big", "small"]);
    assert!(
        intact(&sb, "big", FILES),
        "every file back with its contents"
    );
    assert!(recycled_from(&sb.0).is_empty(), "and gone from the bin");

    // Settings › Files: the switch is on; clicking it turns the fast path off. (It is the last
    // card on the page, so the window is tall enough to show it without scrolling.)
    cx.simulate_resize(gpui::size(px(1400.), px(3000.)));
    cx.dispatch_action(crate::workspace::OpenSettings);
    cx.run_until_parked();
    ws.update(cx, |ws, cx| {
        ws.settings_category_for_test(&t!("settings.files"), cx);
    });
    cx.run_until_parked();
    let bounds = cx
        .debug_bounds("fast-recycle")
        .expect("the Fast Recycle Bin switch should be drawn");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(!settings(cx).fast_recycle, "switched off");
    assert!(!enabled(), "and applied");
    ws.update_in(cx, Workspace::close_settings_for_test);
    cx.run_until_parked();

    // Now the Windows engine does it, and Ctrl+Z still brings it back.
    let taken = TAKEN.load(Ordering::Relaxed);
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    assert_eq!(view.update(cx, |v, _| selected_names(v)), ["small"]);
    cx.simulate_keystrokes("delete");
    settle(cx, &view, "small recycled", |v| names(v) == ["big"]);
    assert_eq!(TAKEN.load(Ordering::Relaxed), taken, "left to the engine");
    assert_eq!(
        recycled_from(&sb.0)
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["small"]
    );
    cx.simulate_keystrokes("ctrl-z");
    settle(cx, &view, "small restored", |v| {
        names(v) == ["big", "small"]
    });
    assert!(intact(&sb, "small", 50));
    assert!(recycled_from(&sb.0).is_empty());
}
