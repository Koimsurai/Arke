//! Settings > Context menu: Arke's own menu entries can be turned off one by one, with a live
//! preview of the folder context menu on the right.
//!
//! Core entries (open, cut, copy, rename, delete, paste, new) cannot be hidden.
//! Stores stable keys in `Settings::menu_hidden`; `explorer_menu.rs` filters by them.

use std::sync::Arc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable, RenderImage, SharedString, Window,
    div, img, prelude::*, px, svg,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::settings::{Settings, SettingsStore};
use crate::text_input::{InputEvent, TextInput};
use crate::theme;

/// Element ids for the switches and remove buttons (caps the detected editors and custom apps).
const FOUND_IDS: [&str; 8] = [
    "ow-found-0",
    "ow-found-1",
    "ow-found-2",
    "ow-found-3",
    "ow-found-4",
    "ow-found-5",
    "ow-found-6",
    "ow-found-7",
];
const EXTRA_IDS: [&str; 12] = [
    "ow-extra-0",
    "ow-extra-1",
    "ow-extra-2",
    "ow-extra-3",
    "ow-extra-4",
    "ow-extra-5",
    "ow-extra-6",
    "ow-extra-7",
    "ow-extra-8",
    "ow-extra-9",
    "ow-extra-10",
    "ow-extra-11",
];

/// A menu entry that can be hidden.
pub(crate) struct MenuPref {
    /// Key stored in settings.
    pub(crate) key: &'static str,
    switch_id: &'static str,
    icon: Icon,
    /// Name and description locale keys.
    label: &'static str,
    note: Option<&'static str>,
}

pub(crate) const MENU_PREFS: &[MenuPref] = &[
    MenuPref {
        key: "new_tab",
        switch_id: "menu-pref-new_tab",
        icon: Icon::TabOpen,
        label: "menu.open_in_new_tab",
        note: Some("menupref.folders"),
    },
    MenuPref {
        key: "new_window",
        switch_id: "menu-pref-new_window",
        icon: Icon::External,
        label: "menu.open_new_window",
        note: Some("menupref.folders"),
    },
    MenuPref {
        key: "search_folder",
        switch_id: "menu-pref-search_folder",
        icon: Icon::Search,
        label: "menu.search_in_folder",
        note: Some("menupref.folders"),
    },
    MenuPref {
        key: "pin",
        switch_id: "menu-pref-pin",
        icon: Icon::Pin,
        label: "menupref.pin",
        note: None,
    },
    MenuPref {
        key: "terminal",
        switch_id: "menu-pref-terminal",
        icon: Icon::Terminal,
        label: "menu.open_terminal",
        note: None,
    },
    MenuPref {
        key: "archive",
        switch_id: "menu-pref-archive",
        icon: Icon::Zip,
        label: "menupref.archive",
        note: None,
    },
    MenuPref {
        key: "open_with",
        switch_id: "menu-pref-open_with",
        icon: Icon::OpenWith,
        label: "menupref.open_with",
        note: Some("menupref.open_with_note"),
    },
    MenuPref {
        key: "cloud",
        switch_id: "menu-pref-cloud",
        icon: Icon::Cloud,
        label: "menupref.cloud",
        note: Some("menupref.cloud_note"),
    },
    MenuPref {
        key: "shelf",
        switch_id: "menu-pref-shelf",
        icon: Icon::Shelf,
        label: "menu.add_to_shelf",
        note: None,
    },
    MenuPref {
        key: "copy_path",
        switch_id: "menu-pref-copy_path",
        icon: Icon::CopyPath,
        label: "menu.copy_path",
        note: None,
    },
    MenuPref {
        key: "relative_path",
        switch_id: "menu-pref-relative_path",
        icon: Icon::CopyPath,
        label: "menu.copy_relative_path",
        note: Some("menupref.tree"),
    },
    MenuPref {
        key: "reveal",
        switch_id: "menu-pref-reveal",
        icon: Icon::Explorer,
        label: "menu.reveal_in_explorer",
        note: None,
    },
    MenuPref {
        key: "properties",
        switch_id: "menu-pref-properties",
        icon: Icon::Info,
        label: "menu.properties",
        note: None,
    },
    MenuPref {
        key: "set_background",
        switch_id: "menu-pref-set_background",
        icon: Icon::Image,
        label: "menu.set_background",
        note: Some("menupref.images"),
    },
    MenuPref {
        key: "import_settings",
        switch_id: "menu-pref-import_settings",
        icon: Icon::Settings,
        label: "menu.import_settings",
        note: Some("menupref.json"),
    },
    MenuPref {
        key: "view",
        switch_id: "menu-pref-view",
        icon: Icon::ViewList,
        label: "menupref.view",
        note: Some("menupref.blank"),
    },
    MenuPref {
        key: "more_options",
        switch_id: "menu-pref-more_options",
        icon: Icon::Menu,
        label: "menu.more_options",
        note: Some("menupref.more_note"),
    },
];

