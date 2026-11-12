//! Icon size of the Icons view: any size between Explorer's small and extra large icons, picked
//! with the slider in the view options flyout (`explorer_viewopts.rs`).
//!
//! - Ctrl+Shift+1/2/3/4 jump to extra large, large, medium and small icons (Explorer's keys).
//! - Ctrl+mouse wheel over the list steps through [`IconSize::LADDER`]; past small icons it goes on
//!   to Details, and from Details it comes back to small icons, as in Explorer.
//! - The size is remembered per folder together with the view (`folder_views.rs`). The four named
//!   sizes are stored by name, as before sizes became continuous, so older builds still read them.
//! - Thumbnails are decoded at a size matching the tile and the display scale
//!   ([`arke_core::thumbs::source_size`]), so they stay sharp on high DPI without decoding more
//!   pixels than shown.

use std::borrow::Cow;

use gpui::{
    AnyElement, Context, DispatchPhase, Pixels, ScrollDelta, ScrollEvent, Window, canvas,
    prelude::*, px,
};
use rust_i18n::t;

use super::{Explorer, ViewIcons, ViewIconsExtraLarge, ViewIconsMedium, ViewIconsSmall, ViewMode};
use crate::theme;

/// Icon size of the Icons view: the edge of a tile's image area in logical pixels, a multiple of
/// [`IconSize::GRAIN`] from [`IconSize::SMALL`] to [`IconSize::EXTRA_LARGE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct IconSize(u16);

/// Pixels of precise (touchpad) scrolling that count as one wheel notch.
const PIXELS_PER_STEP: f32 = 48.;

/// What one Ctrl+wheel step lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZoomTarget {
    Icons(IconSize),
    Details,
}

/// The four sizes Explorer names, smallest first, with their tile metrics.
const ANCHORS: [(IconSize, theme::Tile); 4] = [
    (IconSize::SMALL, theme::TILE_SMALL),
    (IconSize::MEDIUM, theme::TILE_MEDIUM),
    (IconSize::LARGE, theme::TILE_LARGE),
    (IconSize::EXTRA_LARGE, theme::TILE_XL),
];

impl Default for IconSize {
    /// The size the Icons view always had; what new folders get.
    fn default() -> Self {
        Self::LARGE
    }
}

impl IconSize {
    /// Explorer's small icons (Ctrl+Shift+4), the smallest size.
    pub(crate) const SMALL: Self = Self(48);
    /// Medium icons (Ctrl+Shift+3).
    pub(crate) const MEDIUM: Self = Self(64);
    /// Large icons (Ctrl+Shift+2), the default.
    pub(crate) const LARGE: Self = Self(88);
    /// Extra large icons (Ctrl+Shift+1), the largest size.
    pub(crate) const EXTRA_LARGE: Self = Self(192);
    /// The slider moves in steps of this many pixels.
    const GRAIN: u16 = 4;
    /// Ctrl+wheel stops, smallest to largest: roughly even ratios, through the four named sizes.
    pub(crate) const LADDER: [Self; 10] = [
        Self(48),
        Self(56),
        Self(64),
        Self(76),
        Self(88),
        Self(104),
        Self(124),
        Self(144),
        Self(168),
        Self(192),
    ];

    /// The size nearest to `px` image pixels, clamped to the range and snapped to the grain.
    pub(crate) fn from_px(px: f32) -> Self {
        let (lo, hi) = (f32::from(Self::SMALL.0), f32::from(Self::EXTRA_LARGE.0));
        let grain = f32::from(Self::GRAIN);
        let px = if px.is_finite() { px } else { hi };
        let snapped = (px.clamp(lo, hi) / grain).round() * grain;
        // Clamped to 48..=192 above, so the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Self(snapped as u16)
    }

    /// Where the size sits on the slider, 0 (small) to 1 (extra large).
    pub(crate) fn fraction(self) -> f32 {
        let (lo, hi) = (f32::from(Self::SMALL.0), f32::from(Self::EXTRA_LARGE.0));
        ((f32::from(self.0) - lo) / (hi - lo)).clamp(0., 1.)
    }

    /// The size at slider position `fraction`.
    pub(crate) fn from_fraction(fraction: f32) -> Self {
        let (lo, hi) = (f32::from(Self::SMALL.0), f32::from(Self::EXTRA_LARGE.0));
        Self::from_px(lo + fraction.clamp(0., 1.) * (hi - lo))
    }

