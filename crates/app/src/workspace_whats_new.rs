//! What's new page, like Home and the Recycle Bin a page owned by one tab. It opens by itself
//! the first time Arke runs after an update (`whats_new.rs` decides when), from Settings › About,
//! and for an update that isn't installed yet (from its notice, the Updates card or the tray).

use std::collections::BTreeSet;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, Global,
    ScrollHandle, Subscription, Window, div, prelude::*, svg,
};
use rust_i18n::t;

use super::Workspace;
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::build_info;
use crate::settings::SettingsStore;
use crate::theme;
use crate::update::{self, UpdateState};
use crate::whats_new::{self, Block, Release, Startup};

/// Why the page is open, which decides its heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Arke was just updated from `seen` (`None`: an older version that didn't record it).
    Updated { seen: Option<String> },
    /// Opened by hand: this version and the ones before.
    Browse,
    /// An update that isn't installed yet, with its notes from GitHub.
    Available(update::Release),
}

/// One version on the page.
struct Section {
    release: Release,
    /// This RC or nightly build's own changes, not in a release yet.
    testing: bool,
}

pub(crate) struct WhatsNewPage {
    mode: Mode,
    /// This build's version.
    version: String,
    sections: Vec<Section>,
    /// Sections shown open; the first one is.
    expanded: BTreeSet<usize>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

pub(crate) enum WhatsNewEvent {
    Close,
}

impl EventEmitter<WhatsNewEvent> for WhatsNewPage {}

impl Focusable for WhatsNewPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WhatsNewPage {
    fn new(mode: Mode, version: String, cx: &mut Context<Self>) -> Self {
        let lang = whats_new::notes_lang();
        let all = whats_new::releases(lang);
        let released = |releases: Vec<Release>| {
            releases.into_iter().map(|release| Section {
                release,
                testing: false,
            })
        };
        let mut sections: Vec<Section> = Vec::new();
        match &mode {
            Mode::Available(release) => {
                sections.push(Section {
                    release: whats_new::available(&release.version, &release.notes),
                    testing: false,
                });
                sections.extend(released(whats_new::since(&all, None, &version)));
            }
            Mode::Updated { seen } => {
                sections.extend(whats_new::this_build(lang).map(|release| Section {
                    release,
                    testing: true,
                }));
                sections.extend(released(whats_new::since(&all, seen.as_deref(), &version)));
            }
            Mode::Browse => {
                sections.extend(whats_new::this_build(lang).map(|release| Section {
                    release,
                    testing: true,
                }));
                sections.extend(released(all));
            }
        }
        // An update's own notes, or the newest version: open; the rest folded.
        let expanded = (!sections.is_empty()).then_some(0).into_iter().collect();
        Self {
            mode,
            version,
            sections,
            expanded,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn versions_for_test(&self) -> Vec<(String, bool)> {
        self.sections
            .iter()
            .enumerate()
            .map(|(ix, s)| (s.release.version.clone(), self.expanded.contains(&ix)))
            .collect()
    }

    fn heading(&self) -> (String, Option<String>) {
        match &self.mode {
            Mode::Updated { seen } => (
                t!("whats_new.updated", version = self.version).into_owned(),
                seen.as_ref()
                    .map(|v| t!("whats_new.since", version = v).into_owned()),
            ),
            Mode::Browse => (
                t!("whats_new.title").into_owned(),
                Some(t!("update.desc", version = self.version).into_owned()),
            ),
            Mode::Available(release) => (
                t!("update.available", version = release.version).into_owned(),
                Some(t!("whats_new.not_installed").into_owned()),
            ),
        }
    }

    /// Download, progress or restart for an update that isn't installed yet.
    fn update_action(&self, cx: &App) -> Option<AnyElement> {
        let Mode::Available(release) = &self.mode else {
            return None;
        };
        let line = |text: String| {
            div()
                .text_size(theme::FS_13)
                .text_color(theme::text_muted())
                .child(text)
                .into_any_element()
        };
        Some(match UpdateState::get(cx) {
            UpdateState::Downloading(r, got, total) if r.version == release.version => {
                line(super::update_page::downloading_text(&r.version, got, total))
            }
            UpdateState::Ready(r, exe) if r.version == release.version => {
                super::update_page::button(
                    "whats-new-restart",
                    t!("update.restart"),
                    true,
                    move |_, _, cx| update::restart(&exe, cx),
                )
            }
            _ => {
                let release = release.clone();
                super::update_page::button(
                    "whats-new-install",
                    t!("update.install"),
                    true,
                    move |_, _, cx| update::start_download(release.clone(), cx),
                )
            }
        })
    }

    fn section(&self, ix: usize, section: &Section, cx: &mut Context<Self>) -> AnyElement {
        let open = self.expanded.contains(&ix);
        let release = &section.release;
        let tag = if section.testing {
            Some(t!("whats_new.testing"))
        } else if ix == 0 && matches!(self.mode, Mode::Available(_)) {
            Some(t!("whats_new.not_installed_tag"))
        } else {
            None
        };
        let header = div()
            .id(("whats-new-section", ix))
            .debug_selector(move || format!("whats-new-section-{ix}"))
            .a11y(gpui::Role::Button, release.version.clone())
            .h(theme::H_ROW)
            .px(theme::S_12)
            .flex()
            .items_center()
            .gap(theme::S_8)
            .cursor_pointer()
            .hover(|s| s.bg(theme::wash()))
            .child(
                svg()
                    .path(Icon::ChevronRight.path())
                    .size(theme::ICON_SM)
                    .text_color(theme::text_muted())
                    .when(open, |s| {
                        s.with_transformation(gpui::Transformation::rotate(gpui::radians(
                            std::f32::consts::FRAC_PI_2,
                        )))
                    }),
            )
            .child(
                div()
                    .font_weight(theme::FW_STRONG)
                    .child(release.version.clone()),
            )
            .children(release.date.clone().map(|d| {
                div()
                    .text_size(theme::FS_12)
                    .text_color(theme::text_muted())
                    .child(d)
            }))
            .children(tag.map(|tag| {
                div()
                    .px(theme::S_6)
                    .rounded(theme::R_FULL)
                    .bg(theme::signal_wash())
                    .text_size(theme::FS_12)
                    .child(tag.into_owned())
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !this.expanded.remove(&ix) {
                    this.expanded.insert(ix);
                }
                cx.notify();
            }));
        let body = open.then(|| {
            div()
                .px(theme::S_16)
                .pb(theme::S_12)
                .flex()
                .flex_col()
                .gap(theme::S_4)
                .children(release.blocks.iter().map(render_block))
                .when(release.blocks.is_empty(), |d| {
                    d.child(
                        div()
                            .text_color(theme::text_muted())
                            .child(t!("whats_new.no_notes").into_owned()),
                    )
                })
        });
        div()
            .rounded(theme::R_8)
            .border_1()
            .border_color(theme::line())
            .overflow_hidden()
            .child(header)
            .children(body)
            .into_any_element()
    }
}

fn render_block(block: &Block) -> AnyElement {
    match block {
        Block::Heading(text) => div()
            .pt(theme::S_8)
            .text_size(theme::FS_12)
            .font_weight(theme::FW_STRONG)
            .text_color(theme::signal())
            .child(whats_new::plain(text))
            .into_any_element(),
        Block::Item(text) => div()
            .flex()
            .gap(theme::S_8)
            .child(div().flex_none().text_color(theme::text_muted()).child("•"))
            .child(div().flex_1().min_w_0().child(whats_new::plain(text)))
            .into_any_element(),
        Block::Text(text) => div()
            .pt(theme::S_4)
            .text_color(theme::text_muted())
            .child(whats_new::plain(text))
            .into_any_element(),
    }
}

impl Render for WhatsNewPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, subtitle) = self.heading();
        let sections: Vec<AnyElement> = self
            .sections
            .iter()
            .enumerate()
            .map(|(ix, s)| self.section(ix, s, cx))
            .collect();
        let changelog = format!(
            "{}/blob/main/{}",
            build_info::SOURCE_URL,
            whats_new::changelog_file(whats_new::notes_lang())
        );
        div()
            .id("whats-new-scroll")
            .key_context("WhatsNewPage")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_key_down(cx.listener(|_, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(WhatsNewEvent::Close);
                }
            }))
            .child(
                div()
                    .w_full()
                    .px(theme::S_32)
                    .pt(theme::S_24)
                    .pb(theme::S_32)
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(gpui::px(760.))
                            .flex()
                            .flex_col()
                            .gap(theme::S_12)
                            .child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap(theme::S_12)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(theme::S_4)
                                            .child(
                                                div()
                                                    .text_size(theme::FS_24)
                                                    .font_weight(theme::FW_STRONG)
                                                    .child(title),
                                            )
                                            .children(subtitle.map(|s| {
                                                div()
                                                    .text_size(theme::FS_13)
                                                    .text_color(theme::text_muted())
                                                    .child(s)
                                            })),
                                    )
                                    .children(self.update_action(cx)),
                            )
                            .children(sections)
                            .child(
                                div()
                                    .pt(theme::S_8)
                                    .flex()
                                    .flex_wrap()
                                    .gap_x(theme::S_16)
                                    .text_size(theme::FS_12)
                                    .text_color(theme::text_muted())
                                    .when(matches!(self.mode, Mode::Updated { .. }), |d| {
                                        d.child(t!("whats_new.reopen").into_owned())
                                    })
                                    .child(link(
                                        "whats-new-changelog",
                                        t!("whats_new.changelog"),
                                        changelog,
                                    )),
                            ),
                    ),
            )
    }
}

