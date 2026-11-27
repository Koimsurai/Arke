//! The setup guide: a card in the window's bottom right corner on the first run, in four steps.
//!
//! 1. Where are you coming from: File Explorer, Total/Double Commander, Directory Opus/XYplorer
//!    or Finder, each a preset of existing settings (`onboarding.rs`).
//! 2. The look: theme, background material and list density, which change as they are picked.
//! 3. Open folders in Arke: nothing, folders from other apps, or Win+E as well. This is where the
//!    registry is written (`HKEY_CURRENT_USER\Software\Classes`, through the same setting as
//!    Settings › Startup), with the Start menu shortcut and starting with Windows that the
//!    first-run card used to offer.
//! 4. Folders pinned to File Explorer's Quick access that aren't favorites yet.
//!
//! Every choice is a value Settings has. The look applies as it is picked, like in Settings; the
//! preset, the folder handling and the pins are applied when their step's Next (or Done) is
//! clicked, so nothing is changed by looking. The card doesn't block the window. Skip closes it
//! for good (`onboarding_done`); it opens again from Settings › About, the command palette, and
//! for someone who updated, from What's new.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnimationExt, AnyElement, AnyWindowHandle, App, ClickEvent, Context, FocusHandle, Global,
    SharedString, Window, div, prelude::*, px, svg,
};
use rust_i18n::t;

use super::Workspace;
use super::settings_panel::{self as panel, OnPick, TileLook};
use crate::a11y::A11yExt as _;
use crate::assets::Icon;
use crate::keynav::KeyStop as _;
use crate::onboarding::{Connect, Source, Takeover};
use crate::settings::{Density, Material, Settings, SettingsStore};
use crate::theme;

/// The steps, in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Step {
    #[default]
    Source,
    Look,
    Connect,
    Pins,
}

impl Step {
    const ALL: [Self; 4] = [Self::Source, Self::Look, Self::Connect, Self::Pins];

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn next(self) -> Option<Self> {
        Self::ALL.get(self.index() + 1).copied()
    }

    fn prev(self) -> Option<Self> {
        self.index().checked_sub(1).map(|ix| Self::ALL[ix])
    }
}

/// Quick access pins for the last step, read when it is reached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Pins {
    #[default]
    NotRead,
    Reading,
    Failed,
    /// The pinned folders that aren't favorites yet, and whether each is ticked.
    Read(Vec<(PathBuf, bool)>),
}

/// Tile groups, one tab stop each.
#[derive(Clone, Copy)]
enum Group {
    Source,
    Theme,
    Material,
    Density,
    Takeover,
}

/// The guide's state, shared by every window but shown in one.
#[derive(Default)]
struct Guide {
    /// Opened by hand (Settings, the palette, What's new); a first run shows it without this.
    reopened: bool,
    step: Step,
    /// The tile picked in the first step, applied by its Next.
    source: Option<Source>,
    /// The third step's choices; `None` until changed (the settings' values).
    connect: Option<Connect>,
    pins: Pins,
    /// The window it is shown in.
    window: Option<AnyWindowHandle>,
    focus: Vec<FocusHandle>,
}

impl Global for Guide {}

/// Whether the guide is showing: on a first run until finished or skipped, or opened by hand.
pub(crate) fn welcome_due(cx: &App) -> bool {
    if cx.try_global::<Guide>().is_some_and(|g| g.reopened) {
        return true;
    }
    SettingsStore::try_global(cx).is_some_and(|store| {
        let store = store.read(cx);
        store.loaded && store.first_run && !store.settings.onboarding_done
    })
}

/// Opens the guide again at its first step, in `window` (the one it was asked from).
pub(crate) fn open_guide(window: &Window, cx: &mut App) {
    cx.set_global(Guide {
        reopened: true,
        window: Some(window.window_handle()),
        ..Guide::default()
    });
    cx.refresh_windows();
}

/// Closes the guide for good, applying nothing that wasn't confirmed.
fn close_guide(cx: &mut App) {
    cx.set_global(Guide::default());
    set(cx, |s| {
        s.onboarding_done = true;
        // As "Not now" on the old first-run card: the shortcut was offered.
        s.start_menu_offered = true;
    });
    cx.refresh_windows();
}

fn set(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    SettingsStore::global(cx).update(cx, |s, cx| s.update(f, cx));
}

/// The settings as they are now, from a read-only context.
fn current(cx: &App) -> Option<&Settings> {
    SettingsStore::try_global(cx).map(|store| &store.read(cx).settings)
}

fn guide(cx: &mut App) -> &mut Guide {
    cx.default_global::<Guide>()
}

/// Whether the Start menu row is offered: not when the installer made its own shortcut.
fn offers_start_menu() -> bool {
    !crate::update::installed_by_setup()
}

/// The third step's choices as they stand.
fn connect_now(cx: &App) -> Connect {
    cx.try_global::<Guide>()
        .and_then(|g| g.connect)
        .unwrap_or_else(|| {
            let default = Settings::default();
            Connect::from_settings(current(cx).unwrap_or(&default), offers_start_menu())
        })
}

