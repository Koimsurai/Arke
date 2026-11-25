//! Settings page (Ctrl+,, the sidebar Settings entry, or the title bar context menu).
//!
//! Like Files and Windows 11 Settings it lives in the content area with the sidebar still visible.
//! Categories on the left of the paper, the selected category on the right. Each setting is a card
//! (icon, name and description, control on the right). Controls are a switch, a dropdown, or an
//! expander for choices that need a preview.
//!
//! Every change applies immediately and autosaves (`SettingsStore`); there is no Apply button.
//! Toggles found elsewhere (tab layout, density, preview pane, whole-PC search) are mirrored here.
//!
//! Switching tabs or navigating from the sidebar closes the page.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Animation, AnimationExt, AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyBinding, ScrollHandle, SharedString, Subscription, UniformListScrollHandle, WeakEntity,
    Window, actions, anchored, canvas, deferred, div, point, prelude::*, px, svg, uniform_list,
};
use rust_i18n::t;

use super::{Workspace, chosen_accent};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::explorer::ViewMode;
use crate::keynav::KeyStop;
use crate::scrollbar::ScrollbarDrag;
use crate::settings::{
    Accent, DarkShade, Density, FolderOrder, Language, LastTabAction, Material, NavScope,
    NewTabAction, Settings, SettingsStore, SortBy, StartLocation, TabLayout, ThemeMode, Tint,
};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{self, UiFont as _};

actions!(
    workspace,
    [
        OpenSettings,
        CloseSettings,
        DropdownUp,
        DropdownDown,
        DropdownAccept,
        NextControl,
        PrevControl,
        OtherArea
    ]
);

/// Max width of the card column, centered in the content area so wide windows don't leave one
/// side empty.
fn page_max_w() -> gpui::Pixels {
    crate::theme::scaled_px(1000.)
}
/// Minimum row height on every settings page (one or two lines of text), matching Windows 11
/// Settings.
pub(super) fn card_min_h() -> gpui::Pixels {
    crate::theme::scaled_px(64.)
}
/// Width of the icon column at the start of each row, so titles line up whatever the icon.
pub(super) fn icon_col() -> gpui::Pixels {
    crate::theme::scaled_px(20.)
}

pub(super) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-,", OpenSettings, Some("Workspace")),
        KeyBinding::new("escape", CloseSettings, Some("SettingsPage")),
        KeyBinding::new("up", DropdownUp, Some("SettingsPage")),
        KeyBinding::new("down", DropdownDown, Some("SettingsPage")),
        KeyBinding::new("enter", DropdownAccept, Some("SettingsPage")),
        KeyBinding::new("tab", NextControl, Some("SettingsPage")),
        KeyBinding::new("shift-tab", PrevControl, Some("SettingsPage")),
        KeyBinding::new("f6", OtherArea, Some("SettingsPage")),
        KeyBinding::new("shift-f6", OtherArea, Some("SettingsPage")),
    ]);
}

/// Open Settings page: which tab owns it and the folder that tab was on (closes if it changes).
pub(super) struct SettingsPage {
    pub(super) focus: FocusHandle,
    /// The categories, one tab stop: Up and Down pick the category (#322).
    nav_focus: FocusHandle,
    /// The theme, accent and tint tile groups, one tab stop each (#322).
    radios: [FocusHandle; 3],
    /// Settings is a tab, as in Files; this is the owning tab.
    pub(super) owner: super::pages::Owner,
    scroll: ScrollHandle,
    scrollbar_drag: ScrollbarDrag,
    /// The report text box in About › Report a problem, which scrolls inside the page.
    pub(super) report_scroll: ScrollHandle,
    pub(super) report_scrollbar: ScrollbarDrag,
    /// Accent hex input (`#RRGGBB`).
    accent_input: Entity<TextInput>,
    _accent_input: Subscription,
    /// Expanded cards.
    pub(super) expanded: HashSet<&'static str>,
    /// Background image path input.
    image_input: Entity<TextInput>,
    _image_input: Subscription,
    /// Installed fonts, listed in the background when the page opens.
    fonts: std::rc::Rc<Vec<SharedString>>,
    /// Selected category index.
    pub(super) category: usize,
    /// Settings import path input; the last export/import result is in `backup`.
    import_input: Entity<TextInput>,
    _import_input: Subscription,
    /// Path input for adding an "Open with" app.
    pub(super) app_input: Entity<TextInput>,
    _app_input: Subscription,
    /// About › Updates: the download route (`update_route.rs`).
    pub(super) route_input: Entity<TextInput>,
    _route_input: Subscription,
    /// Keyboard shortcuts: search and the key being recorded.
    pub(super) shortcuts: super::shortcuts::Page,
    pub(super) backup: Option<super::backup::BackupNote>,
    /// Open dropdown.
    dropdown: Option<Dropdown>,
    /// Scroll position of a long dropdown list (fonts), so the open list can show the selection.
    list_scroll: UniformListScrollHandle,
    /// The open dropdown's list, as last drawn, for the window it is shown in.
    dropdown_panel: Rc<std::cell::RefCell<Option<PanelBuilder>>>,
    /// Which dropdown it is (`Single::serial`), 0 while none is open.
    dropdown_shown: Rc<std::cell::Cell<u64>>,
    /// Where each dropdown's button is in the window.
    dropdown_buttons:
        Rc<std::cell::RefCell<std::collections::HashMap<&'static str, gpui::Bounds<gpui::Pixels>>>>,
    /// Thumbnail of the background image, decoded in the background when it changes.
    backdrop: Option<BackdropThumb>,
    /// Whether the opacity slider is being dragged.
    slider_drag: std::rc::Rc<std::cell::Cell<bool>>,
    /// "Remove Arke from this PC" confirmation, when open.
    pub(super) uninstall: Option<super::uninstall_page::UninstallAsk>,
}

/// The background image card's thumbnail.
struct BackdropThumb {
    path: PathBuf,
    /// `None` while decoding.
    image: Option<std::sync::Arc<gpui::RenderImage>>,
    /// The file couldn't be read or decoded.
    failed: bool,
}

/// Thumbnail and preview size in the background image card.
const BACKDROP_THUMB: (f32, f32) = (160., 100.);
/// Decoded size: twice the thumbnail, for high-DPI screens.
const BACKDROP_THUMB_PX: u32 = 320;
fn slider_w() -> gpui::Pixels {
    crate::theme::scaled_px(240.)
}

/// What a Browse button picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Browse {
    BackgroundImage,
    OpenWithProgram,
    ImportSettings,
}

impl Browse {
    fn request(self, s: &Settings, downloads: Option<PathBuf>) -> crate::shell::PickRequest {
        let (title, filter_name, patterns, folder) = match self {
            Self::BackgroundImage => (
                t!("settings.background_pick_title"),
                t!("settings.filter_images"),
                "*.png;*.jpg;*.jpeg;*.jfif;*.gif;*.bmp;*.webp;*.tif;*.tiff",
                s.background_image
                    .as_deref()
                    .and_then(std::path::Path::parent)
                    .map(PathBuf::from),
            ),
            Self::OpenWithProgram => (
                t!("openwith.pick_title"),
                t!("openwith.filter"),
                "*.exe",
                None,
            ),
            // Exports land in Downloads, so start there.
            Self::ImportSettings => (
                t!("backup.pick_title"),
                t!("backup.filter"),
                "*.json",
                downloads,
            ),
        };
        crate::shell::PickRequest {
            title: title.into_owned(),
            filter_name: filter_name.into_owned(),
            patterns: patterns.to_owned(),
            folder,
        }
    }
}

/// What selecting a dropdown option does (change a setting, or act, like resizing a journal).
type Applier = std::rc::Rc<dyn Fn(&mut App)>;

/// Font for each option's label, by option index (the font list draws each name in its font).
type OptionFont = std::rc::Rc<dyn Fn(usize) -> Option<SharedString>>;

/// Open dropdown: its id, keyboard cursor, and per-option appliers for arrow keys and Enter.
struct Dropdown {
    id: &'static str,
    cursor: usize,
    /// Whether the cursor is shown: only once an arrow key moved it. Opened with the mouse, the
    /// row under the pointer is the only one lit, as in a Windows combo box; the current row lit
    /// next to the hovered one read as two selections.
    keyboard: bool,
    options: Vec<Applier>,
    /// The window its list is drawn in (`popup.rs`).
    popup: crate::popup::Single,
}

/// Draws an open dropdown's list.
type PanelBuilder = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// Slide distance for the page's small animations (dropdowns, expanders, category changes). See
/// `motion.rs`.
const ANIM_SLIDE: f32 = crate::motion::NUDGE;

fn anim() -> Animation {
    crate::motion::enter()
}