fn link(id: &'static str, label: std::borrow::Cow<'static, str>, url: String) -> AnyElement {
    let name = label.to_string();
    div()
        .id(id)
        .a11y(gpui::Role::Link, name)
        .cursor_pointer()
        .text_color(theme::signal())
        .hover(Styled::underline)
        .child(label.into_owned())
        .on_click(move |_, _, cx| super::update_page::open_url(&url, cx))
        .into_any_element()
}

pub(super) struct WhatsNewView {
    view: Entity<WhatsNewPage>,
    /// Tab that owns the page.
    pub(super) owner: super::pages::Owner,
    _events: Subscription,
}

/// Whether this process already decided about opening the page after an update (once, in the
/// first window that has the settings loaded).
#[derive(Default)]
struct StartupDone(bool);

impl Global for StartupDone {}

/// The version and channel to go by: this build's, or what a test pretends to be.
fn build(cx: &App) -> (String, Option<build_info::Channel>) {
    #[cfg(test)]
    if let Some(PretendBuild(version, channel)) = cx.try_global::<PretendBuild>() {
        return (version.clone(), *channel);
    }
    let _ = cx;
    (build_info::VERSION.to_string(), build_info::built_channel())
}

/// Tests run as a dev build; this makes What's new behave as in a release of `.0`.
#[cfg(test)]
struct PretendBuild(String, Option<build_info::Channel>);