fn change_connect(cx: &mut App, f: impl FnOnce(&mut Connect)) {
    let mut connect = connect_now(cx);
    f(&mut connect);
    guide(cx).connect = Some(connect);
    cx.refresh_windows();
}

impl Workspace {
    /// Confirms the step shown and moves on (or, after the last, closes the guide).
    fn guide_next(&mut self, cx: &mut Context<Self>) {
        let (step, source, pins) = {
            let g = guide(cx);
            (g.step, g.source, g.pins.clone())
        };
        match step {
            Step::Source => {
                if let Some(source) = source {
                    set(cx, |s| source.apply(s));
                }
            }
            Step::Look => {}
            Step::Connect => {
                let connect = connect_now(cx);
                set(cx, |s| connect.apply(s));
            }
            Step::Pins => {
                if let Pins::Read(list) = pins {
                    let places = self.active_view().read(cx).places.clone();
                    set(cx, |s| {
                        for (path, _) in list.iter().filter(|(_, on)| *on) {
                            s.pin(path, &places);
                        }
                    });
                }
            }
        }
        match step.next() {
            Some(next) => {
                guide(cx).step = next;
                if next == Step::Pins {
                    self.read_pins(cx);
                }
                cx.refresh_windows();
            }
            None => close_guide(cx),
        }
    }

    /// Reads Quick access in the background for the last step, keeping the folders that aren't
    /// favorites yet.
    fn read_pins(&self, cx: &mut Context<Self>) {
        guide(cx).pins = Pins::Reading;
        let shell = Arc::clone(&self.services.shell);
        let places = self.active_view().read(cx).places.clone();
        let (tx, rx) = flume::bounded(1);
        crate::explorer::spawn_thread("guide-pins", move || {
            let _ = tx.send(shell.quick_access_pinned());
        });
        cx.spawn(async move |_, cx| {
            let pinned = rx.recv_async().await.ok().flatten();
            cx.update(|cx| {
                let pins = match pinned {
                    None => Pins::Failed,
                    Some(pinned) => {
                        let settings = &SettingsStore::global(cx).read(cx).settings;
                        Pins::Read(
                            pinned
                                .into_iter()
                                .filter(|p| !crate::quick_access::is_favorite(settings, p, &places))
                                .map(|p| (p, true))
                                .collect(),
                        )
                    }
                };
                let g = guide(cx);
                if g.pins == Pins::Reading {
                    g.pins = pins;
                    cx.refresh_windows();
                }
            });
        })
        .detach();
    }
}

/// The guide's card, or `None` when it isn't showing here.
pub(super) fn welcome_card(window: &Window, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    if !welcome_due(cx) {
        return None;
    }
    // One window shows it: the first that draws it, or another once that one is gone.
    let here = window.window_handle();
    let windows = cx.windows();
    let handles: Vec<FocusHandle> = if guide(cx).focus.is_empty() {
        (0..5)
            .map(|_| cx.focus_handle().tab_index(0).tab_stop(true))
            .collect()
    } else {
        Vec::new()
    };
    let g = guide(cx);
    match g.window {
        Some(w) if w != here && windows.contains(&w) => return None,
        _ => g.window = Some(here),
    }
    if g.focus.is_empty() {
        g.focus = handles;
    }
    let step = g.step;
    let body = match step {
        Step::Source => source_step(window, cx),
        Step::Look => look_step(window, cx),
        Step::Connect => connect_step(window, cx),
        Step::Pins => pins_step(cx),
    };
    let viewport = window.viewport_size();
    let width = theme::scaled_px(448.).min(viewport.width - theme::s_32());
    // Room for the header and buttons; the step scrolls when the window is short.
    let body_max = (viewport.height - theme::scaled_px(196.)).max(theme::scaled_px(96.));
    let card = div()
        .id("welcome")
        .debug_selector(|| "welcome".to_owned())
        .a11y(gpui::Role::Dialog, t!("onboarding.welcome"))
        .occlude()
        .w_full()
        .p(theme::s_16())
        .flex()
        .flex_col()
        .gap(theme::s_12())
        .rounded(theme::r_12())
        .bg(theme::overlay())
        .shadow(theme::elev_float())
        .text_size(theme::fs_13())
        .child(header(step))
        .child(
            div()
                .id("welcome-body")
                .max_h(body_max)
                .overflow_y_scroll()
                .child(body),
        )
        .child(footer(step, cx))
        .with_animation("welcome-appear", crate::motion::enter(), |el, t| {
            crate::motion::slide_in(el, t, 0., crate::motion::SLIDE)
        });
    Some(
        div()
            .absolute()
            .bottom(theme::s_16())
            .right(theme::s_16())
            .w(width)
            .child(card)
            .into_any_element(),
    )
}