thread_local! {
    /// The switch that was just clicked (and its serial). Only it animates; the rest draw in place.
    static LAST_TOGGLE: std::cell::Cell<Option<(&'static str, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// Below this window width the category list shows icons only.
fn nav_compact_below() -> gpui::Pixels {
    crate::theme::scaled_px(1040.)
}
fn nav_w() -> gpui::Pixels {
    crate::theme::scaled_px(208.)
}
fn nav_w_compact() -> gpui::Pixels {
    crate::theme::scaled_px(56.)
}
/// Minimum width of dropdown buttons and lists.
fn dropdown_min_w() -> gpui::Pixels {
    crate::theme::scaled_px(148.)
}
fn dropdown_max_h() -> gpui::Pixels {
    crate::theme::scaled_px(340.)
}
/// Lists longer than this only build the rows in view (hundreds of fonts, each in its own font).
const LONG_LIST: usize = 12;
/// Width of the font list, which shows a sample next to each name.
fn font_list_w() -> gpui::Pixels {
    crate::theme::scaled_px(380.)
}

/// Context for drawing controls: the open dropdown, who to update, and the viewport size
/// (the outside layer has to cover it).
pub(super) struct Ui {
    /// Open dropdown and the keyboard cursor in it, while shown.
    open: Option<(&'static str, Option<usize>)>,
    workspace: WeakEntity<Workspace>,
    viewport: gpui::Size<gpui::Pixels>,
    list_scroll: UniformListScrollHandle,
    /// The open dropdown puts its list here.
    panel: Rc<std::cell::RefCell<Option<PanelBuilder>>>,
    /// Every dropdown records where its button is.
    buttons:
        Rc<std::cell::RefCell<std::collections::HashMap<&'static str, gpui::Bounds<gpui::Pixels>>>>,
}

/// Opens (`Some`) or closes the dropdown.
fn set_dropdown(workspace: &WeakEntity<Workspace>, dropdown: Option<Dropdown>, cx: &mut App) {
    let _ = workspace.update(cx, |this, cx| {
        if let Some(page) = &mut this.settings_panel {
            page.dropdown = dropdown;
        }
        cx.notify();
    });
}

/// Hue strip step in degrees and number of steps.
const HUE_STEP: f32 = 10.;
const HUE_STEPS: usize = 36;

/// Accent color for the hex input; defaults to Arke orange.
fn accent_color(accent: Accent) -> u32 {
    chosen_accent(accent).unwrap_or(theme::ARKE_ACCENT)
}

/// A focus handle Tab stops at (#322): a tracked handle is a tab stop by its own setting, not
/// the element's.
fn stop_handle(cx: &mut App) -> FocusHandle {
    cx.focus_handle().tab_index(0).tab_stop(true)
}

/// Index of the About category; the "Import Arke settings" context menu opens it directly.
pub(super) const ABOUT: usize = 7;

/// Changes a setting: applies now, saves in the background shortly after.
pub(super) fn set(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx));
}

impl Workspace {
    #[allow(
        clippy::too_many_lines,
        reason = "the page's whole state is made here, field by field"
    )]
    pub(super) fn open_settings(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // QuickLook may have been started (or quit) since the last check.
        Self::recheck_quicklook(cx);
        // Already open: switch to that tab.
        let existing = self.settings_panel.as_ref().map(|p| p.owner.clone());
        if self.go_to_page(existing.as_ref(), window, cx) {
            return;
        }
        // Open in a new tab as Files does, keeping the folder tab.
        self.open_page_tab(window, cx);
        LAST_TOGGLE.set(None);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let owner = self.owner_here(true, cx);
        let accent = SettingsStore::global(cx).read(cx).settings.accent;
        let accent_input = cx.new(|cx| {
            let mut input = TextInput::new("#RRGGBB", cx).key_stop();
            input.set_text(Accent::hex(accent_color(accent)), cx);
            input
        });
        let accent_sub = cx.subscribe_in(&accent_input, window, Self::on_accent_input);
        let image = SettingsStore::global(cx)
            .read(cx)
            .settings
            .background_image
            .clone();
        let image_input = cx.new(|cx| {
            let mut input =
                TextInput::new(t!("settings.background_path").to_string(), cx).key_stop();
            input.set_text(image.map_or_default(|p| p.display().to_string()), cx);
            input
        });
        let image_sub = cx.subscribe_in(&image_input, window, Self::on_image_input);
        let import_input =
            cx.new(|cx| TextInput::new(t!("backup.import_placeholder").to_string(), cx).key_stop());
        let import_sub = cx.subscribe_in(&import_input, window, Self::on_import_input);
        let app_input =
            cx.new(|cx| TextInput::new(t!("openwith.placeholder").to_string(), cx).key_stop());
        let app_sub = cx.subscribe_in(&app_input, window, Self::on_app_input);
        let route = SettingsStore::global(cx)
            .read(cx)
            .settings
            .update_route
            .clone();
        let route_input = cx.new(|cx| {
            let mut input =
                TextInput::new(t!("update.route_placeholder").to_string(), cx).key_stop();
            input.set_text(route, cx);
            input
        });
        let route_sub = cx.subscribe_in(&route_input, window, Self::on_route_input);
        let shortcuts = super::shortcuts::Page::new(window, cx);
        // List fonts in the background; there can be hundreds.
        let text_system = Arc::clone(cx.text_system());
        cx.spawn(async move |this, cx| {
            let names = cx
                .background_executor()
                .spawn(async move { text_system.all_font_names() })
                .await;
            let fonts: Vec<SharedString> = names
                .into_iter()
                // Names starting with @ are vertical-writing variants of the same font.
                .filter(|name| !name.starts_with('@'))
                .map(SharedString::from)
                .collect();
            let _ = this.update(cx, |this, cx| {
                if let Some(page) = &mut this.settings_panel {
                    page.fonts = std::rc::Rc::new(fonts);
                }
                cx.notify();
            });
        })
        .detach();
        self.settings_panel = Some(SettingsPage {
            focus,
            nav_focus: stop_handle(cx),
            radios: std::array::from_fn(|_| stop_handle(cx)),
            owner,
            scroll: ScrollHandle::new(),
            scrollbar_drag: ScrollbarDrag::default(),
            report_scroll: ScrollHandle::new(),
            report_scrollbar: ScrollbarDrag::default(),
            accent_input,
            _accent_input: accent_sub,
            image_input,
            _image_input: image_sub,
            fonts: std::rc::Rc::default(),
            // Theme and accent show their previews right away; the rest stay collapsed to keep
            // the page short.
            expanded: HashSet::from(["theme", "accent"]),
            category: 0,
            import_input,
            _import_input: import_sub,
            app_input,
            _app_input: app_sub,
            route_input,
            _route_input: route_sub,
            shortcuts,
            backup: None,
            dropdown: None,
            list_scroll: UniformListScrollHandle::new(),
            dropdown_panel: Rc::default(),
            dropdown_shown: Rc::default(),
            dropdown_buttons: Rc::default(),
            backdrop: None,
            slider_drag: std::rc::Rc::default(),
            uninstall: None,
        });
        self.sync_backdrop_thumb(cx);
        cx.notify();
    }

    /// Decodes the background image card's thumbnail on a background thread whenever the image
    /// changes (Browse, the path field, the image context menu).
    fn sync_backdrop_thumb(&mut self, cx: &mut Context<Self>) {
        let wanted = SettingsStore::global(cx)
            .read(cx)
            .settings
            .background_image
            .clone();
        let Some(page) = &mut self.settings_panel else {
            return;
        };
        if page.backdrop.as_ref().map(|t| &t.path) == wanted.as_ref() {
            return;
        }
        // Free the old thumbnail's texture, not just the reference.
        if let Some(old) = page.backdrop.take().and_then(|t| t.image) {
            crate::media::drop_image_later(old, cx);
        }
        let Some(path) = wanted else {
            return;
        };
        page.backdrop = Some(BackdropThumb {
            path: path.clone(),
            image: None,
            failed: false,
        });
        let fs = Arc::clone(&self.services.fs);
        let (tx, rx) = flume::bounded(1);
        let source = path.clone();
        crate::explorer::spawn_thread("backdrop-thumb", move || {
            let thumb = fs
                .read(&source, arke_core::filetype::MAX_THUMBNAIL_SOURCE)
                .ok()
                .and_then(|bytes| {
                    arke_core::thumbs::decode_thumbnail(&bytes, BACKDROP_THUMB_PX).ok()
                });
            let _ = tx.send(thumb);
        });
        cx.spawn(async move |this, cx| {
            let Ok(thumb) = rx.recv_async().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let image = thumb.and_then(crate::media::to_render_image);
                match this
                    .settings_panel
                    .as_mut()
                    .and_then(|p| p.backdrop.as_mut())
                    .filter(|t| t.path == path)
                {
                    Some(thumb) => {
                        thumb.failed = image.is_none();
                        thumb.image = image;
                    }
                    // Changed again while decoding.
                    None => {
                        if let Some(image) = image {
                            crate::media::drop_image_later(image, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Browse button: the Windows "Open" dialog (in its own process, owned by this window) on a
    /// background thread; the chosen file is applied when it closes.
    pub(super) fn browse(&mut self, what: Browse, window: &mut Window, cx: &mut Context<Self>) {
        let settings = SettingsStore::global(cx).read(cx).settings.clone();
        let downloads = self
            .active_view()
            .read(cx)
            .places
            .iter()
            .find(|p| {
                matches!(
                    p.kind,
                    crate::shell::PlaceKind::Folder(crate::shell::KnownFolder::Downloads)
                )
            })
            .map(|p| p.path.clone());
        let request = what.request(&settings, downloads);
        let shell = Arc::clone(&self.services.shell);
        let owner = crate::explorer::Explorer::owner_from(window);
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("pick-file", move || {
            let _ = tx.send(shell.pick_file(&request, owner));
        });
        cx.spawn(async move |this, cx| match rx.recv_async().await {
            Ok(Ok(Some(path))) => {
                let _ = this.update(cx, |_, cx| Self::picked(what, path, cx));
            }
            Ok(Err(err)) => log::warn!("file dialog failed: {err}"),
            _ => {}
        })
        .detach();
    }

    fn picked(what: Browse, path: PathBuf, cx: &mut Context<Self>) {
        match what {
            Browse::BackgroundImage => set(cx, |s| s.background_image = Some(path)),
            Browse::OpenWithProgram => set(cx, |s| {
                if !s.open_with_extra.contains(&path) {
                    s.open_with_extra.push(path);
                }
            }),
            Browse::ImportSettings => Self::import_settings(path, cx),
        }
        cx.notify();
    }

    /// Hex input: a full six-digit value applies as soon as it is typed or pasted. Enter and Esc return to the page.
    fn on_accent_input(
        &mut self,
        input: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = input.read(cx).committed_text();
        // While typing only accept six digits, otherwise the third digit would apply as #RGB. Shorthand needs Enter.
        let complete = text.trim().trim_start_matches('#').len() == 6;
        if (complete || *event == InputEvent::Confirm)
            && let Some(color) = Accent::parse_hex(&text)
        {
            set(cx, |s| {
                if accent_color(s.accent) != color {
                    s.accent = Accent::Color(color);
                }
            });
        }
        if matches!(event, InputEvent::Confirm | InputEvent::Cancel)
            && let Some(page) = &self.settings_panel
        {
            window.focus(&page.focus, cx);
            self.sync_accent_input(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn accent_input_for_test(&self) -> Option<Entity<TextInput>> {
        self.settings_panel.as_ref().map(|p| p.accent_input.clone())
    }

    /// Shows a category by its title.
    #[cfg(test)]
    pub(crate) fn settings_category_for_test(&mut self, title: &str, cx: &mut Context<Self>) {
        let titles = [
            t!("settings.appearance"),
            t!("settings.files"),
            t!("settings.sorting"),
            t!("settings.startup"),
            t!("settings.search"),
            t!("settings.context_menu"),
            t!("settings.shortcuts"),
            t!("settings.about"),
        ];
        let ix = titles
            .iter()
            .position(|t| t == title)
            .expect("no such settings category");
        if let Some(page) = &mut self.settings_panel {
            page.category = ix;
        }
        cx.notify();
    }

    /// Opens a card that expands, by its id.
    #[cfg(test)]
    pub(crate) fn expand_settings_card_for_test(
        &mut self,
        id: &'static str,
        cx: &mut Context<Self>,
    ) {
        if let Some(page) = &mut self.settings_panel {
            page.expanded.insert(id);
        }
        cx.notify();
    }

    /// Vertical scroll offsets of the settings page and of the report text box.
    #[cfg(test)]
    pub(crate) fn settings_scroll_for_test(&self) -> Option<(gpui::Pixels, gpui::Pixels)> {
        self.settings_panel
            .as_ref()
            .map(|p| (p.scroll.offset().y, p.report_scroll.offset().y))
    }

    /// The open dropdown's list as placed on the screen (physical pixels), once placed.
    #[cfg(test)]
    pub(crate) fn settings_dropdown_placed_for_test(&self) -> Option<crate::popup::ScreenRect> {
        self.settings_panel
            .as_ref()?
            .dropdown
            .as_ref()?
            .popup
            .placed()
    }

    /// The open dropdown's list as last drawn on the screen (physical pixels).
    #[cfg(test)]
    pub(crate) fn settings_dropdown_drawn_for_test(&self) -> Option<crate::popup::ScreenRect> {
        self.settings_panel
            .as_ref()?
            .dropdown
            .as_ref()?
            .popup
            .drawn_on_screen()
    }

    /// The open dropdown's keyboard cursor and whether it is shown.
    #[cfg(test)]
    pub(crate) fn settings_dropdown_cursor_for_test(&self) -> Option<(usize, bool)> {
        let dropdown = self.settings_panel.as_ref()?.dropdown.as_ref()?;
        Some((dropdown.cursor, dropdown.keyboard))
    }

    /// Presses Browse on the background image card (`image`) or the Open-with card.
    #[cfg(test)]
    pub(crate) fn browse_for_test(
        &mut self,
        image: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let what = if image {
            Browse::BackgroundImage
        } else {
            Browse::OpenWithProgram
        };
        self.browse(what, window, cx);
    }

    /// Background image path input: Enter applies (empty means no image), Esc cancels.
    /// The download route is saved as typed; Settings says under it when it can't be used.
    fn on_route_input(
        &mut self,
        input: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = input.read(cx).committed_text().trim().to_owned();
        set(cx, |s| {
            if s.update_route != text {
                s.update_route = text;
            }
        });
        if matches!(event, InputEvent::Confirm | InputEvent::Cancel)
            && let Some(page) = &self.settings_panel
        {
            window.focus(&page.focus, cx);
        }
    }

    fn on_image_input(
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
            // Paths pasted from Explorer's "Copy as path" are quoted.
            let text = input.read(cx).committed_text();
            let text = text.trim().trim_matches('"').trim();
            let path = (!text.is_empty()).then(|| PathBuf::from(text));
            set(cx, |s| s.background_image = path);
        }
        if let Some(page) = &self.settings_panel {
            window.focus(&page.focus, cx);
        }
        self.sync_accent_input(window, cx);
    }

    /// Keeps the hex input in sync when the accent changes elsewhere (swatches, hue strip). Left alone while focused.
    pub(super) fn sync_accent_input(&mut self, window: &Window, cx: &mut Context<Self>) {
        // Settings changed: the background image may be a new one.
        self.sync_backdrop_thumb(cx);
        let Some(page) = &self.settings_panel else {
            return;
        };
        // Same for the background image path, which the clear button and image context menu can change.
        let image = page.image_input.clone();
        if !image.focus_handle(cx).is_focused(window) {
            let text = SettingsStore::global(cx)
                .read(cx)
                .settings
                .background_image
                .as_ref()
                .map_or_default(|p| p.display().to_string());
            image.update(cx, |input, cx| {
                if input.text() != text {
                    input.set_text(text, cx);
                }
            });
        }
        let input = page.accent_input.clone();
        if input.focus_handle(cx).is_focused(window) {
            return;
        }
        let text = Accent::hex(accent_color(
            SettingsStore::global(cx).read(cx).settings.accent,
        ));
        input.update(cx, |input, cx| {
            if input.text() != text {
                input.set_text(text, cx);
            }
        });
    }

    pub(super) fn close_settings(
        &mut self,
        _: &CloseSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Esc closes the dropdown first; a second Esc leaves the page.
        if let Some(page) = &mut self.settings_panel
            && page.dropdown.take().is_some()
        {
            cx.notify();
            return;
        }
        if let Some(page) = self.settings_panel.take() {
            self.dismiss_page(&page.owner, window, cx);
        }
    }

    /// After the page drew: the open dropdown's list is shown in a window of its own under its
    /// button (`popup.rs`), redrawn with what the page just drew.
    pub(super) fn sync_dropdown_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The list shown last frame draws again after this one: with the rows the page draws
        // now, or not at all once its dropdown (or the page) closed, and its window goes.
        if let Some(popup) = self.dropdown_window.take() {
            App::defer(cx, move |cx| {
                let _ = popup.update(cx, |_, window, _| window.refresh());
            });
        }
        let Some(page) = self.settings_panel.as_mut() else {
            return;
        };
        page.dropdown_shown
            .set(page.dropdown.as_ref().map_or(0, |d| d.popup.serial()));
        let Some(dropdown) = page.dropdown.as_mut() else {
            return;
        };
        let Some(button) = page.dropdown_buttons.borrow().get(dropdown.id).copied() else {
            return;
        };
        let control = crate::popup::rect_to_screen(window, button);
        let content: crate::popup::Content = {
            let panel = Rc::downgrade(&page.dropdown_panel);
            let shown = Rc::downgrade(&page.dropdown_shown);
            let serial = dropdown.popup.serial();
            Rc::new(move |_, window, cx| {
                if shown.upgrade()?.get() != serial {
                    return None;
                }
                let build = panel.upgrade()?.borrow().clone()?;
                Some(build(window, cx))
            })
        };
        dropdown.popup.ensure_handed(
            || crate::popup::Anchor::Below {
                control,
                align_right: true,
            },
            |this: &mut Self| {
                this.settings_panel
                    .as_mut()?
                    .dropdown
                    .as_mut()
                    .map(|d| &mut d.popup)
            },
            content,
            |this, _, cx| {
                if let Some(page) = this.settings_panel.as_mut() {
                    page.dropdown = None;
                }
                cx.notify();
            },
            window,
            cx,
        );
        self.dropdown_window = page.dropdown.as_ref().and_then(|d| d.popup.window());
    }

    /// Arrow keys move and Enter selects while a dropdown is open.
    fn dropdown_move(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(dropdown) = self
            .settings_panel
            .as_mut()
            .and_then(|p| p.dropdown.as_mut())
        else {
            // No list open: the arrow is for the control with the focus (#322).
            cx.propagate();
            return;
        };
        let last = dropdown.options.len().saturating_sub(1);
        dropdown.keyboard = true;
        dropdown.cursor = if down {
            (dropdown.cursor + 1).min(last)
        } else {
            dropdown.cursor.saturating_sub(1)
        };
        let cursor = dropdown.cursor;
        if let Some(page) = &self.settings_panel {
            page.list_scroll
                .scroll_to_item(cursor, gpui::ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn dropdown_up(&mut self, _: &DropdownUp, _: &mut Window, cx: &mut Context<Self>) {
        self.dropdown_move(false, cx);
    }

    fn dropdown_down(&mut self, _: &DropdownDown, _: &mut Window, cx: &mut Context<Self>) {
        self.dropdown_move(true, cx);
    }

    fn dropdown_accept(&mut self, _: &DropdownAccept, _: &mut Window, cx: &mut Context<Self>) {
        let Some(dropdown) = self.settings_panel.as_mut().and_then(|p| p.dropdown.take()) else {
            // No list open: Enter presses the control with the focus (#322).
            cx.propagate();
            return;
        };
        if let Some(apply) = dropdown.options.get(dropdown.cursor).cloned() {
            apply(cx);
        }
        cx.notify();
    }

    /// Tab: the next control on the page; an open list closes first, as in Windows.
    fn next_control(&mut self, _: &NextControl, window: &mut Window, cx: &mut Context<Self>) {
        self.close_dropdown(cx);
        crate::keynav::next(window, cx);
    }

    /// Shift+Tab: the previous control on the page.
    fn prev_control(&mut self, _: &PrevControl, window: &mut Window, cx: &mut Context<Self>) {
        self.close_dropdown(cx);
        crate::keynav::prev(window, cx);
    }

    /// F6 and Shift+F6: between the categories and the page's controls.
    fn other_area(&mut self, _: &OtherArea, window: &mut Window, cx: &mut Context<Self>) {
        self.close_dropdown(cx);
        let Some(page) = &self.settings_panel else {
            return;
        };
        if page.nav_focus.is_focused(window) {
            // The categories come first in Tab order: the next stop is the page's first control.
            crate::keynav::next(window, cx);
        } else {
            window.focus(&page.nav_focus, cx);
        }
    }

    fn close_dropdown(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = &mut self.settings_panel
            && page.dropdown.take().is_some()
        {
            cx.notify();
        }
    }

    /// Up and Down on the categories (Home and End too): selects that category and shows its
    /// page, as Windows Settings does.
    fn nav_key(&mut self, key: &str, count: usize, cx: &mut Context<Self>) -> bool {
        let Some(page) = &mut self.settings_panel else {
            return false;
        };
        let last = count.saturating_sub(1);
        let at = page.category.min(last);
        let to = match key {
            "up" => at.saturating_sub(1),
            "down" => (at + 1).min(last),
            "home" => 0,
            "end" => last,
            _ => return false,
        };
        if to != page.category {
            page.category = to;
            page.dropdown = None;
            LAST_TOGGLE.set(None);
            page.scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }
        true
    }

    /// Whether the active tab is showing Settings.
    pub(super) fn settings_open(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(|p| self.shows(&p.owner))
    }

    /// Called before paint: closes Settings once its tab is gone or has navigated elsewhere.
    pub(super) fn settings_follow_navigation(&mut self, cx: &App) {
        let Some(page) = &self.settings_panel else {
            return;
        };
        // Only closing the tab or navigating counts, not switching to another tab.
        if !self.owner_alive(&page.owner, cx) {
            self.settings_panel = None;
        }
    }

    /// Sidebar Settings button at the bottom; clicking it again closes Settings. While open its
    /// text darkens but it gets no accent bar: the category list on the page is the one place
    /// that shows where you are, and the tab already says Settings.
    pub(super) fn render_settings_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let open = self.settings_open();
        div()
            .id("open-settings")
            .a11y(gpui::Role::Link, t!("settings.title"))
            .relative()
            .flex_none()
            .h(theme::h_row())
            .mx(theme::s_8())
            .mb(theme::s_8())
            .pl(theme::s_8())
            .flex()
            .items_center()
            .gap(theme::s_8())
            .rounded(theme::r_8())
            .cursor_pointer()
            .when(open, |d| {
                d.text_color(theme::text())
                    .hover(|s| s.bg(theme::wash()))
            })
            .when(!open, |d| {
                d.text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
            })
            .child(
                svg()
                    .path(Icon::Settings.path())
                    .size(theme::icon())
                    .text_color(if open {
                        theme::text()
                    } else {
                        theme::text_subtle()
                    }),
            )
            .child(t!("settings.title").into_owned())
            // Update badge when a new version is available.
            .children(super::update_page::sidebar_badge(cx))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if open {
                    this.close_settings(&CloseSettings, window, cx);
                } else {
                    this.open_settings(&OpenSettings, window, cx);
                }
            }))
    }

    /// Settings page for the content area, or `None` when closed.
    #[allow(
        clippy::too_many_lines,
        reason = "The whole page layout (categories, content, scrollbar) is kept together so the structure is visible"
    )]
    pub(super) fn render_settings_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let page = self
            .settings_panel
            .as_ref()
            .filter(|p| self.shows(&p.owner))?;
        let s = SettingsStore::global(cx).read(cx).settings.clone();
        let top_bar = Self::render_settings_top_bar(window, cx);
        let compact = self.last_viewport_width < nav_compact_below();
        let mut categories = Self::settings_sections(&s, page, window, cx);
        let selected = page.category.min(categories.len().saturating_sub(1));
        let count = categories.len();
        // The ring goes on the selected category rather than round the whole column.
        // The control with the focus went (a row's button that closed with what it did): the page
        // takes the focus, so the keys keep working (#322).
        if window.focused(cx).is_none() {
            let focus = page.focus.clone();
            cx.defer_in(window, move |_, window, cx| {
                if window.focused(cx).is_none() {
                    window.focus(&focus, cx);
                }
            });
        }
        let nav_held = page.nav_focus.is_focused(window);
        let nav_ring = nav_held && window.last_input_was_keyboard();
        crate::keynav::set_view(Some(page.scroll.clone()));
        let nav = div()
            .id("settings-nav")
            .a11y(gpui::Role::TabList, t!("settings.title"))
            .track_focus(&page.nav_focus)
            .key_stop_plain()
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && this.nav_key(&event.keystroke.key, count, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .flex_none()
            .w(if compact { nav_w_compact() } else { nav_w() })
            .h_full()
            .p(theme::s_8())
            .flex()
            .flex_col()
            .gap(theme::s_2())
            .border_r_1()
            .border_color(theme::line())
            .children(
                categories
                    .iter()
                    .enumerate()
                    .map(|(ix, (title, icon, _))| {
                        let on = ix == selected;
                        div()
                            .id(("settings-category", ix))
                            .a11y_lazy(gpui::Role::Tab, || title.to_string())
                            .aria_selected(on)
                            // Read as focused while the categories have the focus, not while a
                            // control on the page under them does.
                            .when(on && nav_held, gpui::StatefulInteractiveElement::aria_active_descendant)
                            .when(on && nav_ring, |d| d.shadow(crate::keynav::ring()))
                            .relative()
                            .h(theme::h_row() + theme::s_4())
                            .px(theme::s_12())
                            .flex()
                            .items_center()
                            .gap(theme::s_12())
                            .rounded(theme::r_8())
                            .cursor_pointer()
                            .when(on, |d| d.bg(theme::selection()))
                            .when(!on, |d| {
                                d.text_color(theme::text_muted())
                                    .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
                            })
                            .when(compact, |d| d.tooltip(super::tooltip(title.to_string())))
                            .child(
                                svg()
                                    .path(icon.path())
                                    .flex_none()
                                    .size(theme::icon())
                                    .text_color(if on {
                                        theme::text()
                                    } else {
                                        theme::text_subtle()
                                    }),
                            )
                            .when(!compact, |d| {
                                d.child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .when(on, |d| d.font_weight(theme::FW_STRONG))
                                        .child(title.to_string()),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(page) = &mut this.settings_panel
                                    && page.category != ix
                                {
                                    page.category = ix;
                                    page.dropdown = None;
                                    LAST_TOGGLE.set(None);
                                    // Start at the top when switching category.
                                    page.scroll.set_offset(point(px(0.), px(0.)));
                                }
                                cx.notify();
                            }))
                    })
                    .collect::<Vec<_>>(),
            );
        let (title, _, cards) = categories.swap_remove(selected);
        let body = div()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&page.scroll)
            .flex()
            .flex_col()
            .child(
                // Centered column with symmetric padding.
                div()
                    .w_full()
                    .px(theme::s_32())
                    .pt(theme::s_24())
                    .pb(theme::s_32())
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(page_max_w())
                            .flex()
                            .flex_col()
                            .gap(theme::s_4())
                            .child({
                                let title = SharedString::from(title.into_owned());
                                div()
                                    .id("settings-page-title")
                                    .a11y(gpui::Role::Heading, title.clone())
                                    .aria_level(1)
                                    .pb(theme::s_4())
                                    .text_size(theme::fs_24())
                                    .font_weight(theme::FW_STRONG)
                                    .child(title)
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(theme::s_8())
                                    .children(cards)
                                    .with_animation(
                                        ("settings-category-body", selected),
                                        anim(),
                                        |el, t| crate::motion::slide_in(el, t, 0., ANIM_SLIDE),
                                    ),
                            ),
                    ),
            );
        Some(
            div()
                .id("settings-page")
                .a11y(gpui::Role::Group, t!("settings.title"))
                .key_context("SettingsPage")
                .track_focus(&page.focus)
                .on_action(cx.listener(Self::close_settings))
                .on_action(cx.listener(Self::dropdown_up))
                .on_action(cx.listener(Self::dropdown_down))
                .on_action(cx.listener(Self::dropdown_accept))
                .on_action(cx.listener(Self::next_control))
                .on_action(cx.listener(Self::prev_control))
                .on_action(cx.listener(Self::other_area))
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(top_bar)
                .children(self.tab_strip_under_bar(window, cx))
                .child(
                    // Same paper surface as the file list.
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .mr(theme::s_8())
                        .mb(theme::s_8())
                        .rounded(theme::r_12())
                        .bg(theme::paper())
                        .shadow(theme::elev_2())
                        .overflow_hidden()
                        .child(
                            div()
                                .size_full()
                                .flex()
                                .child(nav)
                                .child(div().flex_1().min_w_0().h_full().child(body)),
                        )
                        .child(crate::scrollbar::scrollbar(
                            page.scroll.clone(),
                            Rc::clone(&page.scrollbar_drag),
                        ))
                        .children(self.render_uninstall_dialog(cx)),
                )
                .into_any_element(),
        )
    }

    /// Settings top bar: title, drag area, caption buttons.
    fn render_settings_top_bar(
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        div()
            .h(theme::h_bar())
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::s_2())
            .pr(theme::s_8())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::s_8())
                    .px(theme::s_8())
                    .child(
                        svg()
                            .path(Icon::Settings.path())
                            .size(theme::icon())
                            .text_color(theme::text_muted()),
                    )
                    .child(
                        div()
                            .font_weight(theme::FW_STRONG)
                            .child(t!("settings.title").into_owned()),
                    ),
            )
            .child(super::drag_area("settings-drag", cx).flex_1().h_full())
            .child(crate::explorer::caption_buttons(window))
    }

    /// Each category (name, icon) and its cards.
    #[allow(
        clippy::too_many_lines,
        reason = "each category and its cards, in the order shown"
    )]
    fn settings_sections(
        s: &Settings,
        page: &SettingsPage,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<(Cow<'static, str>, Icon, Vec<AnyElement>)> {
        let ui = Ui {
            open: page
                .dropdown
                .as_ref()
                .map(|d| (d.id, d.keyboard.then_some(d.cursor))),
            workspace: cx.entity().downgrade(),
            viewport: window.viewport_size(),
            list_scroll: page.list_scroll.clone(),
            panel: Rc::clone(&page.dropdown_panel),
            buttons: Rc::clone(&page.dropdown_buttons),
        };
        let ui = &ui;
        // Whether a tile group has the focus, and by keyboard (the picked tile shows the ring).
        let keys = |focus: &FocusHandle| {
            let focused = focus.is_focused(window);
            (focused, focused && window.last_input_was_keyboard())
        };
        let material = theme::mica_available().then(|| {
            card(
                Icon::Window,
                t!("settings.material"),
                Some(t!("settings.material_desc")),
                choice(
                    "material",
                    &[
                        (Material::Mica, t!("material.mica")),
                        (Material::MicaAlt, t!("material.mica_alt")),
                        (Material::Solid, t!("material.solid")),
                    ],
                    s.material,
                    |st, v| st.material = v,
                    ui,
                ),
            )
        });
        let mut appearance = vec![expander(
            "theme",
            Icon::Theme,
            t!("settings.theme"),
            t!("settings.theme_desc"),
            text_summary(theme_label(s.theme, s.dark_shade)),
            page.expanded.contains("theme"),
            theme_body(s, &page.radios[0], keys(&page.radios[0])),
            cx,
        )];
        appearance.extend(material);
        appearance.extend([
            expander(
                "accent",
                Icon::Accent,
                t!("settings.accent"),
                t!("settings.accent_desc"),
                look_summary(theme::signal(), accent_label(s.accent)),
                page.expanded.contains("accent"),
                accent_body(s, &page.accent_input, &page.radios[1], window, cx),
                cx,
            ),
            expander(
                "tint",
                Icon::Theme,
                t!("settings.tint"),
                t!("settings.tint_desc"),
                look_summary(theme::tint_preview(tint_color(s)), tint_label(s.tint)),
                page.expanded.contains("tint"),
                tint_body(s, &page.radios[2], keys(&page.radios[2])),
                cx,
            ),
            expander(
                "background",
                Icon::Image,
                t!("settings.background"),
                t!("settings.background_desc"),
                background_summary(s, page),
                page.expanded.contains("background"),
                background_body(s, page, window, cx),
                cx,
            ),
            font_card(s, &page.fonts, ui),
            // One card, each choice on a row of its own with its name (#230).
            div()
                .flex()
                .flex_col()
                .child(card(
                    Icon::Density,
                    t!("settings.ui_scale"),
                    None,
                    choice(
                        "ui-scale",
                        &theme::SCALES.map(|pct| (pct, Cow::Owned(format!("{pct}%")))),
                        s.ui_scale,
                        |st, v: u32| st.ui_scale = v,
                        ui,
                    ),
                ))
                .child(sub_row(
                    t!("settings.list_density"),
                    choice(
                        "density",
                        &[
                            (Density::Compact, t!("density.compact")),
                            (Density::Standard, t!("density.standard")),
                            (Density::Comfortable, t!("density.comfortable")),
                        ],
                        s.density,
                        |st, v| st.density = v,
                        ui,
                    ),
                ))
                .into_any_element(),
            tabs_card(s, ui),
            card(
                Icon::Globe,
                t!("settings.language"),
                None,
                choice(
                    "language",
                    &[
                        (Language::System, t!("settings.language_system")),
                        // Language names are written in their own language, so every locale uses the same strings.
                        (Language::ZhTw, t!("settings.language_zh")),
                        (Language::ZhCn, t!("settings.language_zh_cn")),
                        (Language::En, t!("settings.language_en")),
                        (Language::Ja, t!("settings.language_ja")),
                        (Language::De, t!("settings.language_de")),
                        (Language::Es, t!("settings.language_es")),
                        (Language::PtBr, t!("settings.language_pt_br")),
                    ],
                    s.language,
                    |st, v| st.language = v,
                    ui,
                ),
            ),
        ]);
        let files = vec![
            card(
                Icon::EyeOff,
                t!("settings.show_hidden"),
                None,
                switch("show-hidden", s.show_hidden, |st| {
                    st.show_hidden = !st.show_hidden;
                }),
            ),
            card(
                Icon::EyeOff,
                t!("settings.show_protected"),
                Some(t!("settings.show_protected_desc")),
                switch("show-protected", s.show_protected, |st| {
                    st.show_protected = !st.show_protected;
                }),
            ),
            card(
                Icon::EyeOff,
                t!("settings.hide_dotfiles"),
                Some(t!("settings.hide_dotfiles_desc")),
                switch("hide-dotfiles", s.hide_dotfiles, |st| {
                    st.hide_dotfiles = !st.hide_dotfiles;
                }),
            ),
            card(
                Icon::Check,
                t!("settings.checkboxes"),
                Some(t!("settings.checkboxes_desc")),
                switch("show-checkboxes", s.show_checkboxes, |st| {
                    st.show_checkboxes = !st.show_checkboxes;
                }),
            ),
            single_click_card(s),
            card(
                Icon::FileExt,
                t!("settings.show_extensions"),
                None,
                switch("show-extensions", s.show_extensions, |st| {
                    st.show_extensions = !st.show_extensions;
                }),
            ),
            card(
                Icon::Image,
                t!("settings.show_thumbnails"),
                Some(t!("settings.show_thumbnails_desc")),
                thumbnail_controls(s, cx),
            ),
            card(
                Icon::NetDrive,
                t!("settings.network_thumbnails"),
                Some(t!("settings.network_thumbnails_desc")),
                switch("network-thumbnails", s.network_thumbnails, |st| {
                    st.network_thumbnails = !st.network_thumbnails;
                }),
            ),
            card(
                Icon::Drive,
                t!("settings.folder_sizes"),
                Some(t!("settings.folder_sizes_desc")),
                switch("folder-sizes", s.folder_sizes, |st| {
                    st.folder_sizes = !st.folder_sizes;
                }),
            ),
            card(
                Icon::Trash,
                t!("settings.confirm_recycle"),
                Some(t!("settings.confirm_recycle_desc")),
                switch("confirm-recycle", s.confirm_recycle, |st| {
                    st.confirm_recycle = !st.confirm_recycle;
                }),
            ),
            card(
                Icon::Rename,
                t!("settings.warn_extension"),
                None,
                switch("warn-extension", s.warn_extension_change, |st| {
                    st.warn_extension_change = !st.warn_extension_change;
                }),
            ),
            card(
                Icon::ArrowUp,
                t!("settings.double_click_up"),
                None,
                switch("double-click-up", s.double_click_up, |st| {
                    st.double_click_up = !st.double_click_up;
                }),
            ),
            card(
                Icon::Preview,
                t!("settings.preview"),
                Some(t!("settings.preview_desc")),
                preview_controls(s),
            ),
            card(
                Icon::ArrowLeft,
                t!("settings.nav_scope"),
                Some(t!("settings.nav_scope_desc")),
                choice(
                    "nav-scope",
                    &[
                        (NavScope::AcrossTabs, t!("settings.nav_scope_across")),
                        (NavScope::WithinTab, t!("settings.nav_scope_within")),
                    ],
                    s.nav_scope,
                    |st, v| st.nav_scope = v,
                    ui,
                ),
            ),
            card(
                Icon::ArrowRight,
                t!("settings.swipe_nav"),
                Some(t!("settings.swipe_nav_desc")),
                switch("swipe-nav", s.swipe_nav, |st| {
                    st.swipe_nav = !st.swipe_nav;
                }),
            ),
            // One card: the Recycle Bin, and under it copies, both done by Arke on local drives.
            div()
                .flex()
                .flex_col()
                .child(card(
                    Icon::Trash,
                    t!("settings.fast_recycle"),
                    Some(t!("settings.fast_recycle_desc")),
                    switch("fast-recycle", s.fast_recycle, |st| {
                        st.fast_recycle = !st.fast_recycle;
                    }),
                ))
                .child(sub_row(
                    t!("settings.fast_copy"),
                    switch("fast-copy", s.fast_copy, |st| {
                        st.fast_copy = !st.fast_copy;
                    }),
                ))
                .into_any_element(),
        ];
        let sorting = vec![
            wrapping_card(
                "columns-card",
                Icon::ViewColumns,
                t!("settings.columns"),
                Some(t!("settings.columns_desc")),
                column_toggles(s),
            ),
            card(
                Icon::Info,
                t!("settings.dates"),
                None,
                choice(
                    "dates",
                    &[
                        (false, t!("settings.dates_short")),
                        (true, t!("settings.dates_full")),
                    ],
                    s.full_dates,
                    |st, v| st.full_dates = v,
                    ui,
                ),
            ),
            card(
                Icon::Drive,
                t!("settings.size_units"),
                None,
                choice(
                    "size-units",
                    &[
                        (false, t!("settings.size_units_binary")),
                        (true, t!("settings.size_units_decimal")),
                    ],
                    s.decimal_sizes,
                    |st, v| st.decimal_sizes = v,
                    ui,
                ),
            ),
            card(
                Icon::ViewList,
                t!("settings.default_view"),
                None,
                choice(
                    "default-view",
                    &[
                        (ViewMode::Details, t!("settings.view_details")),
                        (ViewMode::Icons, t!("settings.view_icons")),
                        (ViewMode::Columns, t!("settings.view_columns")),
                    ],
                    s.default_view,
                    |st, v| st.default_view = v,
                    ui,
                ),
            ),
            card(
                Icon::Sort,
                t!("settings.sort_by"),
                // With per-folder memory on, the default reaches folders, not tabs
                Some(if s.remember_folder_views {
                    t!("settings.sort_by_desc_folders")
                } else {
                    t!("settings.sort_by_desc")
                }),
                choice(
                    "sort-by",
                    &[
                        (SortBy::Name, t!("col.name")),
                        (SortBy::Modified, t!("col.modified")),
                        (SortBy::Type, t!("col.type")),
                        (SortBy::Size, t!("col.size")),
                        (SortBy::Created, t!("col.created")),
                    ],
                    s.sort_by,
                    |st, v| st.sort_by = v,
                    ui,
                ),
            ),
            card(
                Icon::ArrowUp,
                t!("settings.sort_direction"),
                None,
                choice(
                    "sort-direction",
                    &[
                        (false, t!("settings.ascending")),
                        (true, t!("settings.descending")),
                    ],
                    s.sort_descending,
                    |st, v| st.sort_descending = v,
                    ui,
                ),
            ),
            card(
                Icon::ViewList,
                t!("settings.group_rows"),
                Some(t!("settings.group_rows_desc")),
                switch("group-rows", s.group_rows, |st| {
                    st.group_rows = !st.group_rows;
                }),
            ),
            card(
                Icon::Subfolders,
                t!("settings.details_tree"),
                Some(t!("settings.details_tree_desc")),
                switch("details-tree", s.details_tree, |st| {
                    st.details_tree = !st.details_tree;
                }),
            ),
            card(
                Icon::Subfolders,
                t!("settings.folders"),
                None,
                choice(
                    "folders",
                    &[
                        (FolderOrder::First, t!("settings.folders_first")),
                        (FolderOrder::Mixed, t!("settings.folders_mixed")),
                    ],
                    s.folders,
                    |st, v| st.folders = v,
                    ui,
                ),
            ),
            folder_views_card(s, cx),
        ];
        let mut startup = vec![
            card(
                Icon::WindowStart,
                t!("settings.start_menu"),
                Some(t!("settings.start_menu_desc")),
                switch("start-menu", s.start_menu, |st| {
                    st.start_menu = !st.start_menu;
                    st.start_menu_offered = true;
                }),
            ),
            card(
                Icon::Bolt,
                t!("settings.autostart"),
                Some(t!("settings.autostart_desc")),
                switch("autostart", s.autostart, |st| {
                    st.autostart = !st.autostart;
                }),
            ),
            card(
                Icon::Tray,
                t!("settings.tray"),
                None,
                switch("tray-icon", s.tray_icon, |st| {
                    st.tray_icon = !st.tray_icon;
                }),
            ),
            card(
                Icon::WindowStart,
                t!("settings.start"),
                None,
                choice(
                    "start",
                    &[
                        (StartLocation::HomePage, t!("home.title")),
                        (StartLocation::Home, t!("settings.start_home")),
                        (StartLocation::LastFolder, t!("settings.start_last")),
                        (StartLocation::LastSession, t!("settings.start_session")),
                    ],
                    s.start_location,
                    |st, v| st.start_location = v,
                    ui,
                ),
            ),
            card(
                Icon::Power,
                t!("settings.keep_running"),
                Some(t!("settings.keep_running_desc")),
                switch("keep-running", s.keep_running, |st| {
                    st.keep_running = !st.keep_running;
                    // Taking over Explorer only works while Arke stays in the background.
                    if !st.keep_running {
                        st.open_folders_in_arke = false;
                    }
                }),
            ),
            card(
                Icon::Folder,
                t!("settings.takeover"),
                Some(t!("settings.takeover_desc")),
                switch("takeover", s.open_folders_in_arke, |st| {
                    st.open_folders_in_arke = !st.open_folders_in_arke;
                    if st.open_folders_in_arke {
                        st.keep_running = true;
                    }
                }),
            ),
            card(
                Icon::Explorer,
                t!("settings.win_e"),
                Some(t!("settings.win_e_desc")),
                switch("win-e", s.win_e_opens_arke, |st| {
                    st.win_e_opens_arke = !st.win_e_opens_arke;
                    // Only means something while Arke opens folders.
                    if st.win_e_opens_arke {
                        st.open_folders_in_arke = true;
                        st.keep_running = true;
                    }
                }),
            ),
            card(
                Icon::TabOpen,
                t!("settings.single_window"),
                Some(t!("settings.single_window_desc")),
                switch("single-window", s.single_window, |st| {
                    st.single_window = !st.single_window;
                }),
            ),
        ];
        // The installer made its own Start menu shortcut; Arke's would only duplicate it.
        if crate::update::installed_by_setup() {
            startup.remove(0);
        }
        let search = search_page(s, cx);
        let route_focused = page.route_input.focus_handle(cx).is_focused(window);
        let update = super::update_page::update_card(
            s,
            page.expanded.contains("update"),
            (&page.route_input, route_focused),
            ui,
            cx,
        );
        let report = super::report_page::report_card(
            page.expanded.contains("report"),
            &page.report_scroll,
            &page.report_scrollbar,
            cx,
        );
        let backup_open = page.expanded.contains("backup");
        let backup =
            super::backup::backup_body(page.backup.as_ref(), &page.import_input, window, cx);
        let about = vec![
            card(
                Icon::FileExt,
                t!("settings.settings_file"),
                crate::settings::default_path().map(|p| Cow::Owned(p.display().to_string())),
                text_button(
                    "reveal-settings",
                    t!("settings.open_folder"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.reveal_settings_file(window, cx);
                    }),
                ),
            ),
            update,
            card(
                Icon::Bolt,
                t!("whats_new.title"),
                Some(t!("whats_new.desc")),
                text_button(
                    "whats-new-show",
                    t!("whats_new.open"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_whats_new(super::whats_new_page::Mode::Browse, window, cx);
                    }),
                ),
            ),
            card(
                Icon::Bolt,
                t!("whats_new.auto"),
                Some(t!("whats_new.auto_desc")),
                switch("whats-new-auto", s.whats_new_auto, |st| {
                    st.whats_new_auto = !st.whats_new_auto;
                }),
            ),
            report,
            expander(
                "backup",
                Icon::Restore,
                t!("backup.title"),
                t!("backup.desc"),
                super::backup::backup_summary(),
                backup_open,
                backup,
                cx,
            ),
            super::uninstall_page::uninstall_card(cx),
        ];
        // Theme and colors first (material is Windows 11 only); the last five are layout.
        let layout = appearance.split_off(appearance.len() - 5);
        let appearance = vec![
            group(Some(t!("settings.group_colors")), appearance),
            group(Some(t!("settings.group_layout")), layout),
        ];
        let mut files = regroup(
            files,
            &[
                (Some(t!("settings.group_show")), &[0, 2, 1, 5, 6, 7, 8]),
                (
                    Some(t!("settings.group_behavior")),
                    &[4, 3, 11, 12, 13, 14, 15],
                ),
                (Some(t!("settings.group_confirm")), &[9, 10]),
            ],
        );
        files.push(group(Some(t!("settings.group_file_ops")), file_ops_rows(s)));
        let sorting = regroup(
            sorting,
            &[
                (Some(t!("settings.group_new_tabs")), &[3, 4, 5, 8]),
                (Some(t!("settings.group_folder_views")), &[9]),
                (Some(t!("settings.group_details")), &[0, 1, 2, 6, 7]),
            ],
        );
        let startup = regroup(startup, &[(None, &[3, 4, 5, 6, 7, 0, 1, 2])]);
        let about = vec![about_header(), group(None, about)];
        vec![
            (t!("settings.appearance"), Icon::Theme, appearance),
            (t!("settings.files"), Icon::FileExt, files),
            (t!("settings.sorting"), Icon::Sort, sorting),
            (t!("settings.startup"), Icon::Power, startup),
            (t!("settings.search"), Icon::Search, search),
            (
                t!("settings.context_menu"),
                Icon::Menu,
                super::menuprefs::menu_section(s, &page.app_input, window, cx),
            ),
            (
                t!("settings.shortcuts"),
                Icon::Bolt,
                super::shortcuts::section(&s.shortcuts, &page.shortcuts, window, cx),
            ),
            (t!("settings.about"), Icon::Info, about),
        ]
    }

    /// Checks again whether QuickLook is running or installed, in the background, and redraws if
    /// that changes what Settings offers. Cheap (see `quicklook.rs`), so it runs on every window
    /// activation.
    pub(super) fn recheck_quicklook(cx: &mut Context<Self>) {
        // A thread of its own: the search may set up COM to read shortcuts.
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("quicklook-recheck", move || {
            let _ = tx.send(crate::quicklook::recheck());
        });
        cx.spawn(async move |this, cx| {
            if rx.recv_async().await.unwrap_or(false) {
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
    }

    /// "Open folder": selects the settings file in Windows Explorer, on a background thread.
    /// Deletes the thumbnail cache in the background, then redraws (the button goes away).
    fn clear_thumbnail_cache(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let cleared = cx
                .background_spawn(async { crate::media::thumb_cache().clear() })
                .await;
            if let Err(err) = cleared {
                log::warn!("clearing the thumbnail cache: {err}");
            }
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// Closes Settings and shows the settings file, selected, in the active tab (not in File
    /// Explorer: someone who opens folders in Arke expects Arke here too).
    fn reveal_settings_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = crate::settings::default_path() else {
            return;
        };
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        self.close_settings(&super::settings_panel::CloseSettings, window, cx);
        let name = Arc::from(name.to_string_lossy().as_ref());
        let dir = dir.to_path_buf();
        self.active_view().clone().update(cx, |v, cx| {
            v.navigate_selecting(dir, arke_core::NavKind::New, Some(name), cx);
        });
    }
}

/// "Show thumbnails": the switch, with "Clear cache (size)" beside it while the cache holds
/// anything (the size is counted at startup, off the UI thread).
fn thumbnail_controls(s: &crate::settings::Settings, cx: &mut Context<Workspace>) -> AnyElement {
    let toggle = switch("show-thumbnails", s.show_thumbnails, |st| {
        st.show_thumbnails = !st.show_thumbnails;
    });
    let Some(bytes) = crate::media::thumb_cache()
        .counted_disk_bytes()
        .filter(|&b| b > 0)
    else {
        return toggle;
    };
    let size = arke_core::format::format_size_base(bytes, s.decimal_sizes);
    div()
        .flex()
        .items_center()
        .gap(theme::s_12())
        .child(text_button(
            "clear-thumbnail-cache",
            t!("settings.clear_thumbnail_cache", size = size),
            cx.listener(|_, _: &ClickEvent, _, cx| Workspace::clear_thumbnail_cache(cx)),
        ))
        .child(toggle)
        .into_any_element()
}

/// Single-click opening, and under it while on, whether resting the pointer selects (#46).
fn single_click_card(s: &Settings) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(card(
            Icon::ArrowRight,
            t!("settings.single_click"),
            Some(t!("settings.single_click_desc")),
            switch("single-click", s.single_click, |st| {
                st.single_click = !st.single_click;
            }),
        ))
        .when(s.single_click, |d| {
            d.child(sub_row(
                t!("settings.point_to_select"),
                switch("point-to-select", s.point_to_select, |st| {
                    st.point_to_select = !st.point_to_select;
                }),
            ))
        })
        .into_any_element()
}

/// A line under a card's title row for one more of its choices: lined up with the title, without
/// an icon of its own.
fn sub_row(title: Cow<'static, str>, control: AnyElement) -> gpui::Div {
    let a11y = crate::a11y::active();
    let title = SharedString::from(title.into_owned());
    div()
        .pl(theme::s_16() + icon_col() + theme::s_16())
        .pr(theme::s_16())
        .pb(theme::s_12())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .when(a11y, |d| d.child(crate::a11y::label_scope(title.clone())))
        .child(row_text(title, None))
        .child(div().flex_none().child(control))
        .when(a11y, |d| d.child(crate::a11y::end_label_scope()))
}

/// Tabs: where they go, then lines under it for one tab per folder, what Ctrl+T and closing the
/// last tab do (#90), and whether the collapsed sidebar opens on a rest, all in one card.
fn tabs_card(s: &Settings, ui: &Ui) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(card(
            Icon::TabsSide,
            t!("settings.tabs"),
            None,
            choice(
                "tabs",
                &[
                    (TabLayout::Vertical, t!("settings.tabs_side")),
                    (TabLayout::Horizontal, t!("settings.tabs_top")),
                ],
                s.tab_layout,
                |st, v| st.tab_layout = v,
                ui,
            ),
        ))
        .child(sub_row(
            t!("settings.one_tab_per_folder"),
            switch("one-tab-per-folder", s.one_tab_per_folder, |st| {
                st.one_tab_per_folder = !st.one_tab_per_folder;
            }),
        ))
        .child(sub_row(
            t!("settings.new_tab"),
            choice(
                "new-tab",
                &[
                    (NewTabAction::GoToFolder, t!("settings.new_tab_box")),
                    (NewTabAction::Home, t!("settings.new_tab_home")),
                    (NewTabAction::CurrentFolder, t!("settings.new_tab_here")),
                ],
                s.new_tab,
                |st, v| st.new_tab = v,
                ui,
            ),
        ))
        .child(sub_row(
            t!("settings.last_tab"),
            choice(
                "last-tab",
                &[
                    (LastTabAction::CloseWindow, t!("settings.last_tab_close")),
                    (LastTabAction::Home, t!("settings.last_tab_home")),
                ],
                s.last_tab,
                |st, v| st.last_tab = v,
                ui,
            ),
        ))
        // Also on the right-click menu of the collapsed sidebar; off by default.
        .child(sub_row(
            t!("settings.sidebar_peek"),
            switch("sidebar-peek", s.sidebar_peek, |st| {
                st.sidebar_peek = !st.sidebar_peek;
            }),
        ))
        .child(sub_row(
            t!("settings.sidebar_follow"),
            switch("sidebar-follow", s.sidebar_follow, |st| {
                st.sidebar_follow = !st.sidebar_follow;
            }),
        ))
        .into_any_element()
}