#[cfg(test)]
impl Global for PretendBuild {}

impl Workspace {
    /// Whether the active tab is showing What's new.
    pub(super) fn whats_new_open(&self) -> bool {
        self.whats_new
            .as_ref()
            .is_some_and(|p| self.shows(&p.owner))
    }

    /// Opens the page in a new tab (or the tab that already has it, showing `mode`).
    pub(crate) fn open_whats_new(
        &mut self,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.whats_new.as_ref().map(|p| p.owner.clone());
        let (version, _) = build(cx);
        let view = cx.new(|cx| WhatsNewPage::new(mode, version, cx));
        let events = cx.subscribe_in(&view, window, Self::on_whats_new_event);
        if !self.go_to_page(existing.as_ref(), window, cx) {
            self.open_page_tab(window, cx);
        }
        let owner = existing.unwrap_or_else(|| self.owner_here(true, cx));
        self.whats_new = Some(WhatsNewView {
            view,
            owner,
            _events: events,
        });
        if let Some(focus) = self.whats_new_focus(cx) {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn whats_new_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.whats_new.as_ref().map(|p| p.view.focus_handle(cx))
    }

    pub(super) fn close_whats_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.whats_new.take() {
            self.dismiss_page(&page.owner, window, cx);
        }
    }

    fn on_whats_new_event(
        &mut self,
        _: &Entity<WhatsNewPage>,
        event: &WhatsNewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WhatsNewEvent::Close => self.close_whats_new(window, cx),
        }
    }

    /// Called before paint: closes the page once its tab is gone or has navigated elsewhere.
    pub(super) fn whats_new_follow_navigation(&mut self, cx: &App) {
        if self
            .whats_new
            .as_ref()
            .is_some_and(|page| !self.owner_alive(&page.owner, cx))
        {
            self.whats_new = None;
        }
    }

    /// Once per run, as soon as the settings are loaded: after an update, opens the page (and
    /// remembers the version either way).
    pub(super) fn whats_new_on_startup(window: &Window, cx: &mut Context<Self>) {
        if cx.try_global::<StartupDone>().is_some_and(|d| d.0) {
            return;
        }
        let store = SettingsStore::global(cx);
        let (seen, first_run, auto) = {
            let store = store.read(cx);
            if !store.loaded {
                return;
            }
            let s = &store.settings;
            (s.whats_new_seen.clone(), store.first_run, s.whats_new_auto)
        };
        cx.set_global(StartupDone(true));
        let (version, channel) = build(cx);
        let decision = whats_new::on_startup(seen.as_deref(), first_run, &version, channel, auto);
        if decision == Startup::Nothing {
            return;
        }
        store.update(cx, |s, cx| {
            s.update(|st| st.whats_new_seen = Some(version), cx);
        });
        if decision == Startup::Show {
            // After this frame, so the restored tabs are in place first.
            cx.defer_in(window, move |this, window, cx| {
                this.open_whats_new(Mode::Updated { seen }, window, cx);
            });
        }
    }

    /// Runs as a release build of `version` and lets startup decide again, as on a new run.
    #[cfg(test)]
    pub(crate) fn pretend_build_for_test(
        version: &str,
        channel: Option<build_info::Channel>,
        cx: &mut App,
    ) {
        cx.set_global(PretendBuild(version.to_string(), channel));
        cx.set_global(StartupDone(false));
    }

    #[cfg(test)]
    pub(crate) fn whats_new_for_test(&self) -> Option<Entity<WhatsNewPage>> {
        self.whats_new.as_ref().map(|p| p.view.clone())
    }

    /// What's new page for the content area, or `None` when closed.
    pub(super) fn render_whats_new_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let page = self.whats_new.as_ref().filter(|p| self.shows(&p.owner))?;
        let horizontal = Self::horizontal_tabs(cx);
        let top_bar = div()
            .h(theme::H_BAR)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme::S_2)
            .pr(theme::S_8)
            .when(horizontal, |d| {
                d.child(crate::explorer::ghost_button(
                    "whats-new-back",
                    Icon::ArrowLeft,
                    true,
                    t!("nav.back"),
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.close_whats_new(window, cx);
                    }),
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme::S_8)
                    .px(theme::S_8)
                    .child(
                        svg()
                            .path(Icon::Bolt.path())
                            .size(theme::ICON)
                            .text_color(theme::text_muted()),
                    )
                    .child(
                        div()
                            .font_weight(theme::FW_STRONG)
                            .child(t!("whats_new.title").into_owned()),
                    ),
            )
            .child(super::drag_area("whats-new-drag", cx).flex_1().h_full())
            .when(!horizontal, |d| {
                d.child(crate::explorer::caption_buttons(window))
            });
        Some(
            div()
                .id("whats-new-page")
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(top_bar)
                .child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .mr(theme::S_8)
                        .mb(theme::S_8)
                        .rounded(theme::R_12)
                        .bg(theme::paper())
                        .shadow(theme::elev_2())
                        .overflow_hidden()
                        .child(page.view.clone()),
                )
                .into_any_element(),
        )
    }
}