/// The step's title and line, the progress, and which step it is.
fn header(step: Step) -> AnyElement {
    let (title, desc) = match step {
        Step::Source => (t!("onboarding.source_title"), t!("onboarding.source_desc")),
        Step::Look => (t!("onboarding.look_title"), t!("onboarding.look_desc")),
        Step::Connect => (
            t!("onboarding.connect_title"),
            t!("onboarding.connect_desc"),
        ),
        Step::Pins => (t!("onboarding.pins_title"), t!("onboarding.pins_desc")),
    };
    let at = step.index();
    let count = Step::ALL.len();
    div()
        .flex()
        .flex_col()
        .gap(theme::s_4())
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme::s_8())
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(
                    svg()
                        .path(Icon::Bolt.path())
                        .flex_none()
                        .size(theme::icon_sm())
                        .text_color(theme::signal()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(t!("onboarding.welcome").into_owned()),
                )
                .child(t!("onboarding.step", n = at + 1, total = count).into_owned()),
        )
        // Progress: one segment per step, filled up to this one.
        .child(
            div()
                .flex()
                .gap(theme::s_4())
                .children((0..count).map(|ix| {
                    div()
                        .flex_1()
                        .h(theme::s_2())
                        .rounded(theme::R_FULL)
                        .bg(if ix <= at {
                            theme::signal()
                        } else {
                            theme::line()
                        })
                })),
        )
        .child(
            div()
                .pt(theme::s_4())
                .text_size(theme::fs_14())
                .font_weight(theme::FW_STRONG)
                .child(title.into_owned()),
        )
        .child(
            div()
                .text_color(theme::text_muted())
                .child(desc.into_owned()),
        )
        .into_any_element()
}

/// Skip on the left; Back and Next (Done on the last step) on the right.
fn footer(step: Step, cx: &mut Context<Workspace>) -> AnyElement {
    let last = step.next().is_none();
    let next_label = if last {
        t!("onboarding.done")
    } else {
        t!("onboarding.next")
    };
    div()
        .flex()
        .items_center()
        .gap(theme::s_8())
        .child(
            div().flex_1().min_w_0().child(
                button("welcome-skip", t!("onboarding.skip"), Look::Quiet)
                    .on_click(|_, _, cx| close_guide(cx)),
            ),
        )
        .children(step.prev().map(|prev| {
            button("welcome-back", t!("onboarding.back"), Look::Outline).on_click(
                move |_, _, cx| {
                    guide(cx).step = prev;
                    cx.refresh_windows();
                },
            )
        }))
        .child(
            button("welcome-next", next_label, Look::Primary)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.guide_next(cx))),
        )
        .into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Primary,
    Outline,
    /// Text only, for Skip.
    Quiet,
}

fn button(
    id: &'static str,
    label: std::borrow::Cow<'static, str>,
    look: Look,
) -> gpui::Stateful<gpui::Div> {
    let label = SharedString::from(label.into_owned());
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .a11y_button(label.clone())
        .key_stop()
        .h(theme::h_ctl_sm())
        .px(theme::s_12())
        .flex()
        .flex_none()
        .items_center()
        .rounded(theme::r_8())
        .cursor_pointer()
        .text_size(theme::fs_12())
        .when(look == Look::Primary, |d| {
            d.bg(theme::signal())
                .text_color(theme::on_signal())
                .hover(|s| s.opacity(0.9))
        })
        .when(look == Look::Outline, |d| {
            d.border_1()
                .border_color(theme::line_strong())
                .hover(|s| s.bg(theme::wash()))
        })
        .when(look == Look::Quiet, |d| {
            d.text_color(theme::text_muted())
                .hover(|s| s.bg(theme::wash()).text_color(theme::text()))
        })
        .child(label)
}

/// Focus and keyboard state of a tile group.
fn held(group: Group, window: &Window, cx: &App) -> (FocusHandle, panel::Held) {
    let focus = cx
        .try_global::<Guide>()
        .and_then(|g| g.focus.get(group as usize).cloned());
    let focus = focus.unwrap_or_else(|| cx.focus_handle());
    let focused = focus.is_focused(window);
    let keys = focused && window.last_input_was_keyboard();
    (focus, (focused, keys))
}

/// A labelled row of tiles.
fn section(label: std::borrow::Cow<'static, str>, tiles: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(theme::s_8())
        .child(
            div()
                .text_size(theme::fs_12())
                .font_weight(theme::FW_STRONG)
                .text_color(theme::text_muted())
                .child(label.into_owned()),
        )
        .child(tiles)
}

/// Picks made in the guide (not a setting yet) as tile clicks and arrow keys.
fn picks<T: Copy + 'static>(options: &[T], pick: impl Fn(T, &mut App) + 'static) -> Vec<OnPick> {
    let pick = Rc::new(pick);
    options
        .iter()
        .map(|&option| -> OnPick {
            let pick = Rc::clone(&pick);
            Rc::new(move |cx: &mut App| pick(option, cx))
        })
        .collect()
}