/// The preview pane switch, and when QuickLook (QL-Win) is running or installed, a chip beside it
/// that hands Space to QuickLook (kept in this row rather than a row of its own).
fn preview_controls(s: &crate::settings::Settings) -> AnyElement {
    let toggle = switch("preview", s.preview_pane, |st| {
        st.preview_pane = !st.preview_pane;
    });
    if !crate::quicklook::available() {
        return toggle;
    }
    let on = s.space_quicklook;
    let tip = SharedString::from(match crate::quicklook::exe() {
        Some(exe) => t!("settings.space_quicklook_tip", path = exe.display()).into_owned(),
        None => t!("settings.space_quicklook_tip_running").into_owned(),
    });
    let chip = div()
        .id("space-quicklook")
        .a11y_toggle(tip.clone(), on)
        .key_stop()
        .h(theme::s_24())
        .px(theme::s_12())
        .flex()
        .items_center()
        .rounded(theme::R_FULL)
        .border_1()
        .cursor_pointer()
        .text_size(theme::fs_12())
        .tooltip(super::tooltip(tip))
        .when(on, |d| {
            d.bg(theme::signal_wash())
                .border_color(theme::signal_line())
                .text_color(theme::text())
        })
        .when(!on, |d| {
            d.border_color(theme::line_strong())
                .text_color(theme::text_subtle())
                .hover(|s| s.bg(theme::wash()))
        })
        .child(t!("settings.space_quicklook").into_owned())
        .on_click(|_, _, cx| set(cx, |st| st.space_quicklook = !st.space_quicklook));
    div()
        .flex()
        .items_center()
        .gap(theme::s_12())
        .child(chip)
        .child(toggle)
        .into_any_element()
}