fn hidden(s: &Settings, key: &str) -> bool {
    s.menu_hidden.iter().any(|k| k == key)
}

/// Switch handler: off adds the key to `menu_hidden`, on removes it.
fn toggle(key: &'static str) -> impl Fn(&mut Settings) + 'static {
    move |s: &mut Settings| {
        if let Some(at) = s.menu_hidden.iter().position(|k| k == key) {
            s.menu_hidden.remove(at);
        } else {
            s.menu_hidden.push(key.to_string());
        }
    }
}

fn pref_row(ix: usize, pref: &'static MenuPref, s: &Settings) -> AnyElement {
    let on = !hidden(s, pref.key);
    div()
        .min_h(super::settings_panel::CARD_MIN_H)
        .px(theme::S_16)
        .py(theme::S_12)
        .flex()
        .items_center()
        .gap(theme::S_16)
        .when(ix > 0, |d| d.border_t_1().border_color(theme::line()))
        .child(super::settings_panel::row_icon(
            pref.icon,
            if on {
                theme::text_muted()
            } else {
                theme::text_disabled()
            },
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::S_2)
                .child(
                    div()
                        .when(!on, |d| d.text_color(theme::text_muted()))
                        .child(label(pref)),
                )
                .children(pref.note.map(|note| {
                    div()
                        .text_size(theme::FS_12)
                        .text_color(theme::text_subtle())
                        .child(t!(note).into_owned())
                })),
        )
        .child(div().flex_none().child(super::settings_panel::switch(
            pref.switch_id,
            on,
            toggle(pref.key),
        )))
        .into_any_element()
}

/// "Restore all", shown only when something is hidden.
fn reset_button() -> AnyElement {
    div()
        .id("menu-pref-reset")
        .a11y_button(t!("menupref.reset"))
        .h(theme::H_CTL_SM)
        .px(theme::S_12)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::FS_12)
        .hover(|s| s.bg(theme::wash()))
        .child(t!("menupref.reset").to_string())
        .on_click(|_, _, cx| {
            crate::settings::SettingsStore::global(cx)
                .update(cx, |store, cx| store.update(|s| s.menu_hidden.clear(), cx));
        })
        .into_any_element()
}