/// Source tiles: preview width and height.
const SOURCE_TILE: (f32, f32) = (200., 76.);
/// Theme tiles in the guide (four across).
const THEME_TILE: (f32, f32) = (96., 60.);
/// Material and density tiles (three across).
const SMALL_TILE: (f32, f32) = (130., 44.);

fn source_step(window: &Window, cx: &App) -> AnyElement {
    let picked = cx.try_global::<Guide>().and_then(|g| g.source);
    let (focus, (focused, keys)) = held(Group::Source, window, cx);
    let options = Source::ALL;
    let on_pick = picks(&options, |source, cx| {
        guide(cx).source = Some(source);
        cx.refresh_windows();
    });
    let tiles = options
        .iter()
        .enumerate()
        .map(|(ix, &source)| {
            let selected = picked == Some(source);
            panel::tile_with(
                ("welcome-source", ix),
                source.label().into_owned(),
                selected,
                SOURCE_TILE,
                source_preview(source, SOURCE_TILE).into_any_element(),
                (focused && selected, keys && selected),
                Rc::clone(&on_pick[ix]),
            )
        })
        .collect();
    let at = picked.and_then(|p| options.iter().position(|s| *s == p));
    let group = panel::radio_group_with(
        "welcome-sources",
        t!("onboarding.source_title"),
        &focus,
        on_pick,
        at,
        tiles,
    );
    div()
        .flex()
        .flex_col()
        .gap(theme::s_12())
        .child(group)
        .children(current(cx).map(|settings| changes(picked, settings)))
        .into_any_element()
}

/// What the picked preset will change, before its Next applies it.
fn changes(picked: Option<Source>, settings: &Settings) -> AnyElement {
    let note = |text: std::borrow::Cow<'static, str>| {
        div()
            .text_size(theme::fs_12())
            .text_color(theme::text_muted())
            .child(text.into_owned())
            .into_any_element()
    };
    let Some(source) = picked else {
        return note(t!("onboarding.pick_hint"));
    };
    let items = source.changes(settings);
    if items.is_empty() {
        return note(t!("onboarding.no_changes"));
    }
    div()
        .id("welcome-changes")
        .debug_selector(|| "welcome-changes".to_owned())
        .p(theme::s_12())
        .flex()
        .flex_col()
        .gap(theme::s_4())
        .rounded(theme::r_8())
        .bg(theme::wash())
        .text_size(theme::fs_12())
        .child(
            div()
                .font_weight(theme::FW_STRONG)
                .child(t!("onboarding.changes").into_owned()),
        )
        .children(items.into_iter().map(|item| {
            div()
                .flex()
                .gap(theme::s_8())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme::text_muted())
                        .child(item.name().into_owned()),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(theme::scaled_px(180.))
                        .text_right()
                        .child(item.value().into_owned()),
                )
        }))
        .into_any_element()
}

fn look_step(window: &Window, cx: &App) -> AnyElement {
    let Some(s) = current(cx) else {
        return div().into_any_element();
    };
    let (theme_focus, theme_held) = held(Group::Theme, window, cx);
    let mut body = div().flex().flex_col().gap(theme::s_16()).child(section(
        t!("settings.theme"),
        panel::theme_tiles(s, &theme_focus, theme_held, THEME_TILE),
    ));
    if theme::mica_available() {
        let options = [Material::Mica, Material::MicaAlt, Material::Solid];
        let labels = [
            t!("material.mica"),
            t!("material.mica_alt"),
            t!("material.solid"),
        ];
        body = body.child(section(
            t!("settings.material"),
            live_tiles(
                ("welcome-material", Group::Material, t!("settings.material")),
                &options,
                &labels,
                s.material,
                |m, s| s.material = m,
                material_preview,
                (window, cx),
            ),
        ));
    }
    let options = [Density::Compact, Density::Standard, Density::Comfortable];
    let labels = [
        t!("density.compact"),
        t!("density.standard"),
        t!("density.comfortable"),
    ];
    body.child(section(
        t!("settings.list_density"),
        live_tiles(
            (
                "welcome-density",
                Group::Density,
                t!("settings.list_density"),
            ),
            &options,
            &labels,
            s.density,
            |d, s| s.density = d,
            density_preview,
            (window, cx),
        ),
    ))
    .into_any_element()
}

/// Tiles of a setting that applies as it is picked (material, density), as in Settings.
fn live_tiles<T: Copy + PartialEq + 'static>(
    (id, group, label): (&'static str, Group, std::borrow::Cow<'static, str>),
    options: &[T],
    labels: &[std::borrow::Cow<'static, str>],
    current: T,
    write: fn(T, &mut Settings),
    preview: fn(T, (f32, f32)) -> gpui::Div,
    (window, cx): (&Window, &App),
) -> AnyElement {
    let (focus, (focused, keys)) = held(group, window, cx);
    let on_pick = picks(options, move |value, cx| set(cx, |s| write(value, s)));
    let tiles = options
        .iter()
        .zip(labels)
        .enumerate()
        .map(|(ix, (&value, label))| {
            let selected = value == current;
            panel::tile_with(
                (id, ix),
                label.to_string(),
                selected,
                SMALL_TILE,
                preview(value, SMALL_TILE).into_any_element(),
                (focused && selected, keys && selected),
                Rc::clone(&on_pick[ix]),
            )
        })
        .collect();
    let at = options.iter().position(|v| *v == current);
    panel::radio_group_with(id, label, &focus, on_pick, at, tiles).into_any_element()
}

