//! GPUI tests for the Quick Access sync (`quick_access.rs`, #45): the background read, the
//! favorites it adds to, and the pause between reads.

use std::time::{Duration, Instant};

use gpui::TestAppContext;

use super::*;
use crate::settings::{Settings, SettingsStore};

fn p(name: &str) -> PathBuf {
    PathBuf::from(r"C:\w").join(name)
}

fn current(cx: &mut TestAppContext) -> Settings {
    cx.update(|cx| SettingsStore::global(cx).read(cx).settings.clone())
}

/// Starts a read with the settings marked as read from the file.
fn refresh(cx: &mut TestAppContext, shell: &Arc<FakeShell>) {
    // The read runs on a thread of its own, like the real one.
    cx.executor().allow_parking();
    let shell: Arc<dyn Shell> = Arc::<FakeShell>::clone(shell);
    cx.update(|cx| {
        SettingsStore::global(cx).update(cx, SettingsStore::mark_loaded);
        crate::quick_access::refresh(&shell, Vec::new(), cx);
    });
}

/// Lets the read thread and the update that follows it finish.
fn settle(cx: &mut TestAppContext, done: impl Fn(&Settings) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if done(&current(cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the sync");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
fn pins_from_windows_become_favorites(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    *shell.quick_access.lock() = Some(vec![p("a")]);
    refresh(cx, &shell);
    settle(cx, |st| st.quick_access_seen.is_some());
    let st = current(cx);
    assert!(st.tiles(&[]).contains(&p("a")));
    assert_eq!(st.quick_access_seen, Some(vec![p("a")]));
}

#[gpui::test]
fn an_unreadable_quick_access_changes_nothing(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    *shell.quick_access.lock() = None;
    refresh(cx, &shell);
    // The read ends, then a later one is allowed to start again only after the pause.
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    let st = current(cx);
    assert_eq!(st.quick_access_seen, None);
    assert_eq!(st.favorites, None);
}

#[gpui::test]
fn reads_are_not_repeated_within_the_pause(cx: &mut TestAppContext) {
    let shell = Arc::new(FakeShell::default());
    *shell.quick_access.lock() = Some(vec![p("a")]);
    refresh(cx, &shell);
    settle(cx, |st| st.quick_access_seen.is_some());

    *shell.quick_access.lock() = Some(vec![p("a"), p("b")]);
    refresh(cx, &shell);
    std::thread::sleep(Duration::from_millis(200));
    cx.run_until_parked();
    assert!(!current(cx).tiles(&[]).contains(&p("b")), "too soon");
}