/// Context menu section: description, switches on the left, preview on the right.
pub(super) fn menu_section(
    s: &Settings,
    app_input: &Entity<TextInput>,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Vec<AnyElement> {
    let rows: Vec<AnyElement> = MENU_PREFS
        .iter()
        .enumerate()
        .map(|(ix, pref)| pref_row(ix, pref, s))
        .collect();
    let list = div()
        .flex_1()
        .min_w(px(300.))
        .flex()
        .flex_col()
        .rounded(theme::R_8)
        .bg(theme::card())
        .border_1()
        .border_color(theme::line())
        .overflow_hidden()
        .children(rows);
    let preview = div()
        .flex_none()
        .w(px(260.))
        .flex()
        .flex_col()
        .gap(theme::S_8)
        .child(
            div()
                .text_size(theme::FS_12)
                .text_color(theme::text_subtle())
                .child(t!("menupref.preview").to_string()),
        )
        .child(preview_menu(s));
    let reset = (!s.menu_hidden.is_empty()).then(reset_button);
    vec![
        div()
            .flex()
            .items_center()
            .gap(theme::S_12)
            .child(
                div()
                    .flex_1()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(t!("menupref.intro").to_string()),
            )
            .children(reset)
            .into_any_element(),
        div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap(theme::S_16)
            .child(list)
            .child(preview)
            .into_any_element(),
        pinned_card(s),
        open_with_card(s, app_input, window, cx),
    ]
}

/// Windows commands pinned to the main menu, each removable. Pinning happens via the pin
/// button under "More options".
fn pinned_card(s: &Settings) -> AnyElement {
    let rows: Vec<AnyElement> = s
        .menu_pinned
        .iter()
        .enumerate()
        .map(|(ix, key)| {
            let remove = key.clone();
            let button = div()
                .id(("menu-pinned-remove", ix))
                .h(theme::H_CTL_SM)
                .px(theme::S_12)
                .flex()
                .items_center()
                .rounded(theme::R_8)
                .border_1()
                .border_color(theme::line_strong())
                .cursor_pointer()
                .text_size(theme::FS_12)
                .hover(|s| s.bg(theme::wash()))
                .child(t!("openwith.remove").to_string())
                .on_click(move |_: &ClickEvent, _, cx: &mut App| {
                    let remove = remove.clone();
                    SettingsStore::global(cx).update(cx, |store, cx| {
                        store.update(|st| st.menu_pinned.retain(|k| *k != remove), cx);
                    });
                });
            super::settings_panel::row(
                Icon::Pin,
                SharedString::from(key.clone()),
                None,
                button.into_any_element(),
            )
        })
        .collect();
    let rows = if rows.is_empty() {
        vec![super::settings_panel::row(
            Icon::Pin,
            t!("menupref.pinned_empty").into_owned().into(),
            None,
            div().into_any_element(),
        )]
    } else {
        rows
    };
    super::settings_panel::group_with(
        Some(t!("menupref.pinned_title")),
        Some(t!("menupref.pinned_desc")),
        rows,
    )
}

/// "Open with" apps: detected editors can be turned off, and custom apps added by exe path.
fn open_with_card(
    s: &Settings,
    input: &Entity<TextInput>,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let found = crate::explorer::menu_open_with_found();
    let mut rows: Vec<AnyElement> = Vec::new();
    for (ix, (name, exe, icon)) in found.into_iter().enumerate().take(FOUND_IDS.len()) {
        let on = !s.open_with_hidden.contains(&exe);
        let toggle = move |st: &mut Settings| {
            if let Some(at) = st.open_with_hidden.iter().position(|p| *p == exe) {
                st.open_with_hidden.remove(at);
            } else {
                st.open_with_hidden.push(exe.clone());
            }
        };
        rows.push(app_row(
            icon,
            name,
            None,
            super::settings_panel::switch(FOUND_IDS[ix], on, toggle),
        ));
    }
    for (ix, exe) in s.open_with_extra.iter().enumerate().take(EXTRA_IDS.len()) {
        let remove = exe.clone();
        let button = div()
            .id(EXTRA_IDS[ix])
            .h(theme::H_CTL_SM)
            .px(theme::S_12)
            .flex()
            .items_center()
            .rounded(theme::R_8)
            .border_1()
            .border_color(theme::line_strong())
            .cursor_pointer()
            .text_size(theme::FS_12)
            .hover(|s| s.bg(theme::wash()))
            .child(t!("openwith.remove").to_string())
            .on_click(move |_: &ClickEvent, _, cx: &mut App| {
                let remove = remove.clone();
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(|st| st.open_with_extra.retain(|p| *p != remove), cx);
                });
            })
            .into_any_element();
        rows.push(app_row(
            None,
            crate::explorer::app_name(exe),
            Some(exe.display().to_string()),
            button,
        ));
    }
    let focused = input.focus_handle(cx).is_focused(window);
    let field = div()
        .mt(theme::S_4)
        .h(theme::H_CTL)
        .px(theme::S_8)
        .flex()
        .items_center()
        .rounded(theme::R_8)
        .border_1()
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(theme::line())
        })
        .child(input.clone())
        .into_any_element();
    // Last row: paste a path, or browse for the program.
    rows.push(
        div()
            .min_h(super::settings_panel::CARD_MIN_H)
            .px(theme::S_16)
            .py(theme::S_12)
            .flex()
            .items_center()
            .gap(theme::S_16)
            .child(super::settings_panel::row_icon(
                Icon::Plus,
                theme::text_muted(),
            ))
            .child(super::settings_panel::row_text(
                t!("openwith.add").into_owned().into(),
                Some(field),
            ))
            .child(div().flex_none().child(super::settings_panel::text_button(
                "openwith-browse",
                t!("settings.browse"),
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.browse(super::settings_panel::Browse::OpenWithProgram, window, cx);
                }),
            )))
            .into_any_element(),
    );
    super::settings_panel::group_with(Some(t!("openwith.title")), Some(t!("openwith.desc")), rows)
}