fn connect_step(window: &Window, cx: &App) -> AnyElement {
    let connect = connect_now(cx);
    let (focus, (focused, keys)) = held(Group::Takeover, window, cx);
    let options = Takeover::ALL;
    let on_pick = picks(&options, |takeover, cx| {
        change_connect(cx, |c| c.takeover = takeover);
    });
    let rows = options
        .iter()
        .enumerate()
        .map(|(ix, &takeover)| {
            let selected = connect.takeover == takeover;
            option_row(
                ix,
                takeover,
                selected,
                (focused && selected, keys && selected),
                Rc::clone(&on_pick[ix]),
            )
        })
        .collect();
    let at = options.iter().position(|t| *t == connect.takeover);
    let group = panel::radio_group_with(
        "welcome-takeover",
        t!("onboarding.connect_title"),
        &focus,
        on_pick,
        at,
        rows,
    );
    let note = if connect.takeover.needs_background() {
        t!("onboarding.registry_note")
    } else {
        t!("onboarding.registry_none")
    };
    div()
        .flex()
        .flex_col()
        .gap(theme::s_12())
        .child(group)
        .child(
            div()
                .flex()
                .items_start()
                .gap(theme::s_8())
                .text_size(theme::fs_12())
                .text_color(theme::text_muted())
                .child(
                    svg()
                        .path(Icon::Info.path())
                        .flex_none()
                        .mt(theme::s_2())
                        .size(theme::icon_sm())
                        .text_color(theme::text_muted()),
                )
                .child(div().flex_1().min_w_0().child(note.into_owned())),
        )
        .child(connect_checks(connect))
        .into_any_element()
}

/// Keep running, the Start menu shortcut and starting with Windows.
fn connect_checks(connect: Connect) -> AnyElement {
    let needed = connect.takeover.needs_background();
    let mut rows = div().flex().flex_col().gap(theme::s_8());
    rows = rows.child(
        div()
            .flex()
            .flex_col()
            .gap(theme::s_2())
            .child(super::report_page::check_row(
                "welcome-keep-running",
                t!("settings.keep_running"),
                connect.background(),
                move |cx| {
                    // Opening folders in Arke needs it running: the box stays ticked.
                    if !needed {
                        change_connect(cx, |c| c.keep_running = !c.keep_running);
                    }
                },
            ))
            .when(needed, |d| {
                d.child(
                    div()
                        .pl(theme::icon() + theme::s_8())
                        .text_size(theme::fs_12())
                        .text_color(theme::text_subtle())
                        .child(t!("onboarding.background_needed").into_owned()),
                )
            }),
    );
    if let Some(start_menu) = connect.start_menu {
        rows = rows.child(super::report_page::check_row(
            "welcome-start-menu",
            t!("settings.start_menu"),
            start_menu,
            |cx| {
                change_connect(cx, |c| c.start_menu = c.start_menu.map(|on| !on));
            },
        ));
    }
    rows.child(super::report_page::check_row(
        "welcome-autostart",
        t!("settings.autostart"),
        connect.autostart,
        |cx| change_connect(cx, |c| c.autostart = !c.autostart),
    ))
    .child(
        div()
            .text_size(theme::fs_12())
            .text_color(theme::text_subtle())
            .child(t!("welcome.later_hint").into_owned()),
    )
    .into_any_element()
}

