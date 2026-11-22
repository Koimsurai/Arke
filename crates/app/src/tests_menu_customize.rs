//! GPUI tests for the context menu as customized in Settings › Context menu: the shorter New
//! entries (#190).

use super::*;
use crate::settings::{Settings, SettingsStore};

fn change(cx: &mut VisualTestContext, f: impl FnOnce(&mut Settings) + 'static) {
    cx.update(|_, cx| SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx)));
    cx.run_until_parked();
}

/// Highlights top-level row `ix` of the open menu, as the keyboard would.
fn highlight(view: &Entity<Explorer>, cx: &mut VisualTestContext, ix: usize) {
    view.update(cx, |v, cx| {
        if let Some(menu) = v.context_menu.as_mut() {
            menu.highlight_for_test(ix);
        }
        cx.notify();
    });
}

fn labels_at(view: &Entity<Explorer>, cx: &mut VisualTestContext, level: usize) -> Vec<String> {
    view.update(cx, |v, _| {
        v.context_menu
            .as_ref()
            .map_or_else(Vec::new, |m| m.labels_at(level))
    })
}

/// A folder's menu has "New file…" and "New folder…" in "New", reached with the keyboard too,
/// and New folder as a button in the icon row; a file's menu has neither. "Move to new folder"
/// can be turned off (#190).
#[gpui::test]
fn the_new_entries_are_grouped_and_move_to_new_folder_can_be_hidden(cx: &mut TestAppContext) {
    let fs = FakeFs::new();
    fs.insert_dir(r"C:\m", vec![dir("docs"), file("a.txt", 1)]);
    let (view, cx) = setup(cx, fs, Arc::default(), r"C:\m");
    wait_until(cx, &view, "load", loaded(r"C:\m"));
    let new = format!("{} ›", t!("menu.new"));
    let move_to = t!("menu.move_to_new_folder").into_owned();
    let icons = |first: Option<String>| {
        let mut names: Vec<String> = first.into_iter().collect();
        names.extend(
            ["menu.cut", "menu.copy", "menu.rename", "menu.delete"].map(|k| t!(k).into_owned()),
        );
        format!("[{}]", names.join(" "))
    };

    // docs, a folder.
    right_click(&view, cx, Some(0));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(labels.contains(&new), "{labels:?}");
    assert!(
        !labels.contains(&t!("menu.new_folder_in").into_owned()),
        "{labels:?}"
    );
    assert!(labels.contains(&move_to), "{labels:?}");
    assert_eq!(
        labels.last(),
        Some(&icons(Some(t!("menu.new_folder").into_owned())))
    );
    let at = labels.iter().position(|l| *l == new).unwrap();
    highlight(&view, cx, at);
    cx.simulate_keystrokes("right");
    assert_eq!(
        labels_at(&view, cx, 1),
        [
            t!("menu.new_file_in").into_owned(),
            t!("menu.new_folder_in").into_owned()
        ]
    );
    cx.simulate_keystrokes("escape escape");
    assert!(view.update(cx, |v, _| v.context_menu.is_none()));

    // a.txt, a file.
    right_click(&view, cx, Some(1));
    let labels = menu_labels(&view, cx).unwrap();
    assert!(!labels.contains(&new), "{labels:?}");
    assert!(labels.contains(&move_to), "{labels:?}");
    assert_eq!(labels.last(), Some(&icons(None)));
    cx.simulate_keystrokes("escape");

    change(cx, |st| st.menu_hidden = vec!["move_new_folder".into()]);
    for row in [0, 1] {
        right_click(&view, cx, Some(row));
        let labels = menu_labels(&view, cx).unwrap();
        assert!(!labels.contains(&move_to), "{labels:?}");
        cx.simulate_keystrokes("escape");
    }
}