/// One app row in the settings row format: the app's own icon (or a generic one), name plus path
/// for custom apps, control on the right.
fn app_row(
    icon: Option<std::sync::Arc<gpui::RenderImage>>,
    name: String,
    path: Option<String>,
    control: AnyElement,
) -> AnyElement {
    let icon = match icon {
        Some(image) => div()
            .w(super::settings_panel::ICON_COL)
            .flex_none()
            .flex()
            .justify_center()
            .child(img(image).size(theme::ICON).flex_none()),
        None => super::settings_panel::row_icon(Icon::OpenWith, theme::text_muted()),
    };
    div()
        .min_h(super::settings_panel::CARD_MIN_H)
        .px(theme::S_16)
        .py(theme::S_12)
        .flex()
        .items_center()
        .gap(theme::S_16)
        .child(icon)
        .child(super::settings_panel::row_text(
            name.into(),
            path.map(|p| {
                div()
                    .truncate()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_subtle())
                    .child(p)
                    .into_any_element()
            }),
        ))
        .child(div().flex_none().child(control))
        .into_any_element()
}

impl Workspace {
    /// New app path field. Enter adds it (quotes from Explorer's "Copy as path" are stripped), Esc cancels.
    pub(super) fn on_app_input(
        &mut self,
        input: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *event == InputEvent::Changed {
            return;
        }
        if *event == InputEvent::Confirm {
            let text = input.read(cx).committed_text();
            let text = text.trim().trim_matches('"').trim().to_string();
            if !text.is_empty() {
                let exe = std::path::PathBuf::from(text);
                input.update(cx, |input, cx| input.set_text("", cx));
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(
                        |st| {
                            if !st.open_with_extra.contains(&exe) {
                                st.open_with_extra.push(exe);
                            }
                        },
                        cx,
                    );
                });
            }
        }
        if let Some(page) = &self.settings_panel {
            window.focus(&page.focus, cx);
        }
    }
}

/// Icon of a preview row: the theme icon, or an app's own image drawn instead of it.
type PreviewIcon = (Option<Icon>, Option<Arc<RenderImage>>);

fn label(pref: &MenuPref) -> String {
    t!(pref.label).into_owned()
}