    /// Tile metrics: the named sizes use their own, sizes between them are interpolated, so the
    /// grid grows smoothly while the slider moves.
    pub(crate) fn tile(self) -> theme::Tile {
        let ((a, ta), (b, tb)) = ANCHORS
            .windows(2)
            .map(|w| (w[0], w[1]))
            .find(|(_, (b, _))| self.0 <= b.0)
            .unwrap_or((ANCHORS[2], ANCHORS[3]));
        let t = (f32::from(self.0.saturating_sub(a.0)) / f32::from(b.0 - a.0)).clamp(0., 1.);
        let mix = |p: Pixels, q: Pixels| px(f32::from(p).mul_add(1. - t, f32::from(q) * t).round());
        theme::Tile {
            w: mix(ta.w, tb.w),
            h: mix(ta.h, tb.h),
            image: px(f32::from(self.0)),
            icon: mix(ta.icon, tb.icon),
        }
    }

    /// Explorer's name for this size, when it is one of the four.
    const fn named(self) -> Option<Named> {
        match self.0 {
            48 => Some(Named::Small),
            64 => Some(Named::Medium),
            88 => Some(Named::Large),
            192 => Some(Named::ExtraLarge),
            _ => None,
        }
    }

    /// Short label: Explorer's name for the size, or its pixel size in between.
    pub(crate) fn label(self) -> Cow<'static, str> {
        match self.named() {
            Some(Named::Small) => t!("menu.view_icons_small"),
            Some(Named::Medium) => t!("menu.view_icons_medium"),
            Some(Named::Large) => t!("menu.view_icons"),
            Some(Named::ExtraLarge) => t!("menu.view_icons_xl"),
            None => t!("view.icon_px", px = self.0),
        }
    }

    /// Tooltip of the view switcher's Icons button, with the shortcut of a named size.
    pub(crate) fn tooltip(self) -> Cow<'static, str> {
        match self.named() {
            Some(Named::Small) => t!("ctx.view_icons_small"),
            Some(Named::Medium) => t!("ctx.view_icons_medium"),
            Some(Named::Large) => t!("ctx.view_icons"),
            Some(Named::ExtraLarge) => t!("ctx.view_icons_xl"),
            None => t!("ctx.view_icons_custom", px = self.0),
        }
    }

    /// One wheel notch up: the next ladder stop above, if any.
    fn larger(self) -> Option<Self> {
        Self::LADDER.into_iter().find(|s| *s > self)
    }

    /// One wheel notch down: the next ladder stop below, if any.
    fn smaller(self) -> Option<Self> {
        Self::LADDER.into_iter().rev().find(|s| *s < self)
    }
}

/// The four sizes Explorer names, stored by these names as before sizes became continuous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Named {
    Small,
    Medium,
    Large,
    ExtraLarge,
}

/// How a size is stored: a named size by name (older builds read it), any other in whole pixels
/// (older builds fall back to large icons).
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Stored {
    Named(Named),
    Pixels(f32),
}

impl serde::Serialize for IconSize {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.named() {
            Some(named) => named.serialize(s),
            None => s.serialize_u16(self.0),
        }
    }
}

impl<'de> serde::Deserialize<'de> for IconSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Stored::deserialize(d)? {
            Stored::Named(Named::Small) => Self::SMALL,
            Stored::Named(Named::Medium) => Self::MEDIUM,
            Stored::Named(Named::Large) => Self::LARGE,
            Stored::Named(Named::ExtraLarge) => Self::EXTRA_LARGE,
            Stored::Pixels(px) => Self::from_px(px),
        })
    }
}

/// Where `steps` Ctrl+wheel notches (positive: larger) go from `view` at `size`. Columns view and
/// moves that end where they started stay put (`None`).
pub(crate) fn zoom(view: ViewMode, size: IconSize, steps: i32) -> Option<ZoomTarget> {
    // Details sits one notch below small icons (`None` here).
    let start = match view {
        ViewMode::Details => None,
        ViewMode::Icons => Some(size),
        ViewMode::Columns => return None,
    };
    let mut at = start;
    // More notches than stops can't go further than the ends.
    for _ in 0..steps.unsigned_abs().min(16) {
        at = if steps > 0 {
            Some(at.map_or(IconSize::SMALL, |s| s.larger().unwrap_or(s)))
        } else {
            at.and_then(IconSize::smaller)
        };
    }
    (at != start).then(|| at.map_or(ZoomTarget::Details, ZoomTarget::Icons))
}

