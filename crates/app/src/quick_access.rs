//! Folders pinned to Quick Access in Windows' File Explorer also become Arke favorites (#45).
//!
//! One way only: Arke reads Explorer's pins and never changes them. A pin that appears in
//! Windows is added to the favorites once; unpinning in Windows leaves Arke's favorite alone, and
//! a favorite removed in Arke stays removed while the folder is still pinned in Windows (that is
//! what `Settings::quick_access_seen` remembers).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Global};

use crate::settings::{PinPlace, Settings, SettingsStore};
use crate::shell::{Place, Shell};

/// Reads are skipped when the last one ended less than this long ago.
const MIN_GAP: Duration = Duration::from_secs(2);

/// Case-insensitive, separator-normalized form of a path used to compare folders.
fn key(path: &std::path::Path) -> String {
    crate::place_names::key(path)
}

/// Whether the folder is already a favorite (on the tiles or in the list), ignoring case.
pub(crate) fn is_favorite(settings: &Settings, path: &std::path::Path, places: &[Place]) -> bool {
    let wanted = key(path);
    settings
        .tiles(places)
        .iter()
        .chain(&settings.pinned)
        .any(|p| key(p) == wanted)
}

/// Adds the folders newly pinned in Windows to the favorites, and remembers what Windows has
/// pinned. Returns whether the settings changed.
///
/// A folder is added when Windows has it pinned, the last sync did not know about it (all of them
/// on the first sync) and it is not a favorite yet. Nothing is ever removed.
pub(crate) fn sync(settings: &mut Settings, windows_pinned: &[PathBuf], places: &[Place]) -> bool {
    // The first sync for someone who already arranged their favorites (upgrading from before
    // the sync): Windows' current pins are only noted, not added all at once; ones pinned later
    // still come in.
    let arranged = settings.favorites.is_some() || !settings.pinned.is_empty();
    if settings.quick_access_seen.is_none() && arranged {
        settings.quick_access_seen = Some(windows_pinned.to_vec());
        return true;
    }
    let seen: HashSet<String> = settings
        .quick_access_seen
        .iter()
        .flatten()
        .map(|p| key(p))
        .collect();
    let mut changed = false;
    for path in windows_pinned {
        if seen.contains(&key(path)) || is_favorite(settings, path, places) {
            continue;
        }
        if settings.pin(path, places) != PinPlace::Already {
            changed = true;
        }
    }
    if settings.quick_access_seen.as_deref() != Some(windows_pinned) {
        settings.quick_access_seen = Some(windows_pinned.to_vec());
        changed = true;
    }
    changed
}

/// When the last read ended, and whether one is running.
#[derive(Default)]
struct State {
    running: bool,
    finished: Option<Instant>,
}

impl Global for State {}

/// Reads Explorer's Quick Access pins in the background and adds new ones to the favorites.
/// Cheap to call often (on startup and whenever a window is activated): it does nothing while a
/// read is running, within [`MIN_GAP`] of the last one, or before the settings file was read.
pub(crate) fn refresh(shell: &Arc<dyn Shell>, places: Vec<Place>, cx: &mut App) {
    if !SettingsStore::global(cx).read(cx).loaded {
        return;
    }
    let state = cx.default_global::<State>();
    if state.running || state.finished.is_some_and(|t| t.elapsed() < MIN_GAP) {
        return;
    }
    state.running = true;
    let (tx, rx) = flume::bounded::<Option<Vec<PathBuf>>>(1);
    let shell = Arc::clone(shell);
    crate::explorer::spawn_thread("quick-access", move || {
        let _ = tx.send(shell.quick_access_pinned());
    });
    cx.spawn(async move |cx| {
        let pinned = rx.recv_async().await.ok().flatten();
        cx.update(|cx| {
            let state = cx.default_global::<State>();
            state.running = false;
            state.finished = Some(Instant::now());
            let Some(pinned) = pinned else {
                return;
            };
            let store = SettingsStore::global(cx);
            store.update(cx, |store, cx| {
                store.update(
                    |settings| {
                        if sync(settings, &pinned, &places) {
                            log::info!("synced {} Quick Access pins", pinned.len());
                        }
                    },
                    cx,
                );
            });
        });
    })
    .detach();
}