/// Preview of the folder context menu: same order and look as the real one, hidden entries omitted.
// Rows mirror the real menu order in one place; splitting them up makes that harder to compare.
#[allow(clippy::too_many_lines)]
fn preview_menu(s: &Settings) -> AnyElement {
    // The "Open with" row shows the first editor the real menu would list, with its own icon, so
    // the preview matches the menu; "VS Code" with the generic icon when none is found.
    let editor = crate::explorer::open_with_list(s).into_iter().next();
    let editor_name = editor.as_ref().map_or("VS Code", |app| app.name.as_str());
    let editor_icon = editor.as_ref().and_then(|app| app.icon.clone());
    // (key, icon, label). Rows with key `None` are always shown; an empty label is a separator.
    let rows: [(Option<&str>, Option<Icon>, String); 21] = [
        (None, Some(Icon::Open), t!("menu.open").into_owned()),
        (
            Some("new_tab"),
            Some(Icon::TabOpen),
            t!("menu.open_in_new_tab").into_owned(),
        ),
        (None, None, String::new()),
        (None, Some(Icon::Plus), t!("menu.new_file_in").into_owned()),
        (
            None,
            Some(Icon::FolderPlus),
            t!("menu.new_folder_in").into_owned(),
        ),
        (
            Some("search_folder"),
            Some(Icon::Search),
            t!("menu.search_in_folder").into_owned(),
        ),
        (None, None, String::new()),
        (Some("pin"), Some(Icon::Pin), t!("sidebar.pin").into_owned()),
        (
            Some("terminal"),
            Some(Icon::Terminal),
            t!("menu.open_terminal").into_owned(),
        ),
        (None, None, String::new()),
        (
            Some("archive"),
            Some(Icon::Zip),
            t!("menu.compress", name = "Photos.zip").into_owned(),
        ),
        (None, None, String::new()),
        (
            Some("new_window"),
            Some(Icon::External),
            t!("menu.open_new_window").into_owned(),
        ),
        (
            Some("open_with"),
            Some(Icon::OpenWith),
            t!("menu.open_with", name = editor_name).into_owned(),
        ),
        (
            Some("copy_path"),
            Some(Icon::CopyPath),
            t!("menu.copy_path").into_owned(),
        ),
        (
            Some("relative_path"),
            Some(Icon::CopyPath),
            t!("menu.copy_relative_path").into_owned(),
        ),
        (
            Some("reveal"),
            Some(Icon::Explorer),
            t!("menu.reveal_in_explorer").into_owned(),
        ),
        (
            Some("properties"),
            Some(Icon::Info),
            t!("menu.properties").into_owned(),
        ),
        (None, None, String::new()),
        (
            Some("more_options"),
            None,
            t!("menu.more_options").into_owned(),
        ),
        (None, None, String::new()),
    ];
    let mut kept: Vec<(PreviewIcon, String, bool)> = Vec::new();
    for (key, icon, text) in rows {
        if key.is_some_and(|k| hidden(s, k)) {
            continue;
        }
        // Pinned Windows commands go right above "More options", showing only the last path segment like the real menu.
        if key == Some("more_options") {
            for pin in &s.menu_pinned {
                let name = pin.rsplit(" › ").next().unwrap_or(pin).to_string();
                kept.push(((Some(Icon::Pin), None), name, false));
            }
        }
        let separator = text.is_empty();
        // No leading or repeated separators.
        if separator && kept.last().is_none_or(|(_, _, sep)| *sep) {
            continue;
        }
        let image = (key == Some("open_with"))
            .then(|| editor_icon.clone())
            .flatten();
        kept.push(((icon, image), text, separator));
    }
    let more = !hidden(s, "more_options");
    let items = kept.into_iter().map(|((icon, image), text, separator)| {
        if separator {
            return div()
                .my(theme::S_4)
                .mx(theme::S_8)
                .h(px(1.))
                .bg(theme::line())
                .into_any_element();
        }
        let is_more = more && text == t!("menu.more_options");
        div()
            .h(px(30.))
            .mx(theme::S_4)
            .px(theme::S_8)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .rounded(theme::R_4)
            .text_size(theme::FS_12)
            .child(div().size(theme::ICON_SM).flex_none().map(|d| match image {
                Some(image) => d.child(img(image).size(theme::ICON_SM)),
                None => d.children(icon.map(|i| {
                    svg()
                        .path(i.path())
                        .size(theme::ICON_SM)
                        .text_color(theme::text_muted())
                })),
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(text)),
            )
            .when(is_more, |d| {
                d.child(
                    svg()
                        .path(Icon::ChevronRight.path())
                        .size(theme::ICON_SM)
                        .text_color(theme::text_subtle()),
                )
            })
            .into_any_element()
    });
    // The icon button row at the bottom (cut, copy, rename, delete) is always shown.
    let actions = div()
        .mt(theme::S_4)
        .mx(theme::S_4)
        .pt(theme::S_4)
        .border_t_1()
        .border_color(theme::line())
        .flex()
        .justify_around()
        .children([Icon::Cut, Icon::Copy, Icon::Rename, Icon::Trash].map(|i| {
            div()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    svg()
                        .path(i.path())
                        .size(theme::ICON_SM)
                        .text_color(theme::text_muted()),
                )
        }));
    div()
        .py(theme::S_4)
        .rounded(theme::R_12)
        .bg(theme::overlay())
        .shadow(theme::elev_2())
        .border_1()
        .border_color(theme::line())
        .children(items)
        .child(actions)
        .into_any_element()
}
