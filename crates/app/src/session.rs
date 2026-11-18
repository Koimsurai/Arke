//! The last session: the tabs and place of every open window, kept in the settings as they change,
//! so "New windows open to: Last tabs" brings all of them back, like a browser.
//!
//! The session is what was open when Arke last quit. Closing one of several windows drops it from
//! the session; the last window closing keeps it, and Quit in the notification area or Windows
//! shutting down closes no window one by one, so every window open then stays.

use gpui::{AnyWindowHandle, App, Global, WindowId};

use crate::settings::{Session, SessionWindow, SettingsStore, WindowPlace};
use crate::workspace::{LastActiveWindow, Workspace};

/// The open windows' part of the session, the window used last first.
#[derive(Default)]
struct OpenWindows(Vec<(WindowId, SessionWindow)>);

impl Global for OpenWindows {}

/// Starts following windows closing, once per app, with the first window saved.
fn init(cx: &mut App) {
    if cx.has_global::<OpenWindows>() {
        return;
    }
    cx.set_global(OpenWindows::default());
    cx.on_window_closed(closed).detach();
}

/// A window's tabs changed, or are saved for the first time: a window new to the session is the
/// one used last. `place` is where a new one is.
pub(crate) fn save_tabs(
    window: WindowId,
    session: Session,
    place: Option<WindowPlace>,
    cx: &mut App,
) {
    change(cx, |open| {
        if let Some((_, saved)) = open.iter_mut().find(|(id, _)| *id == window) {
            saved.session = session;
        } else {
            open.insert(0, (window, SessionWindow { session, place }));
        }
    });
}

/// A window moved or was resized.
pub(crate) fn save_place(window: WindowId, place: WindowPlace, cx: &mut App) {
    change(cx, |open| {
        if let Some((_, saved)) = open.iter_mut().find(|(id, _)| *id == window) {
            saved.place = Some(place);
        }
    });
}

/// A window became the active one: it is where folders from outside go
/// ([`LastActiveWindow`]), and it comes first in the session, so a restore puts it in front again.
pub(crate) fn activated(handle: AnyWindowHandle, cx: &mut App) {
    cx.set_global(LastActiveWindow(Some(handle)));
    let window = handle.window_id();
    change(cx, |open| {
        if let Some(ix) = open.iter().position(|(id, _)| *id == window)
            && ix > 0
        {
            let entry = open.remove(ix);
            open.insert(0, entry);
        }
    });
}

fn closed(cx: &mut App, window: WindowId) {
    let others = cx
        .windows()
        .iter()
        .any(|w| w.downcast::<Workspace>().is_some());
    if others {
        change(cx, |open| open.retain(|(id, _)| *id != window));
    } else {
        // The last window: the saved session keeps it. The next window to save starts the session
        // anew, so this one isn't saved beside it.
        cx.global_mut::<OpenWindows>().0.clear();
    }
}

/// Applies `f` to the open windows and writes the session if that changed anything.
fn change(cx: &mut App, f: impl FnOnce(&mut Vec<(WindowId, SessionWindow)>)) {
    init(cx);
    let open = &mut cx.global_mut::<OpenWindows>().0;
    let before = open.clone();
    f(open);
    if *open == before || open.is_empty() {
        return;
    }
    let windows: Vec<SessionWindow> = open.iter().map(|(_, w)| w.clone()).collect();
    SettingsStore::global(cx).update(cx, |store, cx| {
        store.update(
            |st| {
                st.session = windows[0].session.clone();
                st.session_windows = windows;
            },
            cx,
        );
    });
}
