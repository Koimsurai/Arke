//! Screen reader support: roles, names and states for the accessibility tree.
//!
//! GPUI builds an AccessKit tree (UI Automation on Windows) from elements that have both an `id`
//! and a `role`. The tree is only built while an assistive technology (Narrator, NVDA, an UIA
//! client) is connected: the adapter turns on at the first `WM_GETOBJECT` and GPUI redraws then.
//! Setting a role is a plain field store, so roles are always set; names that need formatting
//! (a file row's "name, type, size, date") are only computed while [`active`], which [`sync`]
//! records at the start of each window's render. With no client attached this module costs a few
//! field stores per element and nothing else.
//!
//! The helpers are on [`A11yExt`] so call sites stay one line. Only one of the helpers that edit
//! the node directly ([`A11yExt::a11y_node`] and the ones built on it) may be used per element:
//! GPUI keeps a single callback for them.
//!
//! Settings rows name their control through a label scope: [`label_scope`] marks the row's title
//! for the controls drawn after it (a switch, a dropdown), so those don't need the title passed in.

use std::cell::{Cell, RefCell};

use gpui::accesskit::{Live, Node, SortDirection};
use gpui::{
    IntoElement, Role, SharedString, StatefulInteractiveElement, Styled as _, Toggled, Window,
    canvas,
};

thread_local! {
    /// Whether this frame builds an accessibility tree (see [`sync`]).
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Title of the settings row being prepainted, for controls inside it (see [`label_scope`]).
    static SCOPE_LABEL: RefCell<Option<SharedString>> = const { RefCell::new(None) };
}

/// Records whether `window` is building an accessibility tree this frame. Call first thing in the
/// root view's render; the views it contains render afterwards in the same frame.
pub(crate) fn sync(window: &Window) {
    ACTIVE.set(window.is_a11y_active());
    SCOPE_LABEL.set(None);
}

/// Whether a screen reader or another UIA client is attached (as of this frame).
pub(crate) fn active() -> bool {
    ACTIVE.get()
}

#[cfg(test)]
pub(crate) fn set_active_for_test(on: bool) {
    ACTIVE.set(on);
}

/// Accessible name of a file list row: the name, then type, size and date when known, the way
/// Explorer reads its Details view.
pub(crate) fn item_label<S: AsRef<str>>(name: &str, details: &[S]) -> String {
    let mut label = name.to_owned();
    for part in details
        .iter()
        .map(|p| p.as_ref().trim())
        .filter(|p| !p.is_empty())
    {
        label.push_str(", ");
        label.push_str(part);
    }
    label
}

/// Invisible element that makes `label` the name of the unnamed controls drawn after it, up to
/// [`end_label_scope`]. Settings rows put it before their title and control.
pub(crate) fn label_scope(label: SharedString) -> impl IntoElement {
    canvas(
        move |_, _, _| {
            if active() {
                SCOPE_LABEL.set(Some(label));
            }
        },
        |_, (), _, _| {},
    )
    // Out of the flow, so it adds no gap to the row.
    .absolute()
}

/// Ends a [`label_scope`].
pub(crate) fn end_label_scope() -> impl IntoElement {
    canvas(|_, _, _| SCOPE_LABEL.set(None), |_, (), _, _| {}).absolute()
}

/// The current [`label_scope`] label, if any (read while prepainting).
pub(crate) fn scope_label() -> Option<SharedString> {
    SCOPE_LABEL.with_borrow(Clone::clone)
}

/// Accessibility builders for stateful elements (anything with an `id`).
pub(crate) trait A11yExt: StatefulInteractiveElement + Sized {
    /// Role and name. The name is only stored while a client is attached.
    #[must_use]
    fn a11y(self, role: Role, label: impl Into<SharedString>) -> Self {
        let this = self.role(role);
        if active() {
            this.aria_label(label)
        } else {
            this
        }
    }

    /// Role and a name that takes work to build: `label` only runs while a client is attached.
    #[must_use]
    fn a11y_lazy(self, role: Role, label: impl FnOnce() -> String) -> Self {
        let this = self.role(role);
        if active() {
            this.aria_label(label())
        } else {
            this
        }
    }

    /// A button named by its tooltip or text.
    #[must_use]
    fn a11y_button(self, label: impl Into<SharedString>) -> Self {
        self.a11y(Role::Button, label)
    }

    /// A button that can be off or on (preview pane, split view).
    #[must_use]
    fn a11y_toggle(self, label: impl Into<SharedString>, on: bool) -> Self {
        self.a11y(Role::Button, label).aria_toggled(toggled(on))
    }

    /// Edits the node directly, for properties the element API doesn't cover. Runs only while a
    /// client is attached; one per element (see the module docs).
    #[must_use]
    fn a11y_node(self, edit: impl FnOnce(&mut Node) + 'static) -> Self {
        if active() {
            self.a11y_synthetic_children(move |builder| edit(builder.parent_node()))
        } else {
            self
        }
    }

    /// A control that can't be used right now.
    #[must_use]
    fn a11y_disabled(self, disabled: bool) -> Self {
        if disabled {
            self.a11y_node(Node::set_disabled)
        } else {
            self
        }
    }

