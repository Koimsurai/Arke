//! Settings > Context menu: Arke's own menu entries in the menu's order, each dragged to reorder
//! the menu (#263) and turned off with its switch, with a live preview of the folder context menu
//! on the right.
//!
//! Core entries (open, cut, copy, rename, delete, paste, new) cannot be hidden.
//! Stores stable keys in `Settings::menu_hidden` and `Settings::menu_order` (`menu_order.rs`);
//! `explorer_menu.rs` applies them.

use std::cell::{Cell, RefCell};

use gpui::{
    AnyElement, App, ClickEvent, Context, DragMoveEvent, Entity, Focusable, Pixels, SharedString,
    Window, canvas, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::menu_order::{self, Slot};
use crate::settings::{Settings, SettingsStore};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

/// Element ids for the remove buttons (caps the custom apps).
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

/// One of Arke's menu entries in the list: dragged to reorder the menu, and with a switch to turn
/// it off unless it is a basic entry.
pub(crate) struct MenuPref {
    /// Key stored in settings (`menu_order::DEFAULT`).
    pub(crate) key: &'static str,
    /// The switch's element id; `None` for basic entries, which are always shown.
    switch_id: Option<&'static str>,
    icon: Icon,
    /// Name and description locale keys.
    label: &'static str,
    note: Option<&'static str>,
}

/// The entries, in the built-in order (`menu_order::DEFAULT`).
pub(crate) const MENU_PREFS: &[MenuPref] = &[
    MenuPref {
        key: "open",
        switch_id: None,
        icon: Icon::Open,
        label: "menu.open",
        note: None,
    },
    MenuPref {
        key: "new_tab",
        switch_id: Some("menu-pref-new_tab"),
        icon: Icon::TabOpen,
        label: "menu.open_in_new_tab",
        note: Some("menupref.folders"),
    },
    MenuPref {
        key: "new",
        switch_id: None,
        icon: Icon::Plus,
        label: "menu.new",
        note: None,
    },
    MenuPref {
        key: "view",
        switch_id: Some("menu-pref-view"),
        icon: Icon::ViewList,
        label: "menupref.view",
        note: Some("menupref.blank"),
    },
    MenuPref {
        key: "search_folder",
        switch_id: Some("menu-pref-search_folder"),
        icon: Icon::Search,
        label: "menu.search_in_folder",
        note: Some("menupref.folders"),
    },
    MenuPref {
        key: "pin",
        switch_id: Some("menu-pref-pin"),
        icon: Icon::Pin,
        label: "menupref.pin",
        note: None,
    },
    MenuPref {
        key: "terminal",
        switch_id: Some("menu-pref-terminal"),
        icon: Icon::Terminal,
        label: "menu.open_terminal",
        note: None,
    },
    MenuPref {
        key: "archive",
        switch_id: Some("menu-pref-archive"),
        icon: Icon::Zip,
        label: "menupref.archive",
        note: None,
    },
    MenuPref {
        key: "set_background",
        switch_id: Some("menu-pref-set_background"),
        icon: Icon::Image,
        label: "menu.set_background",
        note: Some("menupref.images"),
    },
    MenuPref {
        key: "import_settings",
        switch_id: Some("menu-pref-import_settings"),
        icon: Icon::Settings,
        label: "menu.import_settings",
        note: Some("menupref.json"),
    },
    MenuPref {
        key: "move_new_folder",
        switch_id: Some("menu-pref-move_new_folder"),
        icon: Icon::FolderPlus,
        label: "menu.move_to_new_folder",
        note: None,
    },
    MenuPref {
        key: "new_window",
        switch_id: Some("menu-pref-new_window"),
        icon: Icon::External,
        label: "menu.open_new_window",
        note: Some("menupref.folders"),
    },
    MenuPref {
        key: "open_with",
        switch_id: Some("menu-pref-open_with"),
        icon: Icon::OpenWith,
        label: "menupref.open_with",
        note: None,
    },
    MenuPref {
        key: "shelf",
        switch_id: Some("menu-pref-shelf"),
        icon: Icon::Shelf,
        label: "menu.add_to_shelf",
        note: None,
    },
    MenuPref {
        key: "cloud",
        switch_id: Some("menu-pref-cloud"),
        icon: Icon::Cloud,
        label: "menupref.cloud",
        note: Some("menupref.cloud_note"),
    },
    MenuPref {
        key: "copy_path",
        switch_id: Some("menu-pref-copy_path"),
        icon: Icon::CopyPath,
        label: "menu.copy_path",
        note: None,
    },
    MenuPref {
        key: "relative_path",
        switch_id: Some("menu-pref-relative_path"),
        icon: Icon::CopyPath,
        label: "menu.copy_relative_path",
        note: Some("menupref.tree"),
    },
    MenuPref {
        key: "reveal",
        switch_id: Some("menu-pref-reveal"),
        icon: Icon::Explorer,
        label: "menu.reveal_in_explorer",
        note: None,
    },
    MenuPref {
        key: "properties",
        switch_id: Some("menu-pref-properties"),
        icon: Icon::Info,
        label: "menu.properties",
        note: None,
    },
    MenuPref {
        key: "more_options",
        switch_id: Some("menu-pref-more_options"),
        icon: Icon::Menu,
        label: "menu.more_options",
        note: Some("menupref.more_note"),
    },
];

fn pref(key: &str) -> Option<&'static MenuPref> {
    MENU_PREFS.iter().find(|p| p.key == key)
}

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