/// About header: the app mark, the name and the version. The only place the version is shown.
fn about_header() -> AnyElement {
    div()
        .pt(theme::s_8())
        .pb(theme::s_16())
        .px(theme::s_4())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .child(crate::brand::about_mark())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(theme::s_2())
                .child(
                    div()
                        .text_size(theme::fs_20())
                        .font_weight(theme::FW_STRONG)
                        .child("Arke"),
                )
                .child(
                    div()
                        .text_size(theme::fs_13())
                        .child(t!("settings.about_tagline").into_owned()),
                )
                .child(
                    div()
                        .text_size(theme::fs_13())
                        .text_color(theme::text_muted())
                        .child(
                            t!(
                                "settings.version_line",
                                version = crate::build_info::describe()
                            )
                            .into_owned(),
                        ),
                )
                .child(about_legal()),
        )
        .into_any_element()
}

/// The license line under the version, with links to the source code and the license texts.
fn about_legal() -> AnyElement {
    div()
        .pt(theme::s_4())
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(theme::s_12())
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .child(t!("settings.about_legal").into_owned())
        .child(about_link(
            "about-source",
            t!("settings.source_code"),
            |cx| super::update_page::open_url(crate::build_info::SOURCE_URL, cx),
        ))
        .child(about_link(
            "about-licenses",
            t!("settings.licenses"),
            |_| open_licenses(),
        ))
        .into_any_element()
}