/// A folder handling choice: a small picture, its name and what it does, the whole row picked
/// like a tile (accent border).
fn option_row(
    ix: usize,
    takeover: Takeover,
    selected: bool,
    (claim, ring): (bool, bool),
    on_pick: OnPick,
) -> AnyElement {
    let (title, desc) = match takeover {
        Takeover::Off => (
            t!("onboarding.takeover_off"),
            t!("onboarding.takeover_off_desc"),
        ),
        Takeover::Folders => (
            t!("settings.takeover"),
            t!("onboarding.takeover_folders_desc"),
        ),
        Takeover::FoldersAndWinE => (
            t!("onboarding.takeover_all"),
            t!("onboarding.takeover_all_desc"),
        ),
    };
    let label = title.to_string();
    let row = div()
        .id(("welcome-takeover", ix))
        .debug_selector(move || format!("welcome-takeover-{ix}"))
        .a11y_lazy(gpui::Role::RadioButton, move || label)
        .aria_toggled(crate::a11y::toggled(selected))
        .when(
            claim,
            gpui::StatefulInteractiveElement::aria_active_descendant,
        )
        .w_full()
        .p(theme::s_6())
        .flex()
        .items_center()
        .gap(theme::s_12())
        .rounded(theme::r_8())
        .border_2()
        .cursor_pointer()
        .when(selected, |d| d.border_color(theme::signal()))
        .when(!selected, |d| {
            d.border_color(theme::line())
                .hover(|s| s.border_color(theme::line_strong()))
        })
        .child(
            div()
                .w(px(84.))
                .h(px(44.))
                .flex_none()
                .rounded(theme::r_4())
                .overflow_hidden()
                .child(takeover_preview(takeover, (84., 44.))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme::s_2())
                .child(
                    div()
                        .when(selected, |d| d.font_weight(theme::FW_STRONG))
                        .child(title.into_owned()),
                )
                .child(
                    div()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_muted())
                        // At most two lines, whatever the language.
                        .line_clamp(2)
                        .child(desc.into_owned()),
                ),
        )
        .on_click(move |_, _, cx| on_pick(cx));
    // The keyboard ring, as on the tiles in Settings: outside the row, taking no room.
    let room = px(4.);
    div()
        .w_full()
        .relative()
        .child(row)
        .when(ring, |d| {
            d.child(
                div()
                    .absolute()
                    .top(-room)
                    .left(-room)
                    .right(-room)
                    .bottom(-room)
                    .rounded(theme::r_8() + room)
                    .border_2()
                    .border_color(theme::text()),
            )
        })
        .into_any_element()
}

fn pins_step(cx: &App) -> AnyElement {
    let pins = cx
        .try_global::<Guide>()
        .map_or(Pins::NotRead, |g| g.pins.clone());
    let note = |id: &'static str, text: std::borrow::Cow<'static, str>| {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .text_color(theme::text_muted())
            .child(text.into_owned())
            .into_any_element()
    };
    match pins {
        Pins::NotRead | Pins::Reading => {
            note("welcome-pins-reading", t!("onboarding.pins_reading"))
        }
        Pins::Failed => note("welcome-pins-failed", t!("onboarding.pins_failed")),
        Pins::Read(list) if list.is_empty() => {
            note("welcome-pins-none", t!("onboarding.pins_none"))
        }
        Pins::Read(list) => div()
            .flex()
            .flex_col()
            .gap(theme::s_8())
            .children(
                list.into_iter()
                    .enumerate()
                    .map(|(ix, (path, on))| pin_row(ix, &path, on)),
            )
            .into_any_element(),
    }
}

/// A pinned folder with its box: name, and the path below.
fn pin_row(ix: usize, path: &std::path::Path, on: bool) -> AnyElement {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let label = name.clone();
    div()
        .id(("welcome-pin", ix))
        .debug_selector(move || format!("welcome-pin-{ix}"))
        .a11y_lazy(gpui::Role::CheckBox, move || label)
        .key_stop()
        .aria_toggled(crate::a11y::toggled(on))
        .flex()
        .items_center()
        .gap(theme::s_8())
        .cursor_pointer()
        .child(
            div()
                .size(theme::icon())
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(theme::r_4())
                .border_1()
                .when(on, |d| {
                    d.bg(theme::signal()).border_color(theme::signal()).child(
                        svg()
                            .path(Icon::Check.path())
                            .size(theme::icon_sm())
                            .text_color(theme::on_signal()),
                    )
                })
                .when(!on, |d| d.border_color(theme::line_strong())),
        )
        .child(
            svg()
                .path(Icon::Folder.path())
                .flex_none()
                .size(theme::icon())
                .text_color(theme::text_muted()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().truncate().child(name))
                .child(
                    div()
                        .truncate()
                        .text_size(theme::fs_12())
                        .text_color(theme::text_muted())
                        .child(path.display().to_string()),
                ),
        )
        .on_click(move |_, _, cx| {
            if let Pins::Read(list) = &mut guide(cx).pins
                && let Some(row) = list.get_mut(ix)
            {
                row.1 = !row.1;
            }
            cx.refresh_windows();
        })
        .into_any_element()
}

/// Whether What's new after an update offers the guide: only the first time (it is recorded
/// here), and not to someone who has been through it.
pub(super) fn offer_once(cx: &mut App) -> bool {
    let offer = current(cx).is_some_and(|s| !s.onboarding_done && !s.onboarding_offered);
    if offer {
        set(cx, |s| s.onboarding_offered = true);
    }
    offer
}