thread_local! {
    /// The row being dragged: an entry's key, or a pinned command's pin key.
    static DRAGGING: RefCell<Option<SharedString>> = const { RefCell::new(None) };
    /// How wide the list's rows were drawn, for the row that follows the pointer.
    static ROW_W: Cell<f32> = const { Cell::new(320.) };
}

/// A row being dragged to reorder the menu (#263): one of Arke's entries, or a pinned Windows
/// command among the others.
#[derive(Clone)]
struct DraggedMenuRow {
    key: SharedString,
    pinned: bool,
    icon: Icon,
    label: SharedString,
}

/// What follows the pointer: the row, lifted off the list.
struct MenuRowPreview {
    row: DraggedMenuRow,
    width: Pixels,
}

impl Render for MenuRowPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(self.width)
            .min_h(super::settings_panel::card_min_h())
            .px(theme::s_16())
            .py(theme::s_12())
            .flex()
            .items_center()
            .gap(theme::s_16())
            .rounded(theme::r_8())
            .bg(theme::frame_solid())
            .border_1()
            .border_color(theme::line())
            .shadow(theme::elev_float())
            .ui_font()
            .text_size(theme::fs_14())
            .text_color(theme::text())
            .child(super::settings_panel::row_icon(
                self.row.icon,
                theme::text_muted(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.row.label.clone()),
            )
    }
}

/// `row` dragged as `drag`: it takes the place of the rows of its list it is dragged over, saved
/// as it goes, so the preview follows. While dragged, its place in the list is dimmed. A grip in
/// the row's left padding shows on hover, so the rows read as draggable.
fn draggable(
    row: gpui::Stateful<gpui::Div>,
    drag: DraggedMenuRow,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let (key, pinned) = (drag.key.clone(), drag.pinned);
    let lifted = cx.has_active_drag() && DRAGGING.with_borrow(|d| d.as_deref() == Some(&*key));
    let group = SharedString::from(format!("menu-drag-{}-{key}", u8::from(pinned)));
    let grip = div()
        .absolute()
        .left_0()
        .top_0()
        .bottom_0()
        .flex()
        .items_center()
        .debug_selector(|| "menu-row-grip".to_owned())
        .child(
            svg()
                .path(Icon::Grip.path())
                .flex_none()
                .size(theme::icon())
                .text_color(theme::text_muted()),
        )
        .when(!lifted, |d| {
            d.invisible()
                .group_hover(group.clone(), gpui::Styled::visible)
        });
    row.relative()
        .group(group)
        .child(grip)
        .cursor_grab()
        .when(lifted, |d| d.opacity(0.4))
        .on_drag(drag, |row, _, _, cx| {
            DRAGGING.with_borrow_mut(|d| *d = Some(row.key.clone()));
            let width = px(ROW_W.get());
            cx.new(|_| MenuRowPreview {
                row: row.clone(),
                width,
            })
        })
        .on_drag_move(move |e: &DragMoveEvent<DraggedMenuRow>, _, cx| {
            reorder(&key, pinned, e, cx);
        })
}