fn about_link(
    id: &'static str,
    label: Cow<'static, str>,
    on_click: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let name = label.to_string();
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y(gpui::Role::Link, name)
        .key_stop()
        .cursor_pointer()
        .text_color(theme::signal())
        .hover(Styled::underline)
        .child(label.into_owned())
        .on_click(move |_, _, cx| on_click(cx))
        .into_any_element()
}

/// "Licenses": writes the embedded license texts to `%LOCALAPPDATA%\Arke` as a `.txt`, which every
/// PC can open (Notepad by default), and opens it. On a background thread.
fn open_licenses() {
    crate::explorer::spawn_thread("open-licenses", || {
        let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Arke"))
        else {
            return;
        };
        let path = dir.join("THIRD_PARTY_LICENSES.txt");
        let written = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&path, crate::build_info::THIRD_PARTY_LICENSES));
        match written {
            Ok(()) => {
                let _ = std::process::Command::new("rundll32.exe")
                    .arg("url.dll,FileProtocolHandler")
                    .arg(&path)
                    .spawn();
            }
            Err(err) => log::warn!("writing {}: {err}", path.display()),
        }
    });
}

/// Groups a page's items by `groups` (item indices per group, in order). Unlisted items are dropped;
/// a bad index is caught by a debug assertion.
fn regroup(
    items: Vec<AnyElement>,
    groups: &[(Option<Cow<'static, str>>, &[usize])],
) -> Vec<AnyElement> {
    let mut items: Vec<Option<AnyElement>> = items.into_iter().map(Some).collect();
    let out = groups
        .iter()
        .map(|(title, picks)| {
            let rows = picks
                .iter()
                .filter_map(|&ix| items.get_mut(ix).and_then(Option::take))
                .collect();
            group(title.clone(), rows)
        })
        .collect();
    debug_assert!(
        items.iter().all(Option::is_none),
        "some settings are not assigned to a group"
    );
    out
}

/// Copy and move engine: the same-drive queue, verify after copy, and TeraCopy (only offered when
/// it is installed; found in the background at startup).
fn file_ops_rows(s: &crate::settings::Settings) -> Vec<AnyElement> {
    let mut rows = vec![
        card(
            Icon::Drive,
            t!("settings.queue_same_drive"),
            Some(t!("settings.queue_same_drive_desc")),
            switch("queue-same-drive", s.queue_same_drive, |st| {
                st.queue_same_drive = !st.queue_same_drive;
            }),
        ),
        card(
            Icon::Check,
            t!("settings.verify_copies"),
            Some(t!("settings.verify_copies_desc")),
            switch("verify-copies", s.verify_copies, |st| {
                st.verify_copies = !st.verify_copies;
            }),
        ),
    ];
    if let Some(exe) = crate::teracopy::found() {
        rows.push(card(
            Icon::Copy,
            t!("settings.use_teracopy"),
            Some(t!("settings.use_teracopy_desc", path = exe.display())),
            switch("use-teracopy", s.use_teracopy, |st| {
                st.use_teracopy = !st.use_teracopy;
            }),
        ));
    }
    rows
}

/// A settings group: small heading, one box, hairlines between rows, as in Windows 11 Settings.
pub(super) fn group(title: Option<Cow<'static, str>>, rows: Vec<AnyElement>) -> AnyElement {
    group_with(title, None, rows)
}

/// [`group`] with a one-line note under the heading (outside the box, so every row in the box
/// keeps the same row format).
pub(super) fn group_with(
    title: Option<Cow<'static, str>>,
    note: Option<Cow<'static, str>>,
    rows: Vec<AnyElement>,
) -> AnyElement {
    let rows = rows.into_iter().enumerate().map(|(ix, row)| {
        div()
            .when(ix > 0, |d| d.border_t_1().border_color(theme::line()))
            .child(row)
    });
    let heading = (title.is_some() || note.is_some()).then(|| {
        div()
            .pt(theme::s_16())
            .pb(theme::s_8())
            .px(theme::s_4())
            .flex()
            .flex_col()
            .gap(theme::s_2())
            .children(title.map(|t| {
                let t = SharedString::from(t.into_owned());
                // A heading screen readers can jump between (#322).
                div()
                    .id(SharedString::from(format!("group-title-{t}")))
                    .a11y(gpui::Role::Heading, t.clone())
                    .aria_level(2)
                    .text_size(theme::fs_13())
                    .font_weight(theme::FW_STRONG)
                    .text_color(theme::text_muted())
                    .child(t)
            }))
            .children(note.map(|n| {
                div()
                    .text_size(theme::fs_12())
                    .text_color(theme::text_subtle())
                    .child(SharedString::from(n.into_owned()))
            }))
    });
    div()
        .flex()
        .flex_col()
        .children(heading)
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(theme::r_8())
                .bg(theme::card())
                .border_1()
                .border_color(theme::line())
                .overflow_hidden()
                .children(rows),
        )
        .into_any_element()
}

/// A settings row: icon, name (and short description), control on the right. Used inside [`group`].
pub(super) fn card(
    icon: Icon,
    title: impl Into<SharedString>,
    desc: Option<Cow<'static, str>>,
    control: AnyElement,
) -> AnyElement {
    let spoken = desc.as_ref().map(|d| SharedString::from(d.to_string()));
    row_with(icon, title.into(), desc.map(row_desc), spoken, control)
}

/// The one row layout every settings page uses: icon column, title over an optional second line,
/// control on the right; at least [`card_min_h()`] tall and vertically centered.
pub(super) fn row(
    icon: Icon,
    title: SharedString,
    desc: Option<AnyElement>,
    control: AnyElement,
) -> AnyElement {
    row_with(icon, title, desc, None, control)
}

/// [`row`] with the second line as text too, which screen readers read with the control.
fn row_with(
    icon: Icon,
    title: SharedString,
    desc: Option<AnyElement>,
    spoken: Option<SharedString>,
    control: AnyElement,
) -> AnyElement {
    // The switch or dropdown on the right is named after the row for screen readers.
    let a11y = crate::a11y::active();
    div()
        .min_h(card_min_h())
        .px(theme::s_16())
        .py(theme::s_12())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .when(a11y, |d| {
            d.child(crate::a11y::described_scope(title.clone(), spoken))
        })
        .child(row_icon(icon, theme::text_muted()))
        .child(row_text(title, desc))
        .child(div().flex_none().child(control))
        .when(a11y, |d| d.child(crate::a11y::end_label_scope()))
        .into_any_element()
}

/// A row whose control is a set of chips: they wrap to more lines as the row narrows, and the
/// title keeps a third of the row, instead of the chips staying on one line and squeezing the
/// title to a letter a line (#313, with a wider fallback font or a narrow window).
fn wrapping_card(
    id: &'static str,
    icon: Icon,
    title: impl Into<SharedString>,
    desc: Option<Cow<'static, str>>,
    control: AnyElement,
) -> AnyElement {
    let title = title.into();
    let a11y = crate::a11y::active();
    let spoken = desc.as_ref().map(|d| SharedString::from(d.to_string()));
    div()
        .debug_selector(move || id.to_owned())
        .min_h(card_min_h())
        .px(theme::s_16())
        .py(theme::s_12())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .when(a11y, |d| {
            d.child(crate::a11y::described_scope(title.clone(), spoken))
        })
        .child(row_icon(icon, theme::text_muted()))
        .child(row_text(title, desc.map(row_desc)).min_w(gpui::relative(1. / 3.)))
        .child(div().min_w_0().child(control))
        .when(a11y, |d| d.child(crate::a11y::end_label_scope()))
        .into_any_element()
}

/// Icon centered in the fixed icon column.
pub(super) fn row_icon(icon: Icon, color: gpui::Rgba) -> gpui::Div {
    div()
        .w(icon_col())
        .flex_none()
        .flex()
        .justify_center()
        .child(
            svg()
                .path(icon.path())
                .flex_none()
                .size(theme::icon())
                .text_color(color),
        )
}

/// Title and an optional second line.
pub(super) fn row_text(title: SharedString, desc: Option<AnyElement>) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(theme::s_2())
        .child(title)
        .children(desc)
}

/// Second line of a row: a short description or status.
pub(super) fn row_desc(text: Cow<'static, str>) -> AnyElement {
    div()
        .text_size(theme::fs_12())
        .text_color(theme::text_subtle())
        .child(SharedString::from(text.into_owned()))
        .into_any_element()
}

/// Dropdown for picking one of a few options. The button shows the current value; the list checks the selected one.
/// While open, an invisible layer covers the rest so an outside click only closes the menu.
pub(super) fn choice<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, Cow<'static, str>)],
    current: T,
    apply: impl Fn(&mut Settings, T) + Clone + 'static,
    ui: &Ui,
) -> AnyElement {
    choice_with(
        id,
        options,
        current,
        move |value, cx| {
            let apply = apply.clone();
            set(cx, move |st| apply(st, value));
        },
        None,
        ui,
    )
}

/// [`choice`] where picking runs `pick` (not necessarily a setting), and `fonts` optionally gives
/// each option's label its own font.
#[allow(
    clippy::too_many_lines,
    reason = "Button, list and outside layer are parts of one control; splitting them makes it harder to read"
)]
pub(super) fn choice_with<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, Cow<'static, str>)],
    current: T,
    pick: impl Fn(T, &mut App) + Clone + 'static,
    fonts: Option<OptionFont>,
    ui: &Ui,
) -> AnyElement {
    let (open, cursor) = match ui.open {
        Some((open, cursor)) if open == id => (true, cursor),
        _ => (false, None),
    };
    let at = options
        .iter()
        .position(|(value, _)| *value == current)
        .unwrap_or(0);
    let label = options
        .iter()
        .find(|(value, _)| *value == current)
        .map_or_default(|(_, label)| label.to_string());
    let label_font = fonts.as_ref().and_then(|font| font(at));
    let workspace = ui.workspace.clone();
    let long = options.len() > LONG_LIST;
    let button = div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_combo(label.clone(), open)
        .key_stop()
        .h(theme::h_ctl())
        .min_w(dropdown_min_w())
        .pl(theme::s_12())
        .pr(theme::s_8())
        .flex()
        .items_center()
        .justify_between()
        .gap(theme::s_12())
        .rounded(theme::r_8())
        .border_1()
        .border_color(theme::line_strong())
        .cursor_pointer()
        .when(open, |d| d.bg(theme::wash_strong()))
        .when(!open, |d| {
            d.bg(theme::wash()).hover(|s| s.bg(theme::wash_strong()))
        })
        .child(
            div()
                .when_some(label_font, gpui::Styled::font_family)
                .child(label),
        )
        .child(
            svg()
                .path(Icon::ChevronRight.path())
                .flex_none()
                .size(theme::icon_sm())
                .text_color(theme::text_subtle())
                .with_transformation(gpui::Transformation::rotate(gpui::radians(
                    std::f32::consts::FRAC_PI_2,
                ))),
        );
    // Opens the list; `keyboard` when a key or a screen reader opened it (the cursor shows).
    let open_list = {
        let workspace = workspace.clone();
        let scroll = ui.list_scroll.clone();
        // Hand the per-option actions and the selected index to the keyboard handling.
        let appliers: Vec<Applier> = options
            .iter()
            .map(|(value, _)| {
                let value = *value;
                let pick = pick.clone();
                std::rc::Rc::new(move |cx: &mut App| pick(value, cx)) as _
            })
            .collect();
        std::rc::Rc::new(move |keyboard: bool, cx: &mut App| {
            // A long list opens scrolled to the current choice.
            if long {
                scroll.scroll_to_item(at, gpui::ScrollStrategy::Center);
            }
            set_dropdown(
                &workspace,
                Some(Dropdown {
                    id,
                    cursor: at,
                    keyboard,
                    options: appliers.clone(),
                    popup: crate::popup::Single::new(),
                }),
                cx,
            );
        })
    };
    let button = button
        .on_click({
            let open_list = std::rc::Rc::clone(&open_list);
            // Opened by Space or Enter: the keyboard cursor shows in the list.
            move |event: &ClickEvent, _, cx| open_list(event.is_keyboard(), cx)
        })
        .a11y_on_expand({
            let workspace = workspace.clone();
            move |want, _, cx| {
                if want && !open {
                    open_list(true, cx);
                } else if !want && open {
                    set_dropdown(&workspace, None, cx);
                }
            }
        });
    // Where each button is, for its list to open under it.
    let buttons = std::rc::Rc::clone(&ui.buttons);
    let button = div().relative().child(button).child(
        canvas(
            move |bounds, _, _| {
                buttons.borrow_mut().insert(id, bounds);
            },
            |_, (), _, _| {},
        )
        // Pinned to the corner: without insets an absolute child sits where it would flow, here
        // below the button, and the list opened a button's height too low.
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    );
    if !open {
        return div().child(button).into_any_element();
    }
    let rows: std::rc::Rc<Vec<(T, SharedString)>> = std::rc::Rc::new(
        options
            .iter()
            .map(|(value, label)| (*value, SharedString::from(label.to_string())))
            .collect(),
    );
    let row = {
        let workspace = workspace.clone();
        move |ix: usize| {
            let (value, label) = rows[ix].clone();
            let on = value == current;
            let font = fonts.as_ref().and_then(|font| font(ix));
            let workspace = workspace.clone();
            let pick = pick.clone();
            div()
                .id((id, ix))
                .debug_selector(move || format!("{id}-{ix}"))
                .a11y(gpui::Role::ListBoxOption, label.clone())
                .aria_selected(on)
                .aria_position_in_set(ix)
                // Keep row height fixed when the list scrolls.
                .flex_none()
                .h(theme::h_ctl())
                .pl(theme::s_8())
                .pr(theme::s_16())
                .flex()
                .items_center()
                .gap(theme::s_8())
                .rounded(theme::r_4())
                .cursor_pointer()
                .hover(|s| s.bg(theme::wash()))
                // Keyboard cursor.
                .when(cursor == Some(ix), |d| d.bg(theme::wash_strong()))
                .when(on, |d| d.font_weight(theme::FW_STRONG))
                .child(
                    svg()
                        .path(Icon::Check.path())
                        .flex_none()
                        .size(theme::icon())
                        .text_color(if on { theme::signal() } else { theme::clear() }),
                )
                .child(match font {
                    // Font list: the name and a sample, both in that font.
                    Some(font) => div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(theme::s_16())
                        .font_family(font)
                        .child(div().min_w_0().truncate().child(label))
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme::text_subtle())
                                .child(t!("settings.font_sample").into_owned()),
                        ),
                    None => div().child(label),
                })
                .on_click(move |_, _, cx| {
                    pick(value, cx);
                    set_dropdown(&workspace, None, cx);
                })
        }
    };
    let count = options.len();
    let row = std::rc::Rc::new(row);
    let list_scroll = ui.list_scroll.clone();
    // At least as wide as its button, like a Windows combo box, so the edges it is lined up with
    // match.
    let min_w = ui
        .buttons
        .borrow()
        .get(id)
        .map_or_else(dropdown_min_w, |b| b.size.width.max(dropdown_min_w()));
    // The list is drawn in a window of its own (`popup.rs`), under the button and past the
    // window's edge when it has to be; `Workspace::sync_dropdown_popup` opens it.
    *ui.panel.borrow_mut() = Some(std::rc::Rc::new(move |_: &mut Window, _: &mut App| {
        let row = std::rc::Rc::clone(&row);
        let list = if long {
            // Only the rows in view are built, each in its own font.
            let height = (theme::h_ctl() * count as f32).min(dropdown_max_h() - theme::s_8());
            div()
                .w(font_list_w())
                .child(
                    uniform_list((id, usize::MAX - 3), count, move |range, _, _| {
                        range.map(|ix| row(ix)).collect::<Vec<_>>()
                    })
                    .track_scroll(&list_scroll)
                    .h(height),
                )
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(theme::s_2())
                .children((0..count).map(|ix| row(ix)))
                .into_any_element()
        };
        div()
            .id((id, usize::MAX - 1))
            .role(gpui::Role::ListBox)
            .aria_size_of_set(count)
            .occlude()
            .max_h(dropdown_max_h())
            .when(!long, gpui::StatefulInteractiveElement::overflow_y_scroll)
            .min_w(min_w)
            .p(theme::s_4())
            .rounded(theme::r_8())
            .bg(theme::overlay())
            .shadow(theme::elev_2())
            .ui_font()
            .text_size(theme::fs_14())
            .text_color(theme::text())
            .child(list)
            .with_animation((id, usize::MAX - 2), anim(), |el, t| {
                crate::motion::slide_in(el, t, 0., -ANIM_SLIDE)
            })
            .into_any_element()
    }));
    let scrim = deferred(
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .id((id, usize::MAX))
                .w(ui.viewport.width)
                .h(ui.viewport.height)
                .occlude()
                .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                    cx.stop_propagation();
                    set_dropdown(&workspace, None, cx);
                }),
        ),
    )
    .with_priority(1);
    div().child(button).child(scrim).into_any_element()
}