/// The What's new entry for someone who updated and hasn't been through the guide, or `None`.
pub(super) fn whats_new_entry(cx: &App) -> Option<AnyElement> {
    if current(cx).is_none_or(|s| s.onboarding_done) {
        return None;
    }
    Some(
        div()
            .id("whats-new-guide")
            .debug_selector(|| "whats-new-guide".to_owned())
            .p(theme::s_16())
            .flex()
            .items_center()
            .gap(theme::s_12())
            .rounded(theme::r_8())
            .border_1()
            .border_color(theme::signal_line())
            .bg(theme::signal_wash())
            .child(
                svg()
                    .path(Icon::Bolt.path())
                    .flex_none()
                    .size(theme::icon())
                    .text_color(theme::signal()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(theme::s_2())
                    .child(
                        div()
                            .font_weight(theme::FW_STRONG)
                            .child(t!("onboarding.whats_new_title").into_owned()),
                    )
                    .child(
                        div()
                            .text_size(theme::fs_12())
                            .text_color(theme::text_muted())
                            .child(t!("onboarding.whats_new_body").into_owned()),
                    ),
            )
            .child(
                button(
                    "whats-new-guide-open",
                    t!("onboarding.whats_new_open"),
                    Look::Primary,
                )
                .on_click(|_, window, cx| open_guide(window, cx)),
            )
            .into_any_element(),
    )
}

// --- Mini previews, drawn in the current theme ---

/// The colors previews are drawn in.
fn colors() -> TileLook {
    TileLook::current((theme::frame_solid(), theme::surface()), theme::signal())
}

/// A rounded line standing for text.
fn bar(width: f32, color: gpui::Rgba) -> gpui::Div {
    div()
        .h(px(3.))
        .w(px(width))
        .flex_none()
        .rounded(theme::R_FULL)
        .bg(color)
}

/// A list row: a small icon and text lines of `widths` (the first is the name).
fn list_row(widths: &[f32], selected: bool, c: TileLook) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(2.))
        .py(px(1.))
        .rounded(px(2.))
        .when(selected, |d| d.bg(gpui::Rgba { a: 0.2, ..c.accent }))
        .child(div().size(px(4.)).flex_none().rounded(px(1.)).bg(c.faint))
        .children(
            widths
                .iter()
                .enumerate()
                .map(|(ix, &w)| bar(w, if ix == 0 && selected { c.ink } else { c.faint })),
        )
}

/// A tab strip: one active tab (paper colored) and another.
fn tab_strip(c: TileLook) -> gpui::Div {
    div()
        .h(px(9.))
        .flex_none()
        .flex()
        .items_end()
        .gap(px(3.))
        .child(
            div()
                .w(px(36.))
                .h(px(8.))
                .rounded(px(2.))
                .bg(c.paper)
                .flex()
                .items_center()
                .px(px(3.))
                .child(bar(18., c.ink)),
        )
        .child(
            div()
                .w(px(30.))
                .h(px(8.))
                .flex()
                .items_center()
                .px(px(3.))
                .child(bar(16., c.faint)),
        )
}

/// Rows in a paper area.
fn paper(c: TileLook) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .p(px(4.))
        .flex()
        .flex_col()
        .gap(px(3.))
        .overflow_hidden()
        .rounded(px(3.))
        .bg(c.paper)
}

fn source_preview(source: Source, (width, height): (f32, f32)) -> gpui::Div {
    let c = colors();
    let frame = div()
        .w(px(width))
        .h(px(height))
        .flex_none()
        .p(px(6.))
        .flex()
        .flex_col()
        .gap(px(3.))
        .bg(c.frame)
        .overflow_hidden();
    let details = |widths: &[f32]| {
        paper(c)
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .pb(px(1.))
                    .border_b_1()
                    .border_color(theme::line())
                    .children(widths.iter().map(|&w| bar(w * 0.7, c.faint))),
            )
            .children((0..4).map(|row| list_row(widths, row == 1, c)))
    };
    match source {
        // Tabs on top, the navigation pane, details.
        Source::Explorer => frame.child(tab_strip(c)).child(
            div()
                .flex_1()
                .flex()
                .gap(px(5.))
                .child(nav_pane(c, false))
                .child(details(&[46., 22., 16.])),
        ),
        // Two panels, each with its tabs and columns, the function key bar below.
        Source::Commander => {
            let panel = || {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(tab_strip(c))
                    .child(details(&[22., 8., 10., 12.]))
            };
            frame
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .gap(px(4.))
                        .child(panel())
                        .child(panel()),
                )
                .child(
                    div().h(px(5.)).flex_none().flex().gap(px(2.)).children(
                        (0..7).map(|_| div().flex_1().h_full().rounded(px(1.)).bg(c.paper)),
                    ),
                )
        }
        // Tabs, a folder tree and details.
        Source::Opus => frame.child(tab_strip(c)).child(
            div()
                .flex_1()
                .flex()
                .gap(px(5.))
                .child(nav_pane(c, true))
                .child(details(&[42., 20., 18.])),
        ),
        // Window buttons, the sidebar, and icons.
        Source::Finder => {
            frame
                .child(div().flex().gap(px(3.)).children(
                    (0..3).map(|_| div().size(px(5.)).rounded(theme::R_FULL).bg(c.faint)),
                ))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .gap(px(5.))
                        .child(nav_pane(c, false))
                        .child(icon_grid(c)),
                )
        }
    }
}