/// The dragged row is over row `key` of the list: it takes that row's place, once the pointer is
/// past the row's middle in the direction it moves (rows of different heights would otherwise
/// trade places back and forth).
fn reorder(key: &SharedString, pinned: bool, e: &DragMoveEvent<DraggedMenuRow>, cx: &mut App) {
    let dragged = e.drag(cx);
    if dragged.pinned != pinned || dragged.key == *key || !e.bounds.contains(&e.event.position) {
        return;
    }
    let dragged = dragged.key.clone();
    let past = |from: usize, to: usize| {
        let (y, middle) = (e.event.position.y, e.bounds.center().y);
        (to > from && y >= middle) || (to < from && y <= middle)
    };
    let settings = &SettingsStore::global(cx).read(cx).settings;
    if pinned {
        let pins = &settings.menu_pinned;
        let (Some(from), Some(to)) = (
            pins.iter().position(|p| *p == *dragged),
            pins.iter().position(|p| *p == **key),
        ) else {
            return;
        };
        if !past(from, to) {
            return;
        }
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(
                |s| {
                    if from < s.menu_pinned.len() && to < s.menu_pinned.len() {
                        let pin = s.menu_pinned.remove(from);
                        s.menu_pinned.insert(to, pin);
                    }
                },
                cx,
            );
        });
    } else {
        let order = menu_order::resolve(&settings.menu_order);
        let (Some(from), Some(to)) = (
            order.iter().position(|k| *k == &*dragged),
            order.iter().position(|k| *k == &**key),
        ) else {
            return;
        };
        if !past(from, to) {
            return;
        }
        let order = menu_order::stored(&menu_order::moved(&order, from, to));
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(|s| s.menu_order = order, cx);
        });
    }
}

fn pref_row(ix: usize, pref: &'static MenuPref, s: &Settings, cx: &App) -> AnyElement {
    let on = pref.switch_id.is_none() || !hidden(s, pref.key);
    let row = div()
        .id(SharedString::from(format!("menu-row-{}", pref.key)))
        .debug_selector(move || format!("menu-row {}", pref.key))
        .min_h(super::settings_panel::card_min_h())
        .px(theme::s_16())
        .py(theme::s_12())
        .flex()
        .items_center()
        .gap(theme::s_16())
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
                .gap(theme::s_2())
                .child(
                    div()
                        .when(!on, |d| d.text_color(theme::text_muted()))
                        .child(label(pref)),
                )
                .children(pref.note.map(|note| {
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(t!(note).into_owned())
                })),
        )
        // The switch is named after the command it shows or hides (#322).
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::label_scope(label(pref).into()))
        })
        .children(pref.switch_id.map(|id| {
            div()
                .flex_none()
                .child(super::settings_panel::switch(id, on, toggle(pref.key)))
        }))
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::end_label_scope())
        });
    let drag = DraggedMenuRow {
        key: pref.key.into(),
        pinned: false,
        icon: pref.icon,
        label: label(pref).into(),
    };
    draggable(row, drag, cx).into_any_element()
}

/// "Reset": every entry on again, in the built-in order; shown only when something changed.
fn reset_button() -> AnyElement {
    div()
        .id("menu-pref-reset")
        .debug_selector(|| "menu-pref-reset".to_owned())
        .a11y_button(t!("menupref.reset"))
        .h(theme::h_ctl_sm())
        .px(theme::s_12())
        .flex()
        .items_center()
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .hover(|s| s.bg(theme::wash()))
        .child(t!("menupref.reset").to_string())
        .on_click(|_, _, cx| {
            crate::settings::SettingsStore::global(cx).update(cx, |store, cx| {
                store.update(
                    |s| {
                        s.menu_hidden.clear();
                        s.menu_order.clear();
                    },
                    cx,
                );
            });
        })
        .into_any_element()
}