/// Expandable card. The whole header (icon, name, description, current value, chevron) is clickable;
/// options appear below when expanded.
#[allow(
    clippy::too_many_arguments,
    reason = "an expander takes its title, state and body parts"
)]
pub(super) fn expander(
    id: &'static str,
    icon: Icon,
    title: impl Into<SharedString>,
    desc: Cow<'static, str>,
    summary: AnyElement,
    open: bool,
    body: AnyElement,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let title: SharedString = title.into();
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .a11y_lazy(gpui::Role::Button, || title.to_string())
                .a11y_node({
                    let desc = desc.to_string();
                    move |node| node.set_description(desc)
                })
                .a11y_open_close(open, cx.entity().downgrade(), move |this, _, cx| {
                    if let Some(page) = &mut this.settings_panel
                        && !page.expanded.remove(id)
                    {
                        page.expanded.insert(id);
                    }
                    cx.notify();
                })
                .key_stop()
                .min_h(card_min_h())
                .px(theme::s_16())
                .py(theme::s_12())
                .flex()
                .items_center()
                .gap(theme::s_16())
                .cursor_pointer()
                .hover(|s| s.bg(theme::wash()))
                .child(row_icon(icon, theme::text_muted()))
                .child(row_text(title, Some(row_desc(desc))))
                .child(div().flex_none().child(summary))
                .child(
                    svg()
                        .path(Icon::ChevronRight.path())
                        .flex_none()
                        .size(theme::icon_sm())
                        .text_color(theme::text_subtle())
                        .with_transformation(gpui::Transformation::rotate(gpui::radians(
                            if open {
                                -std::f32::consts::FRAC_PI_2
                            } else {
                                std::f32::consts::FRAC_PI_2
                            },
                        ))),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(page) = &mut this.settings_panel
                        && !page.expanded.remove(id)
                    {
                        page.expanded.insert(id);
                    }
                    cx.notify();
                })),
        )
        .when(open, |d| {
            d.child(
                div()
                    .border_t_1()
                    .border_color(theme::line())
                    .p(theme::s_16())
                    .child(body)
                    .with_animation((id, 0usize), anim(), |el, t| {
                        crate::motion::slide_in(el, t, 0., -ANIM_SLIDE)
                    }),
            )
        })
        .into_any_element()
}

/// Right side of an expander header: the current value as text.
fn text_summary(label: String) -> AnyElement {
    div()
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .child(label)
        .into_any_element()
}

/// Right side of an expander header: current color and name.
fn look_summary(color: gpui::Rgba, label: String) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(theme::s_8())
        .text_size(theme::fs_12())
        .text_color(theme::text_muted())
        .child(
            div()
                .size(theme::icon())
                .rounded(theme::R_FULL)
                .border_1()
                .border_color(theme::line_strong())
                .bg(color),
        )
        .child(label)
        .into_any_element()
}

/// Colors of a preview tile's mini window.
#[derive(Clone, Copy)]
struct TileLook {
    frame: gpui::Rgba,
    paper: gpui::Rgba,
    /// Selected row label.
    ink: gpui::Rgba,
    /// Sidebar and secondary lines.
    faint: gpui::Rgba,
    accent: gpui::Rgba,
}

impl TileLook {
    /// Current theme with these surfaces (`frame`, `paper`) and accent.
    fn current((frame, paper): (gpui::Rgba, gpui::Rgba), accent: gpui::Rgba) -> Self {
        Self {
            frame,
            paper,
            ink: theme::text_muted(),
            faint: theme::text_disabled(),
            accent,
        }
    }

    /// The light or dark theme, whatever the current one is.
    fn mode(dark: bool, gray: bool) -> Self {
        let look = theme::mode_preview(dark, gray);
        Self {
            frame: look.frame,
            paper: look.paper,
            ink: look.ink,
            faint: look.ink_faint,
            accent: look.accent,
        }
    }
}

/// Accent and tint tiles: preview area width and height.
const LOOK_TILE: (f32, f32) = (118., 62.);
/// Theme tiles are a little larger, since the whole window changes.
const THEME_TILE: (f32, f32) = (120., 80.);

/// A mini window (frame, sidebar, paper with a selected row, text lines and an on switch),
/// `width` wide and as tall as the tile's preview area.
fn mini_window(look: TileLook, width: f32) -> gpui::Div {
    let bar = |width: f32, color: gpui::Rgba| {
        div()
            .h(crate::theme::scaled_px(3.))
            .w(px(width))
            .rounded(theme::R_FULL)
            .bg(color)
    };
    div()
        .w(px(width))
        .h_full()
        .flex_none()
        .p(theme::s_6())
        .flex()
        .gap(theme::s_6())
        .bg(look.frame)
        // Sidebar.
        .child(
            div()
                .w(crate::theme::scaled_px(20.))
                .pt(theme::s_4())
                .flex()
                .flex_col()
                .gap(theme::s_4())
                .child(bar(18., look.faint))
                .child(bar(13., look.faint))
                .child(bar(16., look.faint)),
        )
        // Paper: selected row, two text lines, and an on switch in the corner.
        .child(
            div()
                .flex_1()
                .p(theme::s_6())
                .flex()
                .flex_col()
                .gap(theme::s_4())
                .rounded(theme::r_4())
                .bg(look.paper)
                // Selected row: the accent wash, as in the file list.
                .child(
                    div()
                        .flex()
                        .px(theme::s_4())
                        .py(theme::s_2())
                        .rounded(theme::r_4())
                        .bg(gpui::Rgba {
                            a: 0.2,
                            ..look.accent
                        })
                        .child(bar(34., look.ink)),
                )
                .child(bar(42., look.faint))
                .child(bar(28., look.faint))
                .child(
                    div().flex().justify_end().child(
                        div()
                            .w(crate::theme::scaled_px(16.))
                            .h(crate::theme::scaled_px(8.))
                            .rounded(theme::R_FULL)
                            .bg(look.accent),
                    ),
                ),
        )
}

/// Accent or tint option: a mini window in the current theme plus a label. `surfaces` are the
/// frame and paper colors, `accent` the accent; the selected tile gets an accent border.
fn look_tile(
    id: (&'static str, usize),
    label: String,
    selected: bool,
    surfaces: (gpui::Rgba, gpui::Rgba),
    accent: gpui::Rgba,
    pick: &Pick,
) -> AnyElement {
    let (width, height) = LOOK_TILE;
    let look = TileLook::current(surfaces, accent);
    tile(
        id,
        label,
        selected,
        (width, height),
        mini_window(look, width).into_any_element(),
        pick,
    )
}

/// What picking a tile sets, shared by its click and the arrow keys of its group; whether it is
/// read as focused (its group has the focus and it is the picked one); whether it shows the
/// ring (that, by keyboard).
type Pick = (Apply, bool, bool);

/// Sets what a tile stands for.
type Apply = Rc<dyn Fn(&mut Settings)>;

/// Whether a tile group has the focus, and whether by keyboard.
type Held = (bool, bool);

/// [`Pick`] for a tile setting `apply`, picked or not, in a group `held` as given.
fn pick(apply: impl Fn(&mut Settings) + 'static, (focused, keys): Held, selected: bool) -> Pick {
    (Rc::new(apply), focused && selected, keys && selected)
}

/// Tiles of one choice (theme, accent, tint), as a Windows radio group: one tab stop, the arrows
/// pick the previous or next tile (round from the last to the first), Home and End the first and
/// last, and a screen reader reads the picked tile as focused (#322).
fn radio_group(
    id: &'static str,
    label: Cow<'static, str>,
    focus: &FocusHandle,
    picks: Vec<Apply>,
    selected: Option<usize>,
    tiles: Vec<AnyElement>,
) -> gpui::Stateful<gpui::Div> {
    let count = picks.len();
    div()
        .id(id)
        .a11y(gpui::Role::RadioGroup, label.into_owned())
        .track_focus(focus)
        .key_stop_plain()
        .on_key_down(move |event: &gpui::KeyDownEvent, _, cx| {
            if event.keystroke.modifiers.modified() || count == 0 {
                return;
            }
            let at = selected.unwrap_or(0);
            let to = match event.keystroke.key.as_str() {
                // None picked yet (a custom color): the first.
                "left" | "up" | "right" | "down" if selected.is_none() => 0,
                "left" | "up" => (at + count - 1) % count,
                "right" | "down" => (at + 1) % count,
                "home" => 0,
                "end" => count - 1,
                _ => return,
            };
            cx.stop_propagation();
            let apply = Rc::clone(&picks[to]);
            set(cx, move |s| apply(s));
        })
        .flex()
        .flex_wrap()
        .gap(theme::s_8())
        .children(tiles)
}

/// Tile shared by the theme, accent and tint options: `preview` on top (`size` is its width and
/// height), the label below, an accent border when selected.
fn tile(
    id: (&'static str, usize),
    label: String,
    selected: bool,
    (width, height): (f32, f32),
    preview: AnyElement,
    (apply, claim, ring): &Pick,
) -> AnyElement {
    let apply = Rc::clone(apply);
    let tile = div()
        .id(id)
        .a11y_lazy(gpui::Role::RadioButton, || label.clone())
        .aria_toggled(crate::a11y::toggled(selected))
        .when(
            *claim,
            gpui::StatefulInteractiveElement::aria_active_descendant,
        )
        .w(px(width))
        .flex()
        .flex_col()
        .rounded(theme::r_8())
        .border_2()
        .overflow_hidden()
        .cursor_pointer()
        .when(selected, |d| d.border_color(theme::signal()))
        .when(!selected, |d| {
            d.border_color(theme::line())
                .hover(|s| s.border_color(theme::line_strong()))
        })
        .child(div().h(px(height)).flex().overflow_hidden().child(preview))
        .child(
            div()
                .h(crate::theme::scaled_px(26.))
                .px(theme::s_6())
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme::fs_12())
                .when(selected, |d| d.font_weight(theme::FW_STRONG))
                .when(!selected, |d| d.text_color(theme::text_muted()))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                ),
        )
        .on_click(move |_, _, cx| {
            let apply = Rc::clone(&apply);
            set(cx, move |s| apply(s));
        });
    // The keyboard ring: outside the tile, whose preview covers its inside, and in the text color,
    // as the accent already marks the picked tile. The frame is always there (clear when not
    // shown) and its room is taken back with a negative margin, so nothing moves.
    let room = px(4.);
    div()
        .m(-room)
        .p(px(2.))
        .rounded(theme::r_8() + room)
        .border_2()
        .border_color(if *ring { theme::text() } else { theme::clear() })
        .child(tile)
        .into_any_element()
}

/// Theme options: light, dark, and follow Windows (half light, half dark).
fn theme_body(s: &Settings, focus: &FocusHandle, held: Held) -> AnyElement {
    let (width, height) = THEME_TILE;
    let window = |dark: bool, gray: bool| mini_window(TileLook::mode(dark, gray), width);
    // The dark half shows the right side of the same window, so the two halves line up.
    let half = width / 2.;
    let system = div()
        .flex()
        .child(
            div()
                .w(px(half))
                .h_full()
                .overflow_hidden()
                .child(window(false, false)),
        )
        .child(
            div()
                .w(px(half))
                .h_full()
                .relative()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-half))
                        .child(window(true, s.dark_shade == DarkShade::Gray)),
                ),
        )
        .into_any_element();
    // Dark gray is the dark theme with Arke 0.4's gray; System uses whichever dark was picked last.
    let options = [
        (
            ThemeMode::Light,
            None,
            t!("settings.theme_light"),
            window(false, false).into_any_element(),
        ),
        (
            ThemeMode::Dark,
            Some(DarkShade::Black),
            t!("settings.theme_dark"),
            window(true, false).into_any_element(),
        ),
        (
            ThemeMode::Dark,
            Some(DarkShade::Gray),
            t!("settings.theme_gray"),
            window(true, true).into_any_element(),
        ),
        (ThemeMode::System, None, t!("settings.theme_system"), system),
    ];
    let mut picks = Vec::new();
    let mut at = None;
    let tiles = options
        .into_iter()
        .enumerate()
        .map(|(ix, (mode, shade, label, preview))| {
            let selected = s.theme == mode && shade.is_none_or(|shade| s.dark_shade == shade);
            if selected {
                at = Some(ix);
            }
            let pick = pick(
                move |st| {
                    st.theme = mode;
                    if let Some(shade) = shade {
                        st.dark_shade = shade;
                    }
                },
                held,
                selected,
            );
            picks.push(Rc::clone(&pick.0));
            tile(
                ("theme", ix),
                label.into_owned(),
                selected,
                (width, height),
                preview,
                &pick,
            )
        })
        .collect();
    radio_group("theme-tiles", t!("settings.theme"), focus, picks, at, tiles).into_any_element()
}

/// Right side of the Theme header: the current choice.
fn theme_label(mode: ThemeMode, shade: DarkShade) -> String {
    match (mode, shade) {
        (ThemeMode::System, _) => t!("settings.theme_system"),
        (ThemeMode::Light, _) => t!("settings.theme_light"),
        (ThemeMode::Dark, DarkShade::Black) => t!("settings.theme_dark"),
        (ThemeMode::Dark, DarkShade::Gray) => t!("settings.theme_gray"),
    }
    .into_owned()
}

/// Name of the current accent: the swatch name, or the hex value for a custom color.
fn accent_label(accent: Accent) -> String {
    match accent {
        Accent::Arke => t!("accent.arke").into_owned(),
        Accent::Windows => t!("settings.accent_windows").into_owned(),
        Accent::Color(color) => color_label(color),
    }
}

fn color_label(color: u32) -> String {
    theme::ACCENT_PRESETS
        .iter()
        .find(|&&(_, preset)| preset == color)
        .map_or_else(
            || Accent::hex(color),
            |&(name, _)| accent_name(name).into_owned(),
        )
}

fn tint_label(tint: Tint) -> String {
    match tint {
        Tint::None => t!("tint.none").into_owned(),
        Tint::Accent => t!("tint.accent").into_owned(),
        Tint::Color(color) => color_label(color),
    }
}

/// Configured background tint color (`None` is neutral gray).
fn tint_color(s: &Settings) -> Option<u32> {
    match s.tint {
        Tint::None => None,
        Tint::Accent => Some(accent_color(s.accent)),
        Tint::Color(color) => Some(color),
    }
}