/// Reads the folders pinned to Quick Access (`System.Home.IsPinned`) from the Quick Access shell
/// folder. `None` if it cannot be read; an item whose pin state is unknown is skipped. Needs a
/// background thread.
#[cfg(windows)]
pub(crate) fn read_windows_pins() -> Option<Vec<PathBuf>> {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::SystemServices::{SFGAO_FOLDER, SFGAO_STREAM};
    use windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName;
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, IShellItem, IShellItem2, SHCreateItemFromParsingName,
        SIGDN_FILESYSPATH,
    };
    use windows::core::{Interface, w};

    let _com = crate::win_fileops::Com::init().ok()?;
    // SAFETY: COM is initialized on this thread; interfaces are freshly obtained here and the
    // strings Windows returns are copied before being freed.
    unsafe {
        let mut pinned_key = PROPERTYKEY::default();
        if let Err(e) = PSGetPropertyKeyFromName(w!("System.Home.IsPinned"), &raw mut pinned_key) {
            log::debug!("no System.Home.IsPinned property: {e:?}");
            return None;
        }
        let home: IShellItem =
            SHCreateItemFromParsingName(w!("shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}"), None)
                .ok()?;
        let items = home
            .BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems)
            .ok()?;
        let mut found = Vec::new();
        let mut checked = 0usize;
        let mut known = 0usize;
        loop {
            let mut slot = [None];
            let mut fetched = 0u32;
            if items.Next(&mut slot, Some(&raw mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = slot[0].take() else {
                break;
            };
            checked += 1;
            // Only file system folders (not Zip files, which also report as folders).
            let Ok(attrs) = item.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM) else {
                continue;
            };
            if attrs.0 & SFGAO_FOLDER.0 == 0 || attrs.0 & SFGAO_STREAM.0 != 0 {
                continue;
            }
            let Ok(details) = item.cast::<IShellItem2>() else {
                continue;
            };
            let Ok(is_pinned) = details.GetBool(std::ptr::from_ref(&pinned_key)) else {
                continue;
            };
            known += 1;
            if !is_pinned.as_bool() {
                continue;
            }
            let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else {
                continue;
            };
            let path = name.to_string().ok();
            CoTaskMemFree(Some(name.0 as *const _));
            if let Some(path) = path.filter(|p| !p.is_empty()) {
                found.push(PathBuf::from(path));
            }
        }
        // Items were there but none could tell whether it is pinned: the property is not
        // available here, so this is not "nothing is pinned".
        if checked > 0 && known == 0 && found.is_empty() {
            return None;
        }
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::FAVORITES_MAX;
    use crate::shell::{KnownFolder, PlaceKind};

    fn p(name: &str) -> PathBuf {
        PathBuf::from(r"C:\w").join(name)
    }

    fn places() -> Vec<Place> {
        [
            ("Desktop", KnownFolder::Desktop),
            ("Downloads", KnownFolder::Downloads),
        ]
        .into_iter()
        .map(|(label, kind)| Place {
            label: label.into(),
            path: PathBuf::from(r"C:\Users\me").join(label),
            kind: PlaceKind::Folder(kind),
        })
        .collect()
    }

    #[test]
    fn first_sync_imports_what_arke_lacks() {
        let mut st = Settings::default();
        assert!(sync(&mut st, &[p("a"), p("b")], &places()));
        let tiles = st.tiles(&places());
        assert!(tiles.contains(&p("a")) && tiles.contains(&p("b")));
        assert_eq!(st.quick_access_seen, Some(vec![p("a"), p("b")]));
    }

    #[test]
    fn default_tiles_and_existing_pins_are_not_doubled() {
        let mut st = Settings {
            pinned: vec![p("listed")],
            ..Settings::default()
        };
        let desktop = PathBuf::from(r"c:\USERS\me\desktop\");
        assert!(sync(&mut st, &[desktop, p("LISTED")], &places()));
        assert_eq!(st.favorites, None, "default tiles stay the defaults");
        assert_eq!(st.pinned, vec![p("listed")]);
    }

    /// Upgrading with favorites already arranged: Windows' pins at that time are noted, not
    /// poured into the favorites; one pinned later still comes in.
    #[test]
    fn arranged_favorites_only_take_pins_made_later() {
        let mut st = Settings {
            pinned: vec![p("mine")],
            ..Settings::default()
        };
        assert!(sync(&mut st, &[p("a"), p("b")], &places()));
        assert!(!st.is_pinned(&p("a"), &places()) && !st.is_pinned(&p("b"), &places()));
        assert_eq!(st.quick_access_seen, Some(vec![p("a"), p("b")]));
        assert!(sync(&mut st, &[p("a"), p("b"), p("c")], &places()));
        assert!(st.is_pinned(&p("c"), &places()));
        assert!(!st.is_pinned(&p("a"), &places()));
    }

    #[test]
    fn a_favorite_removed_in_arke_stays_removed() {
        let mut st = Settings::default();
        sync(&mut st, &[p("a")], &places());
        st.unpin(&p("a"), &places());
        assert!(!sync(&mut st, &[p("a")], &places()));
        assert!(!st.is_pinned(&p("a"), &places()));
    }

    #[test]
    fn a_pin_added_later_is_imported() {
        let mut st = Settings::default();
        sync(&mut st, &[p("a")], &places());
        assert!(sync(&mut st, &[p("a"), p("b")], &places()));
        assert!(st.is_pinned(&p("b"), &places()));
    }

    #[test]
    fn unpinning_in_windows_keeps_the_favorite() {
        let mut st = Settings::default();
        sync(&mut st, &[p("a")], &places());
        assert!(sync(&mut st, &[], &places()));
        assert!(st.is_pinned(&p("a"), &places()));
        assert_eq!(st.quick_access_seen, Some(Vec::new()));
        // Pinned again in Windows later: it is new again, so it comes back after being removed.
        st.unpin(&p("a"), &places());
        assert!(sync(&mut st, &[p("a")], &places()));
        assert!(st.is_pinned(&p("a"), &places()));
    }

    #[test]
    fn nothing_changes_when_nothing_is_new() {
        let mut st = Settings::default();
        sync(&mut st, &[p("a")], &places());
        assert!(!sync(&mut st, &[p("a")], &places()));
    }

    #[test]
    fn full_tiles_send_it_to_the_list() {
        let mut st = Settings {
            favorites: Some((0..FAVORITES_MAX).map(|i| p(&format!("t{i}"))).collect()),
            // Synced before, with nothing pinned in Windows then: "extra" is new.
            quick_access_seen: Some(Vec::new()),
            ..Settings::default()
        };
        assert!(sync(&mut st, &[p("extra")], &places()));
        assert_eq!(st.pinned, vec![p("extra")]);
        assert_eq!(st.tiles(&places()).len(), FAVORITES_MAX);
    }
}