/// Context menu section: description, the entries on the left, preview on the right.
pub(super) fn menu_section(
    s: &Settings,
    app_input: &Entity<TextInput>,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Vec<AnyElement> {
    let app: &App = cx;
    let rows: Vec<AnyElement> = menu_order::resolve(&s.menu_order)
        .into_iter()
        .filter_map(pref)
        .enumerate()
        .map(|(ix, pref)| pref_row(ix, pref, s, app))
        .collect();
    let list = div()
        .relative()
        .flex_1()
        .min_w(crate::theme::scaled_px(300.))
        .flex()
        .flex_col()
        .rounded(theme::r_8())
        .bg(theme::card())
        .border_1()
        .border_color(theme::line())
        .overflow_hidden()
        .child(
            canvas(
                |bounds, _, _| ROW_W.set(f32::from(bounds.size.width)),
                |_, (), _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .children(rows);
    let preview = div()
        .flex_none()
        .w(crate::theme::scaled_px(260.))
        .flex()
        .flex_col()
        .gap(theme::s_8())
        .child(
            div()
                .text_size(theme::fs_12())
                .text_color(theme::text_subtle())
                .child(t!("menupref.preview").to_string()),
        )
        .child(preview_menu(s));
    let changed = !s.menu_hidden.is_empty() || !s.menu_order.is_empty();
    let reset = changed.then(reset_button);
    let pinned = pinned_card(s, cx);
    vec![
        div()
            .flex()
            .items_center()
            .gap(theme::s_12())
            .child(
                div()
                    .flex_1()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(t!("menupref.intro").to_string()),
            )
            .children(reset)
            .into_any_element(),
        div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap(theme::s_16())
            .child(list)
            .child(preview)
            .into_any_element(),
        pinned,
        open_with_card(s, app_input, window, cx),
    ]
}

/// Windows commands pinned to the main menu, each removable, dragged to reorder them. Pinning
/// happens via the pin button under "More options".
fn pinned_card(s: &Settings, cx: &App) -> AnyElement {
    let rows: Vec<AnyElement> = s
        .menu_pinned
        .iter()
        .enumerate()
        .map(|(ix, key)| {
            let remove = key.clone();
            let button = div()
                .id(("menu-pinned-remove", ix))
                .h(theme::h_ctl_sm())
                .px(theme::s_12())
                .flex()
                .items_center()
                .rounded(theme::r_8())
                .border_1()
                .border_color(theme::line_strong())
                .cursor_pointer()
                .text_size(theme::fs_12())
                .hover(|s| s.bg(theme::wash()))
                .child(t!("openwith.remove").to_string())
                .on_click(move |_: &ClickEvent, _, cx: &mut App| {
                    let remove = remove.clone();
                    SettingsStore::global(cx).update(cx, |store, cx| {
                        store.update(|st| st.menu_pinned.retain(|k| *k != remove), cx);
                    });
                });
            let row = super::settings_panel::row(
                Icon::Pin,
                SharedString::from(key.clone()),
                None,
                button.into_any_element(),
            );
            let selector = format!("menu-pinned {key}");
            let drag = DraggedMenuRow {
                key: key.clone().into(),
                pinned: true,
                icon: Icon::Pin,
                label: key.clone().into(),
            };
            draggable(
                div()
                    .id(("menu-pinned-row", ix))
                    .debug_selector(move || selector)
                    .child(row),
                drag,
                cx,
            )
            .into_any_element()
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

/// "Open with" apps: added by exe path, and removed again.
fn open_with_card(
    s: &Settings,
    input: &Entity<TextInput>,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();
    for (ix, exe) in s.open_with_extra.iter().enumerate().take(EXTRA_IDS.len()) {
        let remove = exe.clone();
        let button = div()
            .id(EXTRA_IDS[ix])
            .h(theme::h_ctl_sm())
            .px(theme::s_12())
            .flex()
            .items_center()
            .rounded(theme::r_8())
            .border_1()
            .border_color(theme::line_strong())
            .cursor_pointer()
            .text_size(theme::fs_12())
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
            crate::explorer::app_name(exe),
            Some(exe.display().to_string()),
            button,
        ));
    }
    let focused = input.focus_handle(cx).is_focused(window);
    let field = div()
        .mt(theme::s_4())
        .h(theme::h_ctl())
        .px(theme::s_8())
        .flex()
        .items_center()
        .rounded(theme::r_8())
        .border_1()
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(theme::line())
        })
        .child(input.clone())
        .into_any_element();
    // Last row: paste a path, or browse for the program. The + and the button line up with the
    // text box, not with the label and box together (#281).
    let beside_field = |d: gpui::Div| d.flex_none().h(theme::h_ctl()).flex().items_center();
    rows.push(
        div()
            .min_h(super::settings_panel::card_min_h())
            .px(theme::s_16())
            .py(theme::s_12())
            .flex()
            .items_end()
            .gap(theme::s_16())
            .child(beside_field(div()).child(super::settings_panel::row_icon(
                Icon::Plus,
                theme::text_muted(),
            )))
            .child(super::settings_panel::row_text(
                t!("openwith.add").into_owned().into(),
                Some(field),
            ))
            .child(
                beside_field(div()).child(super::settings_panel::text_button(
                    "openwith-browse",
                    t!("settings.browse"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.browse(super::settings_panel::Browse::OpenWithProgram, window, cx);
                    }),
                )),
            )
            .into_any_element(),
    );
    super::settings_panel::group(Some(t!("openwith.title")), rows)
}

/// One app row in the settings row format: icon, name plus path, control on the right.
fn app_row(name: String, path: Option<String>, control: AnyElement) -> AnyElement {
    let icon = super::settings_panel::row_icon(Icon::OpenWith, theme::text_muted());
    div()
        .min_h(super::settings_panel::card_min_h())
        .px(theme::s_16())
        .py(theme::s_12())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::label_scope(name.clone().into()))
        })
        .child(icon)
        .child(super::settings_panel::row_text(
            name.into(),
            path.map(|p| {
                div()
                    .truncate()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(p)
                    .into_any_element()
            }),
        ))
        .child(div().flex_none().child(control))
        .when(crate::a11y::active(), |d| {
            d.child(crate::a11y::end_label_scope())
        })
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