impl Explorer {
    /// The grid's tile metrics at the current icon size.
    pub(crate) fn tile(&self) -> theme::Tile {
        self.icon_size.tile()
    }

    /// The user picked an icon size: show Icons at that size and remember it for this folder.
    pub(crate) fn pick_icon_size(&mut self, size: IconSize, cx: &mut Context<Self>) {
        if self.view_mode == ViewMode::Icons && self.icon_size == size {
            return;
        }
        // Another layout brightens in; a size change within Icons (slider, Ctrl+wheel) is direct
        // manipulation and stays instant.
        if self.view_mode != ViewMode::Icons {
            self.view_cue.fire();
        }
        self.set_icon_size(size);
        self.set_view_mode(ViewMode::Icons, cx);
        self.remember_folder_view(cx);
        cx.notify();
    }

    /// Shows `size` without recording it anywhere.
    pub(super) fn set_icon_size(&mut self, size: IconSize) {
        if self.icon_size != size {
            self.icon_size = size;
            // Column count changes with the tile width: keep the cursor in view once it's known.
            self.recenter_grid = self.view_mode == ViewMode::Icons;
        }
    }

    pub(super) fn view_icons(&mut self, _: &ViewIcons, _: &mut Window, cx: &mut Context<Self>) {
        self.pick_icon_size(IconSize::LARGE, cx);
    }

    pub(super) fn view_icons_xl(
        &mut self,
        _: &ViewIconsExtraLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_icon_size(IconSize::EXTRA_LARGE, cx);
    }

    pub(super) fn view_icons_medium(
        &mut self,
        _: &ViewIconsMedium,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_icon_size(IconSize::MEDIUM, cx);
    }

    pub(super) fn view_icons_small(
        &mut self,
        _: &ViewIconsSmall,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_icon_size(IconSize::SMALL, cx);
    }

    /// The view switcher's Icons button: Icons at the size this folder last had.
    pub(super) fn show_icons(&mut self, cx: &mut Context<Self>) {
        self.pick_icon_size(self.icon_size, cx);
    }

