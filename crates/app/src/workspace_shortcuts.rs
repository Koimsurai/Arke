//! Settings > Keyboard shortcuts: every command with its keys. Click a key to record a new one,
//! Esc cancels. A key another command has is shown with a choice to swap the two.
//!
//! Only changes are stored (`Settings::shortcuts`); `commands.rs` rebinds the keys when they
//! change, so a new shortcut works at once.

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable, Keystroke, SharedString, Subscription,
    Window, div, prelude::*,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::commands::{self, COMMANDS, Command, Overrides};
use crate::settings::SettingsStore;
use crate::text_input::{InputEvent, TextInput};
use crate::theme;

/// The page's own state: the search box and the key being recorded.
pub(super) struct Page {
    search: Entity<TextInput>,
    _search: Subscription,
    recording: Option<Recording>,
}

struct Recording {
    /// The command getting a new key.
    id: &'static str,
    /// The key pressed, and the command that has it.
    conflict: Option<(String, &'static str)>,
    /// Takes every key press while recording (it is dropped when recording ends).
    _keys: Subscription,
}

impl Page {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let search = cx.new(|cx| TextInput::new(t!("shortcuts.search").to_string(), cx));
        let subscription = cx.subscribe_in(&search, window, |_, _, event, _, cx| {
            if matches!(event, InputEvent::Changed) {
                cx.notify();
            }
        });
        Self {
            search,
            _search: subscription,
            recording: None,
        }
    }
}

impl Workspace {
    /// Starts recording a new key for `id`, instead of any recording before.
    pub(super) fn record_shortcut(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(page) = &mut self.settings_panel else {
            return;
        };
        let this = cx.entity().downgrade();
        let keys = cx.intercept_keystrokes(move |event, _, cx| {
            cx.stop_propagation();
            let key = event.keystroke.clone();
            this.update(cx, |this, cx| this.shortcut_pressed(&key, cx))
                .ok();
        });
        page.shortcuts.recording = Some(Recording {
            id,
            conflict: None,
            _keys: keys,
        });
        cx.notify();
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = &mut self.settings_panel {
            page.shortcuts.recording = None;
        }
        cx.notify();
    }

    /// A key pressed while recording: Esc gives up, a key that cannot be a shortcut is ignored,
    /// one that is free is taken, one that another command has waits for a choice.
    fn shortcut_pressed(&mut self, key: &Keystroke, cx: &mut Context<Self>) {
        let Some(recording) = self
            .settings_panel
            .as_mut()
            .and_then(|p| p.shortcuts.recording.as_mut())
        else {
            return;
        };
        if key.key == "escape" && !key.modifiers.modified() {
            self.stop_recording(cx);
            return;
        }
        if !commands::assignable(key) {
            return;
        }
        let Some(chord) = commands::chord_of(key) else {
            return;
        };
        let id = recording.id;
        let overrides = SettingsStore::global(cx)
            .read(cx)
            .settings
            .shortcuts
            .clone();
        if let Some(other) = commands::holder(&chord, id, &overrides) {
            recording.conflict = Some((chord, other.id));
            cx.notify();
            return;
        }
        self.stop_recording(cx);
        Self::assign_shortcut(id, chord, false, cx);
    }

