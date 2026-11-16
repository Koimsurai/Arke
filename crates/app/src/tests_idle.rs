//! GPUI tests for idle cost: a window with nothing changing asks for no redraws, the progress ring
//! redraws on its own ~30 fps timer instead of every display frame, and with no window left Arke
//! frees its caches (`resident.rs`).

use std::cell::Cell;
use std::rc::Rc;

use super::*;
use crate::op_center::OpCenter;
use crate::shell::OpOutcome;

/// Animations on for the test's lifetime.
struct Motion;

impl Motion {
    fn on() -> Self {
        crate::motion::force_for_test(true);
        Self
    }
}

impl Drop for Motion {
    fn drop(&mut self) {
        crate::motion::force_for_test(false);
    }
}

/// Adds each notification of `entity` (each one asks for a redraw of the window) to `count`.
fn count_notifies<T: 'static>(
    cx: &mut VisualTestContext,
    entity: &Entity<T>,
    count: &Rc<Cell<usize>>,
) -> gpui::Subscription {
    let count = Rc::clone(count);
    cx.update(|_, cx| cx.observe(entity, move |_, _| count.set(count.get() + 1)))
}

/// Moves the test clock on by `total` in 10 ms steps, running what becomes ready.
fn let_time_pass(cx: &mut VisualTestContext, total: Duration) {
    let step = Duration::from_millis(10);
    let mut passed = Duration::ZERO;
    while passed < total {
        cx.executor().advance_clock(step);
        cx.run_until_parked();
        passed += step;
    }
}

/// Once a folder has loaded and nothing changes, nothing in the window asks for a redraw, so the
/// platform can stop drawing it (the vsync thread sleeps, see third_party/README.md).
#[gpui::test]
fn an_idle_window_asks_for_no_redraws(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(
        r"C:\idle",
        vec![dir("docs"), file("a.txt", 10), file("b.txt", 20)],
    );
    let (ws, cx) = setup_workspace(cx, fs, r"C:\idle");
    let view = ws.update(cx, |ws, _| ws.active_view().clone());
    wait_until(cx, &view, "load", loaded(r"C:\idle"));
    let_time_pass(cx, Duration::from_secs(2));

    let redraws = Rc::new(Cell::new(0));
    let _workspace = count_notifies(cx, &ws, &redraws);
    let _explorer = count_notifies(cx, &view, &redraws);
    let_time_pass(cx, Duration::from_secs(5));
    assert_eq!(redraws.get(), 0, "an idle window kept asking for redraws");
}

/// While a job runs, the progress ring redraws about 30 times a second (not at the display's
/// refresh rate), and stops once the job is done.
#[gpui::test]
fn the_progress_ring_redraws_at_about_30_fps_and_stops_when_done(cx: &mut TestAppContext) {
    let _motion = Motion::on();
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\ops", vec![file("a.txt", 10)]);
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, fs, shell, r"C:\ops");
    wait_until(cx, &view, "load", loaded(r"C:\ops"));

    // Total unknown: the spinner.
    let (id, _progress) = cx.update(|_, cx| OpCenter::begin(cx, "copy", None));
    cx.run_until_parked();
    let redraws = Rc::new(Cell::new(0));
    let _explorer = count_notifies(cx, &view, &redraws);
    let_time_pass(cx, Duration::from_secs(1));
    // 30 ring frames, plus the job list's own 5 updates a second.
    let frames = redraws.get();
    assert!(
        (25..=45).contains(&frames),
        "{frames} redraws in a second while the spinner turns"
    );

    cx.update(|_, cx| OpCenter::finish::<String>(cx, id, "copy", &Ok(OpOutcome::Done)));
    let_time_pass(cx, Duration::from_millis(200));
    redraws.set(0);
    let_time_pass(cx, Duration::from_secs(1));
    assert!(
        redraws.get() <= 6,
        "{} redraws in a second after the job finished",
        redraws.get()
    );
}

/// With no window left, Arke frees its decoded and in-memory thumbnails and goes to background
/// priority; a new window brings normal priority back.
#[gpui::test]
fn closing_the_last_window_frees_thumbnails(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\pics", vec![file("a.png", 100), file("b.png", 100)]);
    fs.set_content(r"C:\pics\a.png", png_bytes(400, 200));
    fs.set_content(r"C:\pics\b.png", png_bytes(50, 50));
    let shell = Arc::new(FakeShell::default());
    let (view, cx) = setup(cx, Arc::clone(&fs), shell, r"C:\pics");
    cx.update(|_, cx| crate::resident::watch(cx));
    wait_until(cx, &view, "load", loaded(r"C:\pics"));
    cx.simulate_keystrokes("ctrl-shift-2");
    let deadline = Instant::now() + Duration::from_secs(20);
    while loaded_media(&view, cx).len() < 2 {
        assert!(Instant::now() < deadline, "{:?}", loaded_media(&view, cx));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let media = view.update(cx, |v, _| v.media.clone());
    assert!(media.read_with(cx, |m, _| m.decoded_bytes()) > 0);
    assert!(crate::media::thumb_cache().memory_bytes() > 0);
    assert!(!crate::resident::is_resident());

    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    assert!(crate::resident::is_resident());
    assert_eq!(media.read_with(cx, |m, _| m.decoded_bytes()), 0);
    assert_eq!(media.read_with(cx, |m, _| m.loaded_count()), 0);
    assert_eq!(crate::media::thumb_cache().memory_bytes(), 0);

    crate::resident::leave();
    assert!(!crate::resident::is_resident());
}