/// Accent options: a preview tile per swatch, plus custom (hue strip and hex input).
fn accent_body(
    s: &Settings,
    input: &Entity<TextInput>,
    focus: &FocusHandle,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let frame = (
        theme::tint_preview(tint_color(s)),
        theme::paper_preview(tint_color(s)),
    );
    let current = s.accent;
    let focused = focus.is_focused(window);
    let held = (focused, focused && window.last_input_was_keyboard());
    let mut options = vec![(
        ("accent", 0),
        t!("accent.arke").into_owned(),
        Accent::Arke,
        theme::accent_default(),
    )];
    options.extend(
        theme::ACCENT_PRESETS
            .iter()
            .enumerate()
            .map(|(ix, &(name, color))| {
                (
                    ("accent", ix + 1),
                    accent_name(name).into_owned(),
                    Accent::Color(color),
                    theme::accent_preview(color),
                )
            }),
    );
    if let Some(system) = crate::win::system_accent() {
        options.push((
            ("accent-windows", 0),
            t!("settings.accent_windows").into_owned(),
            Accent::Windows,
            theme::accent_preview(system),
        ));
    }
    let mut picks = Vec::new();
    let mut at = None;
    let tiles = options
        .into_iter()
        .enumerate()
        .map(|(ix, (id, label, accent, preview))| {
            let selected = current == accent;
            if selected {
                at = Some(ix);
            }
            let pick = pick(move |st| st.accent = accent, held, selected);
            picks.push(Rc::clone(&pick.0));
            look_tile(id, label, selected, frame, preview, &pick)
        })
        .collect();
    let tiles = radio_group(
        "accent-tiles",
        t!("settings.accent"),
        focus,
        picks,
        at,
        tiles,
    );
    let focused = input.focus_handle(cx).is_focused(window);
    let code = div()
        .debug_selector(|| "accent-code".to_owned())
        .w(crate::theme::scaled_px(96.))
        .h(theme::h_ctl_sm())
        .px(theme::s_8())
        .flex()
        .items_center()
        .rounded(theme::r_8())
        .border_1()
        .text_size(theme::fs_12())
        .when(focused, |d| {
            d.bg(theme::surface()).border_color(theme::signal_line())
        })
        .when(!focused, |d| {
            d.bg(theme::wash()).border_color(theme::line())
        })
        .child(input.clone());
    div()
        .flex()
        .flex_col()
        .gap(theme::s_16())
        .child(tiles)
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(theme::s_12())
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_muted())
                        .child(t!("settings.custom").into_owned()),
                )
                .child(hue_strip())
                .child(code),
        )
        .into_any_element()
}

/// Background tint options: neutral gray, follow accent, and a preview tile per swatch.
fn tint_body(s: &Settings, focus: &FocusHandle, held: Held) -> AnyElement {
    let accent = theme::signal();
    let current = s.tint;
    let surfaces = |color: Option<u32>| (theme::tint_preview(color), theme::paper_preview(color));
    let mut options = vec![
        (t!("tint.none").into_owned(), Tint::None, surfaces(None)),
        (
            t!("tint.accent").into_owned(),
            Tint::Accent,
            surfaces(Some(accent_color(s.accent))),
        ),
    ];
    options.extend(theme::ACCENT_PRESETS.iter().map(|&(name, color)| {
        (
            accent_name(name).into_owned(),
            Tint::Color(color),
            surfaces(Some(color)),
        )
    }));
    let mut picks = Vec::new();
    let mut at = None;
    let tiles = options
        .into_iter()
        .enumerate()
        .map(|(ix, (label, tint, look))| {
            let selected = current == tint;
            if selected {
                at = Some(ix);
            }
            let pick = pick(move |st| st.tint = tint, held, selected);
            picks.push(Rc::clone(&pick.0));
            look_tile(("tint", ix), label, selected, look, accent, &pick)
        })
        .collect();
    radio_group("tint-tiles", t!("settings.tint"), focus, picks, at, tiles).into_any_element()
}

/// Font: every installed font, with the default first.
fn font_card(s: &Settings, fonts: &std::rc::Rc<Vec<SharedString>>, ui: &Ui) -> AnyElement {
    let mut options: Vec<(usize, Cow<'static, str>)> = vec![(0, t!("settings.font_default"))];
    options.extend(
        fonts
            .iter()
            .enumerate()
            .map(|(ix, name)| (ix + 1, Cow::Owned(name.to_string()))),
    );
    // Fonts still loading, or the font was removed: show the configured name anyway.
    let mut current = fonts
        .iter()
        .position(|name| name.as_ref() == s.font)
        .map_or(0, |ix| ix + 1);
    if current == 0 && !s.font.is_empty() {
        current = options.len();
        options.push((current, Cow::Owned(s.font.clone())));
    }
    // Each option is drawn in its own font; "Default" in the default UI font.
    let names: std::rc::Rc<Vec<SharedString>> = std::rc::Rc::new(
        options
            .iter()
            .enumerate()
            .map(|(ix, (_, name))| {
                if ix == 0 {
                    SharedString::from(theme::DEFAULT_UI_FONT)
                } else {
                    SharedString::from(name.to_string())
                }
            })
            .collect(),
    );
    let font_of: OptionFont = {
        let names = Rc::clone(&names);
        std::rc::Rc::new(move |ix: usize| names.get(ix).cloned())
    };
    // Live sample under the name, in the chosen font.
    let sample = div()
        .text_size(theme::fs_13())
        .text_color(theme::text_muted())
        .when_some(names.get(current).cloned(), gpui::Styled::font_family)
        .child(t!("settings.font_sample").into_owned())
        .into_any_element();
    let fonts = Rc::clone(fonts);
    row(
        Icon::Font,
        t!("settings.font").into_owned().into(),
        Some(sample),
        choice_with(
            "font",
            &options,
            current,
            move |ix: usize, cx| {
                let fonts = Rc::clone(&fonts);
                set(cx, move |st| {
                    if ix == 0 {
                        st.font.clear();
                    } else if let Some(name) = fonts.get(ix - 1) {
                        st.font = name.to_string();
                    }
                });
            },
            Some(font_of),
            ui,
        ),
    )
}

/// Right side of the background image header: a small thumbnail and the file name.
fn background_summary(s: &Settings, page: &SettingsPage) -> AnyElement {
    let name = s
        .background_image
        .as_ref()
        .and_then(|p| p.file_name())
        .map_or_else(
            || t!("settings.background_none").into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
    let image = page.backdrop.as_ref().and_then(|t| t.image.clone());
    div()
        .flex()
        .items_center()
        .gap(theme::s_8())
        .children(image.map(|image| {
            div()
                .w(crate::theme::scaled_px(32.))
                .h(crate::theme::scaled_px(20.))
                .flex_none()
                .rounded(theme::r_4())
                .overflow_hidden()
                .border_1()
                .border_color(theme::line())
                .child(
                    gpui::img(image)
                        .size_full()
                        .object_fit(gpui::ObjectFit::Cover),
                )
        }))
        .child(
            div()
                .max_w(crate::theme::scaled_px(220.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(name),
        )
        .into_any_element()
}

/// Background image: thumbnail of the chosen file with Browse and Clear (or paste a path and
/// press Enter), then opacity with a live preview of the file list paper. Can also be set from an
/// image's context menu.
#[allow(
    clippy::too_many_lines,
    reason = "The card's two parts (choosing the image, then how strongly it shows) are kept together"
)]
fn background_body(
    s: &Settings,
    page: &SettingsPage,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let (thumb_w, thumb_h) = BACKDROP_THUMB;
    let input = &page.image_input;
    let focused = input.focus_handle(cx).is_focused(window);
    let field = div()
        .w_full()
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
        .child(input.clone());
    let thumb = page.backdrop.as_ref();
    let image = thumb.and_then(|t| t.image.clone());
    // Thumbnail, or what stands in for it: nothing chosen, still decoding, or unreadable.
    let placeholder = |icon: Icon, text: Option<String>| {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme::s_4())
            .bg(theme::wash())
            .text_size(theme::fs_12())
            .text_color(theme::text_subtle())
            .child(
                svg()
                    .path(icon.path())
                    .size(theme::s_20())
                    .text_color(theme::text_disabled()),
            )
            .children(text)
            .into_any_element()
    };
    let thumbnail = match (s.background_image.is_some(), &image, thumb) {
        (false, _, _) => placeholder(
            Icon::Image,
            Some(t!("settings.background_none").into_owned()),
        ),
        (true, Some(image), _) => gpui::img(Arc::clone(image))
            .size_full()
            .object_fit(gpui::ObjectFit::Cover)
            .into_any_element(),
        (true, None, Some(t)) if t.failed => placeholder(
            Icon::Close,
            Some(t!("settings.background_unreadable").into_owned()),
        ),
        (true, None, _) => placeholder(Icon::Image, None),
    };
    let frame = |child: AnyElement| {
        div()
            .w(px(thumb_w))
            .h(px(thumb_h))
            .flex_none()
            .rounded(theme::r_8())
            .overflow_hidden()
            .border_1()
            .border_color(theme::line_strong())
            .child(child)
    };
    let browse = text_button(
        "background-browse",
        t!("settings.browse"),
        cx.listener(|this, _: &ClickEvent, window, cx| {
            this.browse(Browse::BackgroundImage, window, cx);
        }),
    );
    let chooser = div()
        .flex()
        .items_center()
        .gap(theme::s_16())
        .child(frame(thumbnail))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .child(field)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::s_8())
                        .child(browse)
                        .when(s.background_image.is_some(), |d| {
                            d.child(text_button(
                                "background-clear",
                                t!("settings.background_clear"),
                                |_, _, cx| set(cx, |st| st.background_image = None),
                            ))
                        }),
                )
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(t!("settings.background_hint").into_owned()),
                ),
        );
    let opacity = s.background_opacity.min(100);
    // Live preview: the file list paper with the image at this opacity under a few rows.
    let preview = {
        let line = |width: f32, color: gpui::Rgba| {
            div()
                .h(crate::theme::scaled_px(4.))
                .w(px(width))
                .rounded(theme::R_FULL)
                .bg(color)
        };
        let row = |selected: bool, width: f32| {
            div()
                .h(crate::theme::scaled_px(16.))
                .px(theme::s_6())
                .flex()
                .items_center()
                .gap(theme::s_6())
                .rounded(theme::r_4())
                .when(selected, |d| d.bg(theme::selection()))
                .child(
                    div()
                        .size(crate::theme::scaled_px(8.))
                        .rounded(theme::r_4())
                        .bg(theme::text_disabled()),
                )
                .child(line(
                    width,
                    if selected {
                        theme::text_muted()
                    } else {
                        theme::text_disabled()
                    },
                ))
        };
        div()
            .size_full()
            .relative()
            .bg(theme::surface())
            .children(image.map(|image| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .opacity(f32::from(opacity) / 100.)
                    .child(
                        gpui::img(image)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Cover),
                    )
            }))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .p(theme::s_8())
                    .flex()
                    .flex_col()
                    .gap(theme::s_4())
                    .child(row(false, 70.))
                    .child(row(true, 52.))
                    .child(row(false, 84.))
                    .child(row(false, 60.))
                    .child(row(false, 76.)),
            )
            .into_any_element()
    };
    let strength = div()
        .flex()
        .items_center()
        .gap(theme::s_16())
        .child(frame(preview))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::s_8())
                .child(t!("settings.background_opacity").into_owned())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::s_12())
                        .child(opacity_slider(opacity, Rc::clone(&page.slider_drag)))
                        .child(
                            div()
                                .w(crate::theme::scaled_px(40.))
                                .text_color(theme::text_muted())
                                .child(format!("{opacity}%")),
                        ),
                )
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(t!("settings.background_preview").into_owned()),
                ),
        );
    div()
        .flex()
        .flex_col()
        .gap(theme::s_16())
        .child(chooser)
        .child(div().h(px(1.)).bg(theme::line()))
        .child(strength)
        .into_any_element()
}

/// Opacity slider, 0 to 100 %. Press anywhere on it to jump there, drag to adjust; the setting
/// (and the list behind) follows while dragging.
fn opacity_slider(value: u8, dragging: std::rc::Rc<std::cell::Cell<bool>>) -> AnyElement {
    let value = value.min(100);
    let track = crate::slider::slider(
        f32::from(value) / 100.,
        dragging,
        // 0 to 1 in, 0 to 100 out: the cast can't truncate.
        std::rc::Rc::new(|fraction: f32, _: &mut Window, cx: &mut App| {
            set_backdrop_opacity((fraction * 100.).round() as u8, cx);
        }),
    )
    .w(slider_w());
    // A tab stop of its own: Left and Right move it by 5, Page Up and Page Down by 20, Home and
    // End to either end, as a Windows slider (#322).
    div()
        .id("backdrop-opacity")
        .a11y(
            gpui::Role::Slider,
            t!("settings.background_opacity").into_owned(),
        )
        .a11y_node(move |node| {
            node.set_numeric_value(f64::from(value));
            node.set_min_numeric_value(0.);
            node.set_max_numeric_value(100.);
            node.set_numeric_value_step(5.);
        })
        .key_stop()
        .rounded(theme::r_8())
        .on_key_down(move |event: &gpui::KeyDownEvent, _, cx| {
            if event.keystroke.modifiers.modified() {
                return;
            }
            let to = match event.keystroke.key.as_str() {
                "left" | "down" => value.saturating_sub(5),
                "right" | "up" => value.saturating_add(5).min(100),
                "pagedown" => value.saturating_sub(20),
                "pageup" => value.saturating_add(20).min(100),
                "home" => 0,
                "end" => 100,
                _ => return,
            };
            cx.stop_propagation();
            set_backdrop_opacity(to, cx);
        })
        .child(track)
        .into_any_element()
}

fn set_backdrop_opacity(value: u8, cx: &mut App) {
    if SettingsStore::global(cx)
        .read(cx)
        .settings
        .background_opacity
        != value
    {
        set(cx, |st| st.background_opacity = value);
    }
}

/// Visible columns: independently toggled chips (accent colored when on). The header's right-click
/// menu flips the same settings.
fn column_toggles(s: &Settings) -> AnyElement {
    use crate::prefs::Col;
    let chip = |col: Col| {
        let on = col.enabled_in(s);
        div()
            .id(("col-chip", col as usize))
            .a11y_toggle(col.label().into_owned(), on)
            .key_stop()
            .h(theme::s_24())
            .px(theme::s_12())
            .flex()
            .items_center()
            .rounded(theme::R_FULL)
            .border_1()
            .cursor_pointer()
            .text_size(theme::fs_12())
            .when(on, |d| {
                d.bg(theme::signal_wash())
                    .border_color(theme::signal_line())
                    .text_color(theme::text())
            })
            .when(!on, |d| {
                d.border_color(theme::line_strong())
                    .text_color(theme::text_subtle())
                    .hover(|s| s.bg(theme::wash()))
            })
            .child(SharedString::from(col.label().into_owned()))
            .on_click(move |_, _, cx| {
                set(cx, |st| {
                    if let Some(on) = col.setting(st) {
                        *on = !*on;
                    }
                });
            })
    };
    div()
        .debug_selector(|| "column-chips".to_owned())
        .flex()
        .flex_wrap()
        .justify_end()
        .gap(theme::s_6())
        .children(Col::ALL.into_iter().filter(|&c| c != Col::Name).map(chip))
        .into_any_element()
}

/// Swatch name, shown as the hover tooltip.
fn accent_name(name: &str) -> Cow<'static, str> {
    match name {
        "blue" => t!("accent.blue"),
        "indigo" => t!("accent.indigo"),
        "purple" => t!("accent.purple"),
        "pink" => t!("accent.pink"),
        "red" => t!("accent.red"),
        "amber" => t!("accent.amber"),
        "green" => t!("accent.green"),
        "teal" => t!("accent.teal"),
        _ => t!("accent.graphite"),
    }
}

/// Hue strip: 36 cells, click to pick that hue.
fn hue_strip() -> impl IntoElement {
    div()
        .flex()
        .rounded(theme::R_FULL)
        .overflow_hidden()
        .children((0..HUE_STEPS).map(|ix| {
            let color = theme::accent_from_hue(ix as f32 * HUE_STEP);
            div()
                .id(("accent-hue", ix))
                .w(crate::theme::scaled_px(8.))
                .h(crate::theme::scaled_px(16.))
                .bg(theme::accent_preview(color))
                .cursor_pointer()
                .on_click(move |_, _, cx| set(cx, |s| s.accent = Accent::Color(color)))
        }))
}

/// Switch: accent track with the thumb on the right when on. The one just clicked animates its thumb and color.
pub(crate) fn switch(
    id: &'static str,
    on: bool,
    toggle: impl Fn(&mut Settings) + 'static,
) -> AnyElement {
    switch_with(id, on, toggle, true)
}

/// [`switch`] that Tab doesn't stop at and a click doesn't focus, for a flyout over the folder:
/// the keys stay with the folder (the view options' tree switch).
pub(crate) fn flyout_switch(
    id: &'static str,
    on: bool,
    toggle: impl Fn(&mut Settings) + 'static,
) -> AnyElement {
    switch_with(id, on, toggle, false)
}