    fn assign_shortcut(id: &'static str, chord: String, swap: bool, cx: &mut App) {
        SettingsStore::global(cx).update(cx, |store, cx| {
            store.update(|s| commands::assign(&mut s.shortcuts, id, &chord, swap), cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn shortcuts_search_for_test(&self) -> Option<Entity<TextInput>> {
        self.settings_panel
            .as_ref()
            .map(|p| p.shortcuts.search.clone())
    }

    #[cfg(test)]
    pub(crate) fn shortcut_recording_for_test(&self) -> Option<(&'static str, bool)> {
        let recording = self.settings_panel.as_ref()?.shortcuts.recording.as_ref()?;
        Some((recording.id, recording.conflict.is_some()))
    }
}

fn button(
    id: impl Into<gpui::ElementId>,
    selector: String,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .debug_selector(move || selector)
        .a11y_button(label.clone())
        .h(theme::h_ctl_sm())
        .px(theme::s_12())
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .hover(|s| s.bg(theme::wash()))
        .child(label)
        .on_click(on_click)
}

/// The category: search box with "Reset all", then one row per command.
pub(super) fn section(
    overrides: &Overrides,
    page: &Page,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Vec<AnyElement> {
    let query = page.search.read(cx).committed_text();
    let shown: Vec<(usize, &'static Command)> = {
        let all: Vec<(usize, &'static Command)> = COMMANDS.iter().enumerate().collect();
        let texts: Vec<String> = all
            .iter()
            .map(|(_, c)| {
                let key = commands::shown_key(c, overrides).unwrap_or_default();
                format!("{} {key}", c.label())
            })
            .collect();
        arke_core::filter::rank_paths(&texts, &query)
            .into_iter()
            .map(|m| all[m.index])
            .collect()
    };

    let focused = page.search.focus_handle(cx).is_focused(window);
    let field = div()
        .flex_1()
        .min_w_0()
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
        .child(page.search.clone());
    let reset_all = (!overrides.is_empty()).then(|| {
        let selector = "shortcuts-reset-all".to_owned();
        button(
            "shortcuts-reset-all",
            selector,
            t!("shortcuts.reset_all"),
            |_, _, cx| {
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(|s| s.shortcuts.clear(), cx);
                });
            },
        )
    });
    let bar = div()
        .flex()
        .items_center()
        .gap(theme::s_12())
        .child(field)
        .children(reset_all)
        .into_any_element();

    let recording = page.recording.as_ref();
    let rows: Vec<AnyElement> = shown
        .iter()
        .enumerate()
        .map(|(row, (ix, command))| command_row(row, *ix, command, overrides, recording, cx))
        .collect();
    let list = div()
        .flex()
        .flex_col()
        .rounded(theme::r_8())
        .bg(theme::card())
        .border_1()
        .border_color(theme::line())
        .overflow_hidden()
        .when(rows.is_empty(), |d| {
            d.child(
                div()
                    .h(theme::s_32() + theme::s_12())
                    .px(theme::s_16())
                    .flex()
                    .items_center()
                    .text_color(theme::text_subtle())
                    .child(t!("palette.no_match").to_string()),
            )
        })
        .children(rows)
        .into_any_element();
    vec![bar, list]
}

#[allow(
    clippy::too_many_lines,
    reason = "one settings row: name, note, key, and the buttons that go with it"
)]
fn command_row(
    row: usize,
    ix: usize,
    command: &'static Command,
    overrides: &Overrides,
    recording: Option<&Recording>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let id = command.id;
    let mine = recording.filter(|r| r.id == id);
    let changed = overrides.contains_key(id);
    let keys = commands::keys_of(command, overrides);

    let note = mine
        .and_then(|r| r.conflict.as_ref())
        .map(|(chord, other)| {
            let other = commands::find(other).map_or_default(Command::label);
            t!(
                "shortcuts.taken",
                key = commands::display(chord),
                command = other
            )
            .into_owned()
        });

    let chip = |text: String| {
        div()
            .px(theme::s_6())
            .rounded(theme::r_4())
            .border_1()
            .border_color(theme::line_strong())
            .text_size(theme::fs_12())
            .child(text)
    };
    let key_button = div()
        .id(("shortcut-key", ix))
        .debug_selector(move || format!("shortcut-key-{id}"))
        // The command and its keys, or that new keys are being waited for (#322).
        .a11y_lazy(gpui::Role::Button, || {
            let keys = if mine.is_some() {
                t!("shortcuts.press").into_owned()
            } else if keys.is_empty() {
                t!("shortcuts.none").into_owned()
            } else {
                keys.iter()
                    .map(|k| commands::display(k))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("{}, {keys}", command.label())
        })
        .min_h(theme::h_ctl_sm())
        .px(theme::s_8())
        .flex()
        .flex_none()
        .items_center()
        .gap(theme::s_6())
        .rounded(theme::r_8())
        .border_1()
        .cursor_pointer()
        .text_size(theme::fs_12())
        .when(mine.is_some(), |d| {
            d.border_color(theme::signal_line())
                .bg(theme::signal_wash())
        })
        .when(mine.is_none(), |d| {
            d.border_color(theme::line()).hover(|s| s.bg(theme::wash()))
        })
        .when(mine.is_some(), |d| {
            d.child(t!("shortcuts.press").to_string())
        })
        .when(mine.is_none(), |d| {
            if keys.is_empty() {
                d.text_color(theme::text_subtle())
                    .child(t!("shortcuts.none").to_string())
            } else {
                d.children(keys.iter().map(|k| chip(commands::display(k))))
            }
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.record_shortcut(id, cx);
        }));

    let conflict = mine.and_then(|r| r.conflict.clone());
    let choices = conflict.map(|(chord, _)| {
        div()
            .flex()
            .flex_none()
            .gap(theme::s_8())
            .child(button(
                ("shortcut-swap", ix),
                format!("shortcut-swap-{id}"),
                t!("shortcuts.swap"),
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.stop_recording(cx);
                    Workspace::assign_shortcut(id, chord.clone(), true, cx);
                }),
            ))
            .child(button(
                ("shortcut-cancel", ix),
                format!("shortcut-cancel-{id}"),
                t!("shortcuts.cancel"),
                cx.listener(|this, _: &ClickEvent, _, cx| this.stop_recording(cx)),
            ))
    });
    let reset = (changed && mine.is_none()).then(|| {
        let selector = format!("shortcut-reset-{id}");
        button(
            ("shortcut-reset", ix),
            selector,
            t!("shortcuts.reset"),
            move |_, _, cx| {
                SettingsStore::global(cx).update(cx, |store, cx| {
                    store.update(
                        |s| {
                            s.shortcuts.remove(id);
                        },
                        cx,
                    );
                });
            },
        )
    });

    div()
        .min_h(theme::s_32() + theme::s_16())
        .px(theme::s_16())
        .py(theme::s_8())
        .flex()
        .items_center()
        .gap(theme::s_12())
        .when(row > 0, |d| d.border_t_1().border_color(theme::line()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::s_2())
                .child(div().child(command.label()))
                .children(note.map(|note| {
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(note)
                })),
        )
        .children(choices)
        .children(reset)
        .child(key_button)
        .into_any_element()
}