/// The sidebar: place names, or a tree (indented).
fn nav_pane(c: TileLook, tree: bool) -> gpui::Div {
    let lines: &[(f32, f32)] = if tree {
        &[(0., 24.), (4., 20.), (8., 16.), (8., 18.), (4., 20.)]
    } else {
        &[(0., 24.), (0., 18.), (0., 22.), (0., 16.)]
    };
    div()
        .w(px(32.))
        .flex_none()
        .pt(px(2.))
        .flex()
        .flex_col()
        .gap(px(5.))
        .children(
            lines
                .iter()
                .map(|&(indent, w)| div().pl(px(indent)).child(bar(w, c.faint))),
        )
}

/// Large icons: folders with a name under each, one selected.
fn icon_grid(c: TileLook) -> gpui::Div {
    paper(c).child(
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .children((0..8).map(|ix| {
                let selected = ix == 1;
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.))
                    .p(px(1.))
                    .rounded(px(2.))
                    .when(selected, |d| d.bg(gpui::Rgba { a: 0.2, ..c.accent }))
                    .child(div().w(px(16.)).h(px(12.)).rounded(px(2.)).bg(c.faint))
                    .child(bar(12., if selected { c.ink } else { c.faint }))
            })),
    )
}

fn density_preview(density: Density, (width, height): (f32, f32)) -> gpui::Div {
    let c = colors();
    let (rows, gap) = match density {
        Density::Compact => (6, 2.),
        Density::Standard => (4, 5.),
        Density::Comfortable => (3, 8.),
    };
    div()
        .w(px(width))
        .h(px(height))
        .flex_none()
        .p(px(6.))
        .flex()
        .bg(c.frame)
        .child(
            paper(c)
                .gap(px(gap))
                .children((0..rows).map(|row| list_row(&[44., 18.], row == 1, c))),
        )
}

fn material_preview(material: Material, (width, height): (f32, f32)) -> gpui::Div {
    let c = colors();
    // The desktop behind the window, which Mica lets through blurred, Mica Alt with more of
    // the accent, Solid not at all.
    let desktop = gpui::linear_gradient(
        135.,
        gpui::linear_color_stop(c.accent, 0.),
        gpui::linear_color_stop(theme::text_disabled(), 1.),
    );
    let frame_alpha = match material {
        Material::Mica => 0.72,
        Material::MicaAlt => 0.84,
        Material::Solid => 1.,
    };
    let frame = match material {
        Material::MicaAlt => theme::blend(c.frame, c.accent, 0.12),
        _ => c.frame,
    };
    div()
        .w(px(width))
        .h(px(height))
        .flex_none()
        .relative()
        .bg(desktop)
        .child(
            div()
                .absolute()
                .inset_0()
                .p(px(6.))
                .flex()
                .gap(px(6.))
                .bg(gpui::Rgba {
                    a: frame_alpha,
                    ..frame
                })
                .child(nav_pane(c, false).w(px(28.)))
                .child(
                    paper(c)
                        .child(list_row(&[40.], true, c))
                        .child(list_row(&[32.], false, c)),
                ),
        )
}

/// File Explorer alone; a folder opening in Arke; Win+E opening Arke.
fn takeover_preview(takeover: Takeover, (width, height): (f32, f32)) -> gpui::Div {
    let c = colors();
    let glyph = |icon: Icon| {
        svg()
            .path(icon.path())
            .flex_none()
            .size(px(16.))
            .text_color(c.ink)
    };
    let arrow = svg()
        .path(Icon::ArrowRight.path())
        .flex_none()
        .size(px(10.))
        .text_color(c.accent);
    // A small Arke window: frame, paper and a selected row.
    let arke = div()
        .w(px(26.))
        .h(px(20.))
        .flex_none()
        .p(px(2.))
        .flex()
        .rounded(px(3.))
        .border_1()
        .border_color(c.accent)
        .bg(c.frame)
        .child(
            paper(c)
                .p(px(2.))
                .child(bar(12., c.accent))
                .child(bar(9., c.faint)),
        );
    let base = div()
        .w(px(width))
        .h(px(height))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .bg(c.frame);
    match takeover {
        Takeover::Off => base.child(glyph(Icon::Explorer)),
        Takeover::Folders => base.child(glyph(Icon::Folder)).child(arrow).child(arke),
        Takeover::FoldersAndWinE => base
            .child(
                div()
                    .px(px(3.))
                    .py(px(1.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(c.ink)
                    .text_size(theme::fs_11())
                    .text_color(c.ink)
                    // A key name, the same in every language.
                    .child("Win+E"),
            )
            .child(arrow)
            .child(arke),
    }
}

#[cfg(test)]
impl SettingsStore {
    /// Pretends this is a first run (tests).
    pub(crate) fn set_first_run_for_test(&mut self, first_run: bool) {
        self.first_run = first_run;
        self.loaded = true;
    }
}

#[cfg(test)]
impl Workspace {
    /// The guide's step (0 to 3) while it shows, `None` when closed.
    pub(crate) fn guide_step_for_test(cx: &App) -> Option<usize> {
        welcome_due(cx).then(|| cx.try_global::<Guide>().map_or(0, |g| g.step.index()))
    }
}