fn switch_with(
    id: &'static str,
    on: bool,
    toggle: impl Fn(&mut Settings) + 'static,
    stop: bool,
) -> AnyElement {
    // Track inner width is 32 (40 minus border and padding) and the thumb is 14, so it travels 18.
    const TRAVEL: f32 = 18.;
    let track_color = |on: bool| {
        if on {
            theme::signal()
        } else {
            theme::wash_strong()
        }
    };
    let thumb_color = |on: bool| {
        if on {
            theme::on_signal()
        } else {
            theme::text_muted()
        }
    };
    let playing = LAST_TOGGLE
        .get()
        .filter(|(last, _)| *last == id && crate::motion::enabled())
        .map(|(_, serial)| serial);
    // Animation progress 0 to 1; on moves right (0 to 1), off moves left (1 to 0).
    let place = move |t: f32| if on { t } else { 1. - t };
    let thumb = div()
        .size(crate::theme::scaled_px(14.))
        .rounded(theme::R_FULL);
    let thumb = match playing {
        Some(serial) => thumb
            .with_animation(
                ("switch-thumb", serial),
                crate::motion::fast(),
                move |el, t| {
                    let at = place(t);
                    el.ml(px(TRAVEL * at)).bg(theme::blend(
                        thumb_color(false),
                        thumb_color(true),
                        at,
                    ))
                },
            )
            .into_any_element(),
        None => thumb
            .ml(px(if on { TRAVEL } else { 0. }))
            .bg(thumb_color(on))
            .into_any_element(),
    };
    let track = div()
        .id(id)
        .a11y_switch(on)
        .when(stop, KeyStop::key_stop)
        .debug_selector(|| id.to_owned())
        .w(crate::theme::scaled_px(40.))
        .h(crate::theme::scaled_px(20.))
        .flex()
        .items_center()
        .px(crate::theme::scaled_px(3.))
        .rounded(theme::R_FULL)
        .cursor_pointer()
        .border_1()
        .border_color(if on {
            theme::clear()
        } else {
            theme::line_strong()
        })
        .child(thumb)
        .on_click(move |_, _, cx| {
            let serial = LAST_TOGGLE.get().map_or(0, |(_, serial)| serial + 1);
            LAST_TOGGLE.set(Some((id, serial)));
            set(cx, &toggle);
        });
    match playing {
        Some(serial) => track
            .with_animation(
                ("switch-track", serial),
                crate::motion::fast(),
                move |el, t| {
                    el.bg(theme::blend(
                        track_color(false),
                        track_color(true),
                        place(t),
                    ))
                },
            )
            .into_any_element(),
        None => track.bg(track_color(on)).into_any_element(),
    }
}

/// "Remember view for each folder": the switch, and next to it a way to forget every folder.
/// The button only shows while something is remembered, so pressing it visibly does something.
fn folder_views_card(s: &Settings, cx: &mut App) -> AnyElement {
    let remembered = crate::folder_views::FolderViewStore::global(cx)
        .read(cx)
        .len();
    let reset = if remembered > 0 {
        text_button(
            "reset-folder-views",
            t!("settings.folder_views_reset"),
            |_: &ClickEvent, _, cx: &mut App| {
                let store = crate::folder_views::FolderViewStore::global(cx);
                store.update(cx, crate::folder_views::FolderViewStore::clear);
                // The settings page doesn't observe the memory; redraw so the button goes away
                cx.refresh_windows();
            },
        )
    } else {
        div()
            .text_size(theme::fs_12())
            .text_color(theme::text_subtle())
            .child(t!("settings.folder_views_none").to_string())
            .into_any_element()
    };
    card(
        Icon::Folder,
        t!("settings.folder_views"),
        Some(t!("settings.folder_views_desc")),
        div()
            .flex()
            .items_center()
            .gap(theme::s_12())
            .child(reset)
            .child(switch("folder-views", s.remember_folder_views, |st| {
                st.remember_folder_views = !st.remember_folder_views;
            }))
            .into_any_element(),
    )
}

/// "Rank by what you open": the switch, and a way to forget the history (only while there is
/// one, so pressing it visibly does something).
fn search_history_card(s: &Settings, cx: &mut App) -> AnyElement {
    let remembered = crate::search_history::SearchHistory::global(cx)
        .read(cx)
        .len();
    let clear = if remembered > 0 {
        text_button(
            "clear-search-history",
            t!("settings.search_history_clear"),
            |_: &ClickEvent, _, cx: &mut App| {
                let store = crate::search_history::SearchHistory::global(cx);
                store.update(cx, crate::search_history::SearchHistory::clear);
                // The settings page doesn't observe the history; redraw so the button goes away
                cx.refresh_windows();
            },
        )
    } else {
        div()
            .text_size(theme::fs_12())
            .text_color(theme::text_subtle())
            .child(t!("settings.search_history_none").to_string())
            .into_any_element()
    };
    card(
        Icon::Sort,
        t!("settings.search_history"),
        Some(t!("settings.search_history_desc")),
        div()
            .flex()
            .items_center()
            .gap(theme::s_12())
            .child(clear)
            .child(switch("search-history", s.search_history, |st| {
                st.search_history = !st.search_history;
            }))
            .into_any_element(),
    )
}

pub(super) fn text_button(
    id: &'static str,
    label: impl std::fmt::Display,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = SharedString::from(label.to_string());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_row_button(label.clone())
        .key_stop()
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
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

/// "Live updates" button: installs or removes the background service (UAC prompt); shows text only while waiting.
/// Same action as the lightning button in the search scope bar, placed here so it is easier to find.
fn service_control(cx: &App) -> AnyElement {
    let (installed, outdated, busy) = crate::global_search::GlobalSearch::global(cx)
        .map_or_default(|g| {
            let g = g.read(cx);
            (
                g.service_installed == Some(true),
                g.service_outdated.is_some(),
                g.service_busy,
            )
        });
    if busy {
        return div()
            .text_size(theme::fs_12())
            .text_color(theme::text_muted())
            .child(t!("search.service_busy").to_string())
            .into_any_element();
    }
    let toggle = text_button(
        "search-service",
        if installed {
            t!("settings.service_remove")
        } else {
            t!("settings.service_install")
        },
        move |_: &ClickEvent, _, cx: &mut App| {
            if let Some(global) = crate::global_search::GlobalSearch::global(cx) {
                global.update(cx, |g, cx| g.set_service(!installed, cx));
                cx.refresh_windows();
            }
        },
    );
    if !(installed && outdated) {
        return toggle;
    }
    // Installing again copies this Arke's program over the service's and keeps its settings.
    let update = text_button(
        "search-service-update",
        t!("settings.service_update"),
        |_: &ClickEvent, _, cx: &mut App| {
            if let Some(global) = crate::global_search::GlobalSearch::global(cx) {
                global.update(cx, |g, cx| g.set_service(true, cx));
                cx.refresh_windows();
            }
        },
    );
    div()
        .flex()
        .gap(theme::s_8())
        .child(update)
        .child(toggle)
        .into_any_element()
}

/// The service row's description: what it does, or that it's from another version of Arke.
fn service_desc(cx: &App) -> Cow<'static, str> {
    let outdated = crate::global_search::GlobalSearch::global(cx).and_then(|g| {
        let g = g.read(cx);
        (g.service_installed == Some(true))
            .then(|| g.service_outdated.clone())
            .flatten()
    });
    if outdated.is_some() {
        t!("settings.service_outdated")
    } else {
        t!("settings.service_desc")
    }
}

/// Above this many indexed items, suggest adding only frequently used subfolders.
const LARGE_NETWORK_INDEX: usize = 1_000_000;

/// Settings › Search. The whole-PC switch comes first; everything else only matters while it is
/// on, so it is dimmed and can't be clicked otherwise.
fn search_page(s: &Settings, cx: &mut Context<Workspace>) -> Vec<AnyElement> {
    let on = s.global_search;
    let main = group(
        None,
        vec![card(
            Icon::Search,
            t!("settings.global_search"),
            Some(t!("settings.global_search_desc")),
            switch("global-search", on, |st| {
                st.global_search = !st.global_search;
            }),
        )],
    );
    let index = group(
        Some(t!("settings.search_index")),
        vec![
            card(
                Icon::Subfolders,
                t!("settings.skip_build_dirs"),
                Some(t!("settings.skip_build_dirs_desc")),
                switch("search-skip-build", s.search_skip_build_dirs, |st| {
                    st.search_skip_build_dirs = !st.search_skip_build_dirs;
                }),
            ),
            card(
                Icon::Bolt,
                t!("settings.service"),
                Some(service_desc(cx)),
                service_control(cx),
            ),
        ],
    );
    let ranking = group(
        Some(t!("settings.search_ranking")),
        vec![search_history_card(s, cx)],
    );
    let scope = group_with(
        Some(t!("settings.search_scope")),
        Some(t!("settings.search_scope_desc")),
        search_scope_rows(s, crate::win::fixed_drive_roots()),
    );
    let network = group_with(
        Some(t!("settings.search_network")),
        Some(t!("settings.search_network_desc")),
        network_search_rows(s, cx),
    );
    let mut out = vec![main];
    out.extend(
        [index, ranking, scope, network]
            .into_iter()
            .map(|section| needs(on, section)),
    );
    out
}

/// A section that only matters while `on`: otherwise dimmed, and a cover takes the clicks. The
/// wheel still goes through, so the page scrolls with the pointer over it.
fn needs(on: bool, section: AnyElement) -> AnyElement {
    if on {
        return section;
    }
    div()
        .relative()
        .child(div().opacity(0.45).child(section))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .block_mouse_except_scroll()
                .cursor_default(),
        )
        .into_any_element()
}

/// Network locations: auto-add, then each searched or excluded location with its status.
/// Locations are added from the sidebar network context menu.
fn network_search_rows(s: &crate::settings::Settings, cx: &App) -> Vec<AnyElement> {
    let global = crate::global_search::GlobalSearch::global(cx);
    let drives = crate::win::network_drive_roots();
    let searched = s.searched_network(&drives);
    let state_of = |root: &PathBuf| -> Option<String> {
        let g = global.as_ref()?.read(cx);
        let n = g.net.iter().find(|n| n.root == *root)?;
        let count = n
            .status
            .count
            .map(|c| tn!("search.index_network_items", c).into_owned());
        Some(match (n.awake, count) {
            (false, Some(count)) => {
                format!(
                    "{count}{}{}",
                    t!("ops.separator"),
                    t!("search.index_network_dormant")
                )
            }
            (false, None) => t!("search.index_network_waiting").into_owned(),
            (true, None) => t!("search.index_network_building").into_owned(),
            (true, Some(count)) if n.offline => {
                format!("{count}{}", t!("search.index_network_unreachable"))
            }
            (true, Some(count)) => count,
        })
        .map(|text| {
            // Very large share: suggest excluding it and adding only common subfolders.
            if n.status.count.is_some_and(|c| c >= LARGE_NETWORK_INDEX) {
                format!(
                    "{text}{}{}",
                    t!("ops.separator"),
                    t!("search.index_network_large")
                )
            } else {
                text
            }
        })
    };
    let mut rows: Vec<AnyElement> = vec![card(
        Icon::NetDrive,
        t!("settings.search_network_auto"),
        None,
        switch("search-net-auto", s.search_network_auto, |st| {
            st.search_network_auto = !st.search_network_auto;
        }),
    )];
    for (ix, root) in searched.iter().enumerate() {
        let target = root.clone();
        rows.push(network_row(
            ("search-net-remove", ix),
            Icon::NetDrive,
            root,
            state_of(root),
            false,
            t!("settings.search_network_skip").into_owned(),
            move |st| {
                st.search_network.retain(|p| *p != target);
                // Auto-added locations go on the exclusion list, otherwise they come back next time.
                if st.search_network_auto && !st.search_network_excluded.contains(&target) {
                    st.search_network_excluded.push(target.clone());
                }
            },
        ));
    }
    for (ix, root) in s.search_network_excluded.iter().enumerate() {
        let target = root.clone();
        rows.push(network_row(
            ("search-net-readd", ix),
            Icon::NetDrive,
            root,
            Some(t!("settings.search_network_excluded").into_owned()),
            true,
            t!("settings.search_network_readd").into_owned(),
            move |st| {
                st.search_network_excluded.retain(|p| *p != target);
                if !st.search_network_auto && !st.search_network.contains(&target) {
                    st.search_network.push(target.clone());
                }
            },
        ));
    }
    if rows.len() == 1 {
        rows.push(row(
            Icon::NetDrive,
            SharedString::from(t!("settings.search_network_empty").into_owned()),
            None,
            div().into_any_element(),
        ));
    }
    rows
}

/// Element ids (must be `'static`), one per drive letter.
macro_rules! drive_ids {
    ($prefix:literal) => {
        [
            concat!($prefix, "A"),
            concat!($prefix, "B"),
            concat!($prefix, "C"),
            concat!($prefix, "D"),
            concat!($prefix, "E"),
            concat!($prefix, "F"),
            concat!($prefix, "G"),
            concat!($prefix, "H"),
            concat!($prefix, "I"),
            concat!($prefix, "J"),
            concat!($prefix, "K"),
            concat!($prefix, "L"),
            concat!($prefix, "M"),
            concat!($prefix, "N"),
            concat!($prefix, "O"),
            concat!($prefix, "P"),
            concat!($prefix, "Q"),
            concat!($prefix, "R"),
            concat!($prefix, "S"),
            concat!($prefix, "T"),
            concat!($prefix, "U"),
            concat!($prefix, "V"),
            concat!($prefix, "W"),
            concat!($prefix, "X"),
            concat!($prefix, "Y"),
            concat!($prefix, "Z"),
        ]
    };
}

/// Search scope switches, one per drive letter.
const DRIVE_SWITCH_IDS: [&str; 26] = drive_ids!("search-drive-");

/// The `'static` id for `root`'s drive letter from `ids`.
fn drive_id(ids: &[&'static str; 26], root: &std::path::Path) -> Option<&'static str> {
    let letter = root.to_string_lossy().chars().next()?.to_ascii_uppercase();
    letter
        .is_ascii_uppercase()
        .then(|| ids[(letter as u8 - b'A') as usize])
}

/// Search scope: a switch per local disk, plus excluded folders (added via the context menu) that can be restored.
fn search_scope_rows(s: &crate::settings::Settings, drives: Vec<PathBuf>) -> Vec<AnyElement> {
    use crate::global_search::same_path;
    let mut rows: Vec<AnyElement> = Vec::new();
    for root in drives {
        let Some(id) = drive_id(&DRIVE_SWITCH_IDS, &root) else {
            continue;
        };
        let on = !s.search_skipped_drives.iter().any(|d| same_path(d, &root));
        let target = root.clone();
        rows.push(row(
            Icon::Drive,
            SharedString::from(root.display().to_string()),
            None,
            switch(id, on, move |st| {
                let before = st.search_skipped_drives.len();
                st.search_skipped_drives.retain(|d| !same_path(d, &target));
                if st.search_skipped_drives.len() == before {
                    st.search_skipped_drives.push(target.clone());
                }
            }),
        ));
    }
    for (ix, folder) in s.search_excluded.iter().enumerate() {
        let target = folder.clone();
        rows.push(network_row(
            ("search-excluded-readd", ix),
            Icon::Folder,
            folder,
            Some(t!("settings.search_network_excluded").into_owned()),
            true,
            t!("settings.search_network_readd").into_owned(),
            move |st| st.search_excluded.retain(|p| !same_path(p, &target)),
        ));
    }
    rows
}

/// Location row in the settings row format: icon, path and status, and an action button (stop
/// searching / restore).
fn network_row(
    id: (&'static str, usize),
    icon: Icon,
    root: &std::path::Path,
    state: Option<String>,
    dimmed: bool,
    action: String,
    apply: impl Fn(&mut Settings) + 'static,
) -> AnyElement {
    let apply = std::rc::Rc::new(apply);
    let button = div()
        .id(id)
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
        .child(action)
        .on_click(move |_: &ClickEvent, _, cx: &mut App| {
            let apply = Rc::clone(&apply);
            set(cx, move |st| apply(st));
        });
    div()
        .min_h(card_min_h())
        .px(theme::s_16())
        .py(theme::s_12())
        .flex()
        .items_center()
        .gap(theme::s_16())
        .child(row_icon(
            icon,
            if dimmed {
                theme::text_disabled()
            } else {
                theme::text_muted()
            },
        ))
        .child(
            row_text(
                SharedString::from(root.display().to_string()),
                state.map(|text| row_desc(Cow::Owned(text))),
            )
            .when(dimmed, |d| d.text_color(theme::text_muted())),
        )
        .child(div().flex_none().child(button))
        .into_any_element()
}