    /// A menu command, greyed out unless `enabled`; `checked` adds the check state and
    /// `highlighted` reports it as focused while its menu has keyboard focus.
    #[must_use]
    fn a11y_menu_item(
        self,
        label: impl Into<SharedString>,
        enabled: bool,
        checked: bool,
        highlighted: bool,
    ) -> Self {
        let mut this = self.a11y(Role::MenuItem, label).a11y_disabled(!enabled);
        if checked {
            this = this.aria_toggled(Toggled::True);
        }
        if highlighted {
            this = this.aria_active_descendant();
        }
        this
    }

    /// A settings switch, named by the row it sits in.
    #[must_use]
    fn a11y_switch(self, on: bool) -> Self {
        self.role(Role::Switch)
            .aria_toggled(toggled(on))
            .a11y_node(|node| {
                if let Some(label) = scope_label() {
                    node.set_label(label.to_string());
                }
            })
    }

    /// A dropdown showing `value`, named by the row it sits in.
    #[must_use]
    fn a11y_combo(self, value: impl Into<SharedString>, open: bool) -> Self {
        let this = self.role(Role::ComboBox).aria_expanded(open);
        if !active() {
            return this;
        }
        this.aria_value(value).a11y_node(|node| {
            if let Some(label) = scope_label() {
                node.set_label(label.to_string());
            }
        })
    }

    /// A dropdown named `label` showing `value` (outside settings rows, which name it themselves).
    #[must_use]
    fn a11y_combo_named(
        self,
        label: impl Into<SharedString>,
        value: impl Into<SharedString>,
        open: bool,
    ) -> Self {
        let this = self.a11y(Role::ComboBox, label).aria_expanded(open);
        if active() {
            this.aria_value(value)
        } else {
            this
        }
    }

    /// A message that is read out when it appears or changes, without moving focus (toasts).
    /// [`Role::Alert`] interrupts; anything else waits for the reader to finish.
    #[must_use]
    fn a11y_live(self, role: Role, label: impl Into<SharedString>) -> Self {
        let live = if role == Role::Alert {
            Live::Assertive
        } else {
            Live::Polite
        };
        self.a11y(role, label)
            .a11y_node(move |node| node.set_live(live))
    }

    /// Reads `text` out whenever it changes, through an invisible status line inside this element.
    /// For lists driven from a text field (Ctrl+T): focus stays in the field, so moving the
    /// highlight wouldn't be heard otherwise.
    #[must_use]
    fn a11y_announce(self, text: impl FnOnce() -> Option<String>) -> Self {
        if !active() {
            return self;
        }
        let Some(text) = text() else {
            return self;
        };
        self.a11y_synthetic_children(move |builder| {
            let mut node = Node::new(Role::Status);
            node.set_label(text);
            node.set_live(Live::Polite);
            let id = builder.synthetic_node_id("announce");
            builder.push_child(id, node);
        })
    }

    /// A progress bar at `fraction` (0 to 1), or busy with no known end.
    #[must_use]
    fn a11y_progress(self, fraction: Option<f32>) -> Self {
        let this = self.role(Role::ProgressIndicator);
        match fraction {
            Some(f) => this
                .aria_min_numeric_value(0.)
                .aria_max_numeric_value(100.)
                .aria_numeric_value((f64::from(f) * 100.).round()),
            None => this,
        }
    }

    /// A list with several selectable items: `count` items in all (rows outside the view are not
    /// drawn, so the count is given rather than counted).
    #[must_use]
    fn a11y_list(self, label: impl FnOnce() -> String, count: usize) -> Self {
        self.a11y_lazy(Role::ListBox, label)
            .aria_size_of_set(count)
            .a11y_node(Node::set_multiselectable)
    }

    /// Item `ix` of an [`a11y_list`](Self::a11y_list). `cursor` reports it as focused while the
    /// list has keyboard focus, so arrow keys read the item under the cursor.
    #[must_use]
    fn a11y_item(
        self,
        ix: usize,
        selected: bool,
        cursor: bool,
        label: impl FnOnce() -> String,
    ) -> Self {
        let this = self
            .a11y_lazy(Role::ListBoxOption, label)
            .aria_selected(selected)
            .aria_position_in_set(ix);
        if cursor {
            this.aria_active_descendant()
        } else {
            this
        }
    }

    /// A column header, with the sort direction when the list is sorted by it.
    #[must_use]
    fn a11y_column_header(self, label: impl Into<SharedString>, sorted: Option<bool>) -> Self {
        let this = self.a11y(Role::ColumnHeader, label);
        match sorted {
            Some(ascending) => this.a11y_node(move |node| {
                node.set_sort_direction(if ascending {
                    SortDirection::Ascending
                } else {
                    SortDirection::Descending
                });
            }),
            None => this,
        }
    }
}

impl<E: StatefulInteractiveElement> A11yExt for E {}

/// Off or on as AccessKit's toggle state.
pub(crate) const fn toggled(on: bool) -> Toggled {
    if on { Toggled::True } else { Toggled::False }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_labels_skip_missing_details() {
        assert_eq!(
            item_label("notes.txt", &["Text Document", "", "1.2 KB"]),
            "notes.txt, Text Document, 1.2 KB"
        );
        assert_eq!(item_label("Photos", &[" ", ""]), "Photos");
    }

    #[test]
    fn activity_follows_sync_state() {
        set_active_for_test(true);
        assert!(active());
        set_active_for_test(false);
        assert!(!active());
    }
}