fn label(pref: &MenuPref) -> String {
    t!(pref.label).into_owned()
}

/// A row of the preview (`None` in a list of them is a separator).
struct PreviewRow {
    /// Its key in settings (`None`: a pinned Windows command).
    key: Option<&'static str>,
    icon: Option<Icon>,
    text: String,
    submenu: bool,
}

const fn preview_row(key: Option<&'static str>, icon: Icon, text: String) -> PreviewRow {
    PreviewRow {
        key,
        icon: Some(icon),
        text,
        submenu: false,
    }
}

/// The folder context menu's rows in their built-in order, as `build_menu` makes them for one
/// folder.
fn preview_rows(s: &Settings) -> Vec<Option<PreviewRow>> {
    // The "Open with" row shows the first app the real menu would list, so the preview matches
    // the menu; the entry's own name until one is added.
    let open_with = crate::explorer::open_with_list(s)
        .into_iter()
        .next()
        .map_or_else(
            || t!("menupref.open_with").into_owned(),
            |app| t!("menu.open_with", name = app.name.as_str()).into_owned(),
        );
    let row = |key: &'static str, icon: Icon, label: &str| {
        Some(preview_row(Some(key), icon, t!(label).into_owned()))
    };
    vec![
        row("open", Icon::Open, "menu.open"),
        row("new_tab", Icon::TabOpen, "menu.open_in_new_tab"),
        None,
        Some(PreviewRow {
            submenu: true,
            ..preview_row(Some("new"), Icon::Plus, t!("menu.new").into_owned())
        }),
        row("search_folder", Icon::Search, "menu.search_in_folder"),
        None,
        row("pin", Icon::Pin, "sidebar.add_favorite"),
        row("terminal", Icon::Terminal, "menu.open_terminal"),
        None,
        Some(preview_row(
            Some("archive"),
            Icon::Zip,
            t!("menu.compress", name = "Photos.zip").into_owned(),
        )),
        row(
            "move_new_folder",
            Icon::FolderPlus,
            "menu.move_to_new_folder",
        ),
        None,
        row("new_window", Icon::External, "menu.open_new_window"),
        Some(preview_row(Some("open_with"), Icon::OpenWith, open_with)),
        row("shelf", Icon::Shelf, "menu.add_to_shelf"),
        row("copy_path", Icon::CopyPath, "menu.copy_path"),
        row("relative_path", Icon::CopyPath, "menu.copy_relative_path"),
        row("reveal", Icon::Explorer, "menu.reveal_in_explorer"),
        row("properties", Icon::Info, "menu.properties"),
        None,
        Some(PreviewRow {
            key: Some("more_options"),
            icon: None,
            text: t!("menu.more_options").into_owned(),
            submenu: true,
        }),
    ]
}