    /// Ctrl+wheel over the list. Returns whether the event was used (so the list doesn't scroll).
    fn on_zoom_wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) -> bool {
        if self.search_mode || self.view_mode == ViewMode::Columns {
            return false;
        }
        let notches = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / PIXELS_PER_STEP,
        };
        self.zoom_accum += notches;
        let whole = self.zoom_accum.trunc();
        if whole == 0. {
            return true;
        }
        self.zoom_accum -= whole;
        // Clamped by `zoom` anyway; a wheel never sends anywhere near i32's range.
        #[allow(clippy::cast_possible_truncation)]
        let steps = whole.clamp(-16., 16.) as i32;
        match zoom(self.view_mode, self.icon_size, steps) {
            Some(ZoomTarget::Icons(size)) => self.pick_icon_size(size, cx),
            Some(ZoomTarget::Details) => {
                self.set_view_mode(ViewMode::Details, cx);
                self.remember_folder_view(cx);
            }
            None => {}
        }
        true
    }

    /// Invisible layer over the list that takes Ctrl+wheel before the list scrolls (capture phase).
    pub(super) fn render_zoom_capture(cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        canvas(
            |bounds, _, _| bounds,
            move |_, bounds, window, _| {
                let target = this;
                window.on_mouse_event(move |e: &ScrollEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture
                        || !e.modifiers.control
                        || e.modifiers.alt
                        || e.modifiers.shift
                        || !bounds.contains(&e.position)
                    {
                        return;
                    }
                    if target.update(cx, |this, cx| this.on_zoom_wheel(e, cx)) {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// Registers the view switching actions (Details, the icon sizes, Columns, the tree toggle).
    pub(super) fn view_actions(
        d: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        d.on_action(cx.listener(Self::view_details))
            .on_action(cx.listener(Self::view_icons))
            .on_action(cx.listener(Self::view_icons_xl))
            .on_action(cx.listener(Self::view_icons_medium))
            .on_action(cx.listener(Self::view_icons_small))
            .on_action(cx.listener(Self::toggle_tree))
            .on_action(cx.listener(Self::view_columns))
    }

    /// Decode size for grid thumbnails at the current icon size and display scale.
    pub(crate) fn thumbnail_px(&self) -> u32 {
        arke_core::thumbs::source_size(f32::from(self.tile().image), self.ui_scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_steps_through_the_ladder_and_into_details() {
        use IconSize as S;
        use ZoomTarget as Z;
        let icons = ViewMode::Icons;
        assert_eq!(zoom(icons, S::LARGE, 1), Some(Z::Icons(S::from_px(104.))));
        assert_eq!(zoom(icons, S::LARGE, -1), Some(Z::Icons(S::from_px(76.))));
        assert_eq!(zoom(icons, S::MEDIUM, -2), Some(Z::Icons(S::SMALL)));
        assert_eq!(zoom(icons, S::SMALL, -1), Some(Z::Details));
        assert_eq!(zoom(icons, S::EXTRA_LARGE, 1), None, "largest already");
        assert_eq!(zoom(icons, S::LARGE, -9), Some(Z::Details), "clamped");
        assert_eq!(zoom(icons, S::LARGE, 40), Some(Z::Icons(S::EXTRA_LARGE)));
        assert_eq!(zoom(icons, S::MEDIUM, 0), None);
        // A slider size between stops goes to the next stop either way.
        let between = S::from_px(92.);
        assert_eq!(zoom(icons, between, 1), Some(Z::Icons(S::from_px(104.))));
        assert_eq!(zoom(icons, between, -1), Some(Z::Icons(S::LARGE)));
        let details = ViewMode::Details;
        assert_eq!(zoom(details, S::EXTRA_LARGE, 1), Some(Z::Icons(S::SMALL)));
        assert_eq!(zoom(details, S::LARGE, -1), None, "Details is the smallest");
        assert_eq!(zoom(details, S::LARGE, 3), Some(Z::Icons(S::MEDIUM)));
        assert_eq!(zoom(ViewMode::Columns, S::LARGE, 1), None);
    }

    #[test]
    fn named_sizes_keep_their_tiles_and_sizes_between_grow_smoothly() {
        for (size, tile) in ANCHORS {
            assert_eq!(size.tile(), tile, "{size:?}");
        }
        let all: Vec<IconSize> = (0..=200)
            .map(|i| IconSize::from_fraction(i as f32 / 200.))
            .collect();
        assert_eq!(all.first(), Some(&IconSize::SMALL));
        assert_eq!(all.last(), Some(&IconSize::EXTRA_LARGE));
        for pair in all.windows(2) {
            let (a, b) = (pair[0].tile(), pair[1].tile());
            assert!(a.w <= b.w && a.h <= b.h && a.image <= b.image && a.icon <= b.icon);
        }
        for size in &all {
            let t = size.tile();
            assert!(
                t.image < t.w && t.image < t.h,
                "{size:?}: image fits its tile"
            );
            assert!(t.icon <= t.image, "{size:?}: kind icon fits the image area");
        }
        assert!(IconSize::LADDER.windows(2).all(|w| w[0] < w[1]));
        assert!(ANCHORS.iter().all(|(s, _)| IconSize::LADDER.contains(s)));
    }

    #[test]
    fn slider_positions_snap_and_round_trip() {
        assert_eq!(IconSize::from_px(89.), IconSize::LARGE);
        assert_eq!(IconSize::from_px(91.), IconSize::from_px(92.));
        assert_eq!(IconSize::from_px(10.), IconSize::SMALL);
        assert_eq!(IconSize::from_px(1e6), IconSize::EXTRA_LARGE);
        assert_eq!(IconSize::from_px(f32::NAN), IconSize::EXTRA_LARGE);
        for size in IconSize::LADDER {
            assert_eq!(IconSize::from_fraction(size.fraction()), size);
        }
    }

    #[test]
    fn named_sizes_are_stored_by_name_others_in_pixels() {
        let json = |s: IconSize| serde_json::to_string(&s).unwrap();
        assert_eq!(json(IconSize::SMALL), r#""small""#);
        assert_eq!(json(IconSize::EXTRA_LARGE), r#""extra_large""#);
        assert_eq!(json(IconSize::from_px(120.)), "120");
        let back = |text: &str| serde_json::from_str::<IconSize>(text).unwrap();
        assert_eq!(back(r#""large""#), IconSize::LARGE);
        assert_eq!(back(r#""medium""#), IconSize::MEDIUM);
        assert_eq!(back("120.0"), IconSize::from_px(120.));
        assert_eq!(back("121"), IconSize::from_px(120.), "snapped");
        assert_eq!(back("5000"), IconSize::EXTRA_LARGE, "clamped");
        assert!(serde_json::from_str::<IconSize>(r#""huge""#).is_err());
    }
}