/// Preview of the folder context menu: same order and look as the real one, hidden entries
/// omitted.
fn preview_menu(s: &Settings) -> AnyElement {
    let mut rows = preview_rows(s);
    // In the order dragged to, as `open_context_menu` applies it.
    if let Some(ranks) = menu_order::ranks(&s.menu_order) {
        let slot = |row: &Option<PreviewRow>| match row {
            None => Slot::Separator,
            Some(row) => row.key.map_or(Slot::Follow, Slot::Key),
        };
        rows = menu_order::arrange(rows, ranks, slot, || None);
    }
    let shown = |k: &str| menu_order::CORE.contains(&k) || !hidden(s, k);
    let mut kept: Vec<Option<PreviewRow>> = Vec::new();
    for row in rows {
        if row
            .as_ref()
            .is_some_and(|r| r.key.is_some_and(|k| !shown(k)))
        {
            continue;
        }
        // Pinned Windows commands go right above "More options", showing only the last path
        // segment like the real menu; a whole submenu as one (#88).
        if row.as_ref().is_some_and(|r| r.key == Some("more_options")) {
            for pin in &s.menu_pinned {
                let name = pin.rsplit(" › ").next().unwrap_or(pin).to_string();
                kept.push(Some(PreviewRow {
                    submenu: crate::explorer::pin_is_submenu(pin),
                    ..preview_row(None, Icon::Pin, name)
                }));
            }
        }
        // No leading or repeated separators.
        if row.is_none() && kept.last().is_none_or(Option::is_none) {
            continue;
        }
        kept.push(row);
    }
    while kept.last().is_some_and(Option::is_none) {
        kept.pop();
    }
    div()
        .py(theme::s_4())
        .rounded(theme::r_12())
        .bg(theme::overlay())
        .shadow(theme::elev_2())
        .border_1()
        .border_color(theme::line())
        .children(kept.into_iter().map(preview_item))
        .child(preview_actions())
        .into_any_element()
}

/// One row of the preview, or a separator.
fn preview_item(row: Option<PreviewRow>) -> AnyElement {
    let Some(row) = row else {
        return div()
            .my(theme::s_4())
            .mx(theme::s_8())
            .h(px(1.))
            .bg(theme::line())
            .into_any_element();
    };
    let key = row.key;
    let mark = div()
        .size(theme::icon_sm())
        .flex_none()
        .children(row.icon.map(|i| {
            svg()
                .path(i.path())
                .size(theme::icon_sm())
                .text_color(theme::text_muted())
        }));
    div()
        .when_some(key, |d, key| {
            d.debug_selector(move || format!("menu-preview {key}"))
        })
        .h(crate::theme::scaled_px(30.))
        .mx(theme::s_4())
        .px(theme::s_8())
        .flex()
        .items_center()
        .gap(theme::s_8())
        .rounded(theme::r_4())
        .text_size(theme::fs_12())
        .child(mark)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SharedString::from(row.text)),
        )
        .when(row.submenu, |d| {
            d.child(
                svg()
                    .path(Icon::ChevronRight.path())
                    .size(theme::icon_sm())
                    .text_color(theme::text_subtle()),
            )
        })
        .into_any_element()
}

/// The icon button row at the bottom (new folder, cut, copy, rename, delete), always shown.
fn preview_actions() -> AnyElement {
    let icons = [
        Icon::FolderPlus,
        Icon::Cut,
        Icon::Copy,
        Icon::Rename,
        Icon::Trash,
    ];
    div()
        .mt(theme::s_4())
        .mx(theme::s_4())
        .pt(theme::s_4())
        .border_t_1()
        .border_color(theme::line())
        .flex()
        .justify_around()
        .children(icons.map(|i| {
            div()
                .size(crate::theme::scaled_px(28.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    svg()
                        .path(i.path())
                        .size(theme::icon_sm())
                        .text_color(theme::text_muted()),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use crate::menu_order::{CORE, DEFAULT};

    /// The list has a row for every entry the order knows, in the built-in order; basic entries
    /// have no switch.
    #[test]
    fn every_entry_has_its_row() {
        let keys: Vec<&str> = super::MENU_PREFS.iter().map(|p| p.key).collect();
        assert_eq!(keys, DEFAULT);
        for pref in super::MENU_PREFS {
            assert_eq!(
                pref.switch_id.is_none(),
                CORE.contains(&pref.key),
                "{}",
                pref.key
            );
        }
    }
}
