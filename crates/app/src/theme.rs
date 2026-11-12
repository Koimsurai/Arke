//! Design tokens.
//!
//! Two color layers:
//! - **Raw** ([`Palette`]): a few solid colors per theme. Switching theme only swaps this layer.
//! - **Semantic** (the functions below): the only layer components may use. Text, lines and washes are
//!   ink at some opacity, with opacities tuned per theme for contrast targets (noted on [`LIGHT`] / [`DARK`]).
//!
//! Lengths always come from the scales (`S_*` spacing, `H_*` heights, `FS_*` font sizes, `R_*` radii).
//! Literal colors or ad hoc radii outside `theme.rs` are rejected by `tests::ui_code_uses_theme_tokens`.
//!
//! The accent (signal color) is configurable ([`set_accent`]). Any color keeps its hue and chroma and only
//! lightness is adjusted so it stays legible in both light and dark themes ([`accent_for`]).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use arke_core::filetype::FileKind;
use gpui::{BoxShadow, FontWeight, Hsla, Pixels, Rgba, point, px};

use crate::assets::Icon;

// --- Type ---

/// Default UI font.
pub(crate) const DEFAULT_UI_FONT: &str = "Segoe UI Variable Text";

/// User-selected UI font (`None` if unchanged).
static FONT: parking_lot::RwLock<Option<gpui::SharedString>> = parking_lot::RwLock::new(None);

/// UI font.
pub(crate) fn ui_font() -> gpui::SharedString {
    FONT.read()
        .clone()
        .unwrap_or_else(|| DEFAULT_UI_FONT.into())
}

/// Sets the UI font (empty string means default). Returns whether it changed.
pub(crate) fn set_font(name: &str) -> bool {
    let wanted =
        (!name.trim().is_empty()).then(|| gpui::SharedString::from(name.trim().to_owned()));
    let mut font = FONT.write();
    if *font == wanted {
        return false;
    }
    *font = wanted;
    drop(font);
    LOOK.fetch_add(1, Ordering::Relaxed);
    true
}

/// Simplified Chinese UI fonts, UI variant first.
const SIMPLIFIED_FONTS: [&str; 2] = ["Microsoft YaHei UI", "Microsoft YaHei"];
/// Traditional Chinese UI fonts, UI variant first.
const TRADITIONAL_FONTS: [&str; 2] = ["Microsoft JhengHei UI", "Microsoft JhengHei"];

/// CJK fallback fonts, in order, for a UI language (`zh-CN`, `zh-TW`, `en`) on a system whose
/// display locale maps to `system` (same names).
///
/// The default UI font has no CJK glyphs, so DirectWrite picks one by the *system* locale. Han
/// characters shared by both scripts (U+7F29 in 「压缩」) would then come out in Traditional
/// shapes on a zh-TW system even with a Simplified UI, or the reverse. Naming the fonts in the UI
/// language's order fixes that. English follows the system, which is what the file names on it
/// are most likely written in; other systems get no list and keep DirectWrite's own choice
/// (Japanese and Korean included).
pub(crate) fn cjk_fallbacks(ui: &str, system: &str) -> Vec<&'static str> {
    let simplified_first = match (ui, system) {
        ("zh-CN", _) | ("en", "zh-CN") => true,
        ("zh-TW", _) | ("en", "zh-TW") => false,
        _ => return Vec::new(),
    };
    let (first, second) = if simplified_first {
        (SIMPLIFIED_FONTS, TRADITIONAL_FONTS)
    } else {
        (TRADITIONAL_FONTS, SIMPLIFIED_FONTS)
    };
    first.into_iter().chain(second).collect()
}

/// Fallback fonts for the UI font under the current UI language (see [`set_ui_language`]).
static FALLBACKS: parking_lot::RwLock<Option<gpui::FontFallbacks>> = parking_lot::RwLock::new(None);

/// Orders the CJK fallbacks for UI language `ui` (system locale `system`). Returns whether they
/// changed; windows must repaint then. A custom font from settings stays the primary family, only
/// the fallbacks behind it change.
pub(crate) fn set_ui_language(ui: &str, system: &str) -> bool {
    let fonts = cjk_fallbacks(ui, system);
    let wanted = (!fonts.is_empty())
        .then(|| gpui::FontFallbacks::from_fonts(fonts.into_iter().map(str::to_owned).collect()));
    let mut fallbacks = FALLBACKS.write();
    if *fallbacks == wanted {
        return false;
    }
    *fallbacks = wanted;
    drop(fallbacks);
    LOOK.fetch_add(1, Ordering::Relaxed);
    true
}

/// Sets the UI font and its language-ordered fallbacks on an element; children inherit both.
/// Use this instead of `font_family(ui_font())`, which would lose the fallbacks.
pub(crate) trait UiFont: gpui::Styled + Sized {
    #[must_use]
    fn ui_font(mut self) -> Self {
        let style = self.text_style();
        style.font_family = Some(ui_font());
        style.font_fallbacks.clone_from(&FALLBACKS.read());
        self
    }
}

impl<T: gpui::Styled> UiFont for T {}

/// The UI font with its language fallbacks at `weight`, for measuring text before layout (the
/// breadcrumbs decide what to collapse from it).
pub(crate) fn ui_font_at(weight: FontWeight) -> gpui::Font {
    gpui::Font {
        family: ui_font(),
        features: gpui::FontFeatures::default(),
        fallbacks: FALLBACKS.read().clone(),
        weight,
        style: gpui::FontStyle::Normal,
    }
}

/// Background image under the list and its opacity (0-1).
static BACKDROP: parking_lot::RwLock<Option<(std::path::PathBuf, f32)>> =
    parking_lot::RwLock::new(None);

pub(crate) fn backdrop() -> Option<(std::path::PathBuf, f32)> {
    BACKDROP.read().clone()
}

/// Sets the background image (`None` for none). Returns whether it changed.
pub(crate) fn set_backdrop(image: Option<(std::path::PathBuf, f32)>) -> bool {
    let mut backdrop = BACKDROP.write();
    if *backdrop == image {
        return false;
    }
    *backdrop = image;
    drop(backdrop);
    LOOK.fetch_add(1, Ordering::Relaxed);
    true
}
pub(crate) const FW_STRONG: FontWeight = FontWeight::SEMIBOLD;

/// Keyboard shortcut badges only (`Ctrl T`, `Esc`); everything else reads at 12 px or more.
pub(crate) const FS_11: Pixels = px(11.);
/// Secondary text: dates, types, sizes, captions, hints.
pub(crate) const FS_12: Pixels = px(12.);
pub(crate) const FS_13: Pixels = px(13.);
/// Body: list names, menu items, sidebar items, section headings (WinUI's body size).
pub(crate) const FS_14: Pixels = px(14.);
pub(crate) const FS_15: Pixels = px(15.);
pub(crate) const FS_20: Pixels = px(20.);
/// Page titles (Home, Settings), semibold.
pub(crate) const FS_24: Pixels = px(24.);

// --- Spacing and heights (multiples of 4) ---

pub(crate) const S_2: Pixels = px(2.);
pub(crate) const S_4: Pixels = px(4.);
pub(crate) const S_6: Pixels = px(6.);
pub(crate) const S_8: Pixels = px(8.);
pub(crate) const S_12: Pixels = px(12.);
pub(crate) const S_16: Pixels = px(16.);
pub(crate) const S_20: Pixels = px(20.);
pub(crate) const S_24: Pixels = px(24.);
/// Max tooltip width; longer text wraps.
pub(crate) const TOOLTIP_MAX_W: Pixels = px(360.);
pub(crate) const S_32: Pixels = px(32.);

/// List row height (standard density).
pub(crate) const H_ROW: Pixels = px(32.);
/// List row height (compact, used in dual pane).
pub(crate) const H_ROW_COMPACT: Pixels = px(28.);
/// List row height (comfortable).
pub(crate) const H_ROW_COMFORTABLE: Pixels = px(40.);
/// Controls: buttons, inputs.
pub(crate) const H_CTL: Pixels = px(32.);
/// Small controls: context bar actions.
pub(crate) const H_CTL_SM: Pixels = px(26.);
/// Controls inside popovers: segments, filter buttons, menu rows.
pub(crate) const H_CTL_MD: Pixels = px(30.);
/// Frame top bar.
pub(crate) const H_BAR: Pixels = px(48.);
/// Context bar.
pub(crate) const H_CTX: Pixels = px(44.);
/// Column header row.
pub(crate) const H_HEADER: Pixels = px(30.);

/// Sidebar width, including the left margin.
pub(crate) const SIDEBAR_W: Pixels = px(248.);
pub(crate) const FILTER_W: Pixels = px(220.);
/// Narrowest the path bar gets on a short path, so it stays an easy click target; the rest of
/// the title bar drags the window.
pub(crate) const PATH_MIN_W: Pixels = px(200.);
/// Search options panel below the filter box (scope, filters, contents): wide enough for every
/// control on one line in each language.
pub(crate) const SEARCH_PANEL_W: Pixels = px(456.);
/// Widest value shown on a filter button (a long hand-typed term is cut off).
pub(crate) const FILTER_VALUE_MAX_W: Pixels = px(180.);
/// Narrowest filter menu in the search panel.
pub(crate) const FILTER_MENU_W: Pixels = px(208.);
/// The Name column's automatic width, until the user sizes it: what the other columns leave in
/// the pane, but at least `COL_NAME` and at most `COL_NAME_MAX` (see `explorer_layout.rs`).
pub(crate) const COL_NAME: Pixels = px(240.);
pub(crate) const COL_NAME_MAX: Pixels = px(480.);
pub(crate) const COL_MODIFIED: Pixels = px(140.);
pub(crate) const COL_TYPE: Pixels = px(130.);
pub(crate) const COL_SIZE: Pixels = px(84.);
pub(crate) const COL_EXTENSION: Pixels = px(72.);
pub(crate) const COL_ATTRIBUTES: Pixels = px(64.);
pub(crate) const COL_LINK_TARGET: Pixels = px(240.);
pub(crate) const COL_LOCATION: Pixels = px(260.);
/// One icon size of the grid: cell size, the area a thumbnail fills, and the kind icon drawn when
/// there is no thumbnail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Tile {
    pub(crate) w: Pixels,
    pub(crate) h: Pixels,
    pub(crate) image: Pixels,
    pub(crate) icon: Pixels,
}

// Height is the image area plus two lines of 12 px file name plus padding (52); both lines must
// fit. Width leaves the name 28 px more than the image, with a floor so short names still fit.
/// Small icons.
pub(crate) const TILE_SMALL: Tile = Tile {
    w: px(84.),
    h: px(100.),
    image: px(48.),
    icon: px(40.),
};
/// Medium icons.
pub(crate) const TILE_MEDIUM: Tile = Tile {
    w: px(96.),
    h: px(116.),
    image: px(64.),
    icon: px(48.),
};
/// Large icons (the default).
pub(crate) const TILE_LARGE: Tile = Tile {
    w: px(116.),
    h: px(140.),
    image: px(88.),
    icon: px(64.),
};
/// Extra large icons.
pub(crate) const TILE_XL: Tile = Tile {
    w: px(220.),
    h: px(244.),
    image: px(192.),
    icon: px(128.),
};
/// Details view tree: indent per nesting level and the expand chevron's box.
pub(crate) const TREE_INDENT: Pixels = px(18.);
pub(crate) const TREE_CHEVRON: Pixels = px(16.);
/// Quick Look card: the largest it grows on big screens, and the smallest gap it keeps to the
/// window edges (it otherwise takes a share of the window, see `explorer_preview.rs`).
pub(crate) const QL_MAX_W: Pixels = px(1280.);
pub(crate) const QL_MAX_H: Pixels = px(960.);
pub(crate) const QL_MARGIN: Pixels = px(24.);
/// Comfortable reading width for documents (Quick Look's Markdown).
pub(crate) const READ_W: Pixels = px(760.);
/// Quick Look's empty states: the kind icon, large.
pub(crate) const ICON_HERO: Pixels = px(96.);
/// Title icons (the preview pane's folder name, Quick Look's title bar).
pub(crate) const ICON_LG: Pixels = px(20.);
/// Icons.
pub(crate) const ICON: Pixels = px(16.);
pub(crate) const ICON_SM: Pixels = px(12.);

// --- Radii: outer - padding = inner ---

/// Badges, kbd, thumbnails.
pub(crate) const R_4: Pixels = px(4.);
/// Buttons, rows, inputs, menu items.
pub(crate) const R_8: Pixels = px(8.);
/// Cards, menus, dialogs.
pub(crate) const R_12: Pixels = px(12.);
/// Pills, progress bars.
pub(crate) const R_FULL: Pixels = px(999.);

// --- Raw layer ---

pub(crate) struct Palette {
    ground: Rgba,
    surface: Rgba,
    overlay: Rgba,
    ink: Rgba,
    signal: Rgba,
    danger: Rgba,
    /// Error InfoBar background (WinUI's critical severity fill): soft, so text stays in `ink`
    /// and only the icon carries the danger color.
    danger_surface: Rgba,
    /// Green check for cloud files available on this device (green, as in Windows Explorer).
    synced: Rgba,
    /// Folder compare: an item only one side has (blue, like an added line in a diff).
    diff_unique: Rgba,
    /// Folder compare: an item both sides have in different versions (amber).
    diff_changed: Rgba,
    on_signal: Rgba,
    veil: Rgba,
    /// Fraction of the signal color mixed into the frame.
    frame_tint: f32,
    a_muted: f32,
    a_subtle: f32,
    a_disabled: f32,
    a_line: f32,
    a_line_strong: f32,
    a_wash: f32,
    a_wash_strong: f32,
    a_signal_wash: f32,
    /// OKLCH lightness and chroma for file type colors.
    kind_l: f32,
    kind_c: f32,
}

const fn hex(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.,
        g: ((v >> 8) & 0xff) as f32 / 255.,
        b: (v & 0xff) as f32 / 255.,
        a: 1.,
    }
}

/// Light. Contrast on surface/ground: muted 8.4/7.8, subtle 5.0/4.8, signal 5.0/4.7.
pub(crate) const LIGHT: Palette = Palette {
    ground: hex(0xf3f3f3),
    surface: hex(0xfbfbfb),
    overlay: hex(0xffffff),
    ink: hex(0x1b1b1b),
    // A clean orange (Tailwind orange-700) rather than the old brick #b9491f, which read brown
    // next to the gray frame.
    signal: hex(0xc2410c),
    danger: hex(0xc4314b),
    danger_surface: hex(0xfde7e9),
    synced: hex(0x107c10),
    diff_unique: hex(0x0a5aa4),
    diff_changed: hex(0x875000),
    on_signal: hex(0xffffff),
    veil: hex(0x141414),
    frame_tint: 0.,
    a_muted: 0.78,
    a_subtle: 0.63,
    a_disabled: 0.36,
    a_line: 0.10,
    a_line_strong: 0.22,
    a_wash: 0.045,
    a_wash_strong: 0.08,
    a_signal_wash: 0.12,
    kind_l: 0.60,
    kind_c: 0.15,
};

/// Dark. Contrast on surface/ground: muted 7.6/8.1, subtle 4.8/5.0, signal 6.6/7.5.
pub(crate) const DARK: Palette = Palette {
    ground: hex(0x1c1c1c),
    surface: hex(0x272727),
    // Floating layers sit clearly above the paper (#272727); at #2c2c2c menus melted into it.
    overlay: hex(0x353535),
    ink: hex(0xf3f3f3),
    // Tailwind orange-400: saturated orange, not salmon.
    signal: hex(0xfb923c),
    danger: hex(0xf07a8c),
    danger_surface: hex(0x442726),
    synced: hex(0x6ccb5f),
    diff_unique: hex(0x60cdff),
    diff_changed: hex(0xf2c661),
    on_signal: hex(0x1a0f0a),
    veil: hex(0x000000),
    frame_tint: 0.,
    a_muted: 0.70,
    a_subtle: 0.52,
    a_disabled: 0.32,
    a_line: 0.09,
    a_line_strong: 0.20,
    a_wash: 0.05,
    a_wash_strong: 0.09,
    // The signal is bright, so its wash stays low enough to keep text at 10:1.
    a_signal_wash: 0.15,
    kind_l: 0.74,
    kind_c: 0.12,
};

static IS_DARK: AtomicBool = AtomicBool::new(false);
static MICA: AtomicBool = AtomicBool::new(false);
/// Bumped on every mode or accent change. Each window remembers the version it drew and repaints fully on mismatch.
static LOOK: AtomicU32 = AtomicU32::new(0);

/// Another display preference changed (list columns, date format, ...); all windows must repaint.
pub(crate) fn touch() {
    LOOK.fetch_add(1, Ordering::Relaxed);
}

/// Current look version (mode, accent).
pub(crate) fn look() -> u32 {
    LOOK.load(Ordering::Relaxed)
}

// --- Accent ---

/// No custom accent; use the theme's Arke orange.
const NO_ACCENT: u32 = u32::MAX;
/// Custom accent: light theme signal and its text color, then dark theme signal and its text color (0xRRGGBB).
static ACCENT: [AtomicU32; 4] = [
    AtomicU32::new(NO_ACCENT),
    AtomicU32::new(NO_ACCENT),
    AtomicU32::new(NO_ACCENT),
    AtomicU32::new(NO_ACCENT),
];

/// Minimum contrast of the signal color against the background when used for text or icons.
const ACCENT_CONTRAST: f32 = 4.5;
/// Max OKLCH lightness of the dark theme signal; brighter and text on the wash drops below 10:1.
const ACCENT_DARK_MAX_L: f32 = 0.75;

/// Settings page swatches (besides Arke orange). Names are locale key suffixes.
pub(crate) const ACCENT_PRESETS: [(&str, u32); 9] = [
    ("blue", 0x0078d4),
    ("indigo", 0x5b5fc7),
    ("purple", 0x8e4ec6),
    ("pink", 0xd6338a),
    ("red", 0xd13438),
    ("amber", 0xc27c0e),
    ("green", 0x2e8b3a),
    ("teal", 0x0e8a8a),
    ("graphite", 0x69797e),
];

/// Color of one hue strip cell (`hue` is the OKLCH angle).
pub(crate) fn accent_from_hue(hue: f32) -> u32 {
    pack(in_gamut(0.62, 0.16, hue))
}

/// Sets the accent (`None` restores Arke orange). Returns whether it changed; windows must repaint if so.
pub(crate) fn set_accent(color: Option<u32>) -> bool {
    let values = match color {
        None => [NO_ACCENT; 4],
        Some(rgb) => {
            let color = hex(rgb & 0x00ff_ffff);
            let (light, on_light) = accent_for(color, &LIGHT);
            let (dark, on_dark) = accent_for(color, &DARK);
            [pack(light), pack(on_light), pack(dark), pack(on_dark)]
        }
    };
    let mut changed = false;
    for (slot, value) in ACCENT.iter().zip(values) {
        changed |= slot.swap(value, Ordering::Relaxed) != value;
    }
    if changed {
        LOOK.fetch_add(1, Ordering::Relaxed);
    }
    changed
}

/// Default accent (Arke orange) as it appears in the light theme, for the settings swatches and hex input.
pub(crate) const ARKE_ACCENT: u32 = 0x00c2_410c;

/// Signal color actually used for `color` in the current theme. Swatches use it so they match the result.
pub(crate) fn accent_preview(color: u32) -> Rgba {
    accent_for(hex(color & 0x00ff_ffff), p()).0
}

/// Default accent (Arke orange) in the current theme.
pub(crate) fn accent_default() -> Rgba {
    p().signal
}

/// Turns a user color into this theme's signal color plus the text color drawn on it.
///
/// Hue and chroma are kept; only lightness moves (darker in light theme, lighter in dark theme) until it reaches
/// [`ACCENT_CONTRAST`] against both paper and frame. Colors that already pass are left unchanged.
fn accent_for(color: Rgba, palette: &Palette) -> (Rgba, Rgba) {
    let (l, c, h) = to_oklch(color);
    let dark_theme = luminance(palette.surface) < 0.2;
    let clear_enough = |candidate: Rgba| {
        contrast(candidate, palette.surface) >= ACCENT_CONTRAST
            && contrast(candidate, palette.ground) >= ACCENT_CONTRAST
    };
    let mut signal = Rgba { a: 1., ..color };
    let too_bright = dark_theme && l > ACCENT_DARK_MAX_L;
    if too_bright || !clear_enough(signal) {
        // Step toward legibility and stop at the first passing lightness, the closest to the original.
        let mut lightness = if too_bright {
            ACCENT_DARK_MAX_L
        } else {
            l.clamp(0.06, 0.97)
        };
        let step = if dark_theme { 0.005 } else { -0.005 };
        loop {
            signal = in_gamut(lightness, c, h);
            if clear_enough(signal) || !(0.05..=0.98).contains(&lightness) {
                break;
            }
            lightness += step;
        }
    }
    // Text on it: white or a very dark shade of the same hue, whichever contrasts more.
    let white = hex(0xffffff);
    let deep = in_gamut(0.17, 0.02, h);
    let on = if contrast(white, signal) >= contrast(deep, signal) {
        white
    } else {
        deep
    };
    (signal, on)
}

fn pack(c: Rgba) -> u32 {
    let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
    (channel(c.r) << 16) | (channel(c.g) << 8) | channel(c.b)
}

/// Custom accent for the current theme (`None` if not set). `which`: 0 is the signal, 1 the text on it.
fn accent(which: usize) -> Option<Rgba> {
    let slot = if is_dark() { 2 } else { 0 } + which;
    let value = ACCENT[slot].load(Ordering::Relaxed);
    (value != NO_ACCENT).then(|| hex(value))
}

fn luminance(c: Rgba) -> f32 {
    let lin = |v: f32| {
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast; a translucent `fg` is composited onto `bg` first.
fn contrast(fg: Rgba, bg: Rgba) -> f32 {
    let fg = mix(bg, Rgba { a: 1., ..fg }, fg.a);
    let (a, b) = (luminance(fg), luminance(bg));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Whether Mica is available (Windows 11 and not disabled by environment variable). Set once at startup.
static MICA_AVAILABLE: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_mica_available(on: bool) {
    MICA_AVAILABLE.store(on, Ordering::Relaxed);
    MICA.store(on, Ordering::Relaxed);
}

pub(crate) fn mica_available() -> bool {
    MICA_AVAILABLE.load(Ordering::Relaxed)
}

/// Mica window background (Windows 11): the frame becomes translucent and shows the material.
/// Off when the user picks Solid; cannot turn on where unavailable.
pub(crate) fn set_mica(on: bool) {
    let on = on && mica_available();
    if MICA.swap(on, Ordering::Relaxed) != on {
        LOOK.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn mica() -> bool {
    MICA.load(Ordering::Relaxed)
}

// --- Background tint ---

/// Frame (sidebar, top bar) tint: light theme ground, dark theme ground (0xRRGGBB).
static TINT: [AtomicU32; 2] = [AtomicU32::new(NO_ACCENT), AtomicU32::new(NO_ACCENT)];
/// Paper gets a lighter tint: light and dark surface, plus the translucent paper base under Mica (light, dark).
static PAPER_TINT: [AtomicU32; 4] = [
    AtomicU32::new(NO_ACCENT),
    AtomicU32::new(NO_ACCENT),
    AtomicU32::new(NO_ACCENT),
    AtomicU32::new(NO_ACCENT),
];

/// Max tint chroma (OKLCH): visible without competing with content.
const TINT_CHROMA: f32 = 0.035;
/// Paper gets half the tint: same hue as the frame, without skewing file icon and thumbnail colors.
const PAPER_TINT_CHROMA: f32 = 0.016;
/// Base color of the translucent paper under Mica (light, dark theme).
const MICA_PAPER: [u32; 2] = [0x00ff_ffff, 0x003a_3a3a];

/// Tints the frame with the hue of `color` (`None` is neutral gray). Returns whether it changed.
pub(crate) fn set_tint(color: Option<u32>) -> bool {
    let values = match color {
        None => [NO_ACCENT; 2],
        Some(rgb) => {
            let color = hex(rgb & 0x00ff_ffff);
            [pack(tint_for(color, &LIGHT)), pack(tint_for(color, &DARK))]
        }
    };
    let paper = match color {
        None => [NO_ACCENT; 4],
        Some(rgb) => {
            let color = hex(rgb & 0x00ff_ffff);
            let like = |base: Rgba| pack(tint_like(color, base, PAPER_TINT_CHROMA));
            [
                like(LIGHT.surface),
                like(DARK.surface),
                like(hex(MICA_PAPER[0])),
                like(hex(MICA_PAPER[1])),
            ]
        }
    };
    let mut changed = false;
    for (slot, value) in TINT.iter().zip(values) {
        changed |= slot.swap(value, Ordering::Relaxed) != value;
    }
    for (slot, value) in PAPER_TINT.iter().zip(paper) {
        changed |= slot.swap(value, Ordering::Relaxed) != value;
    }
    if changed {
        LOOK.fetch_add(1, Ordering::Relaxed);
    }
    changed
}

/// Paper tinted with `color` in the current theme, for the settings preview.
pub(crate) fn paper_preview(color: Option<u32>) -> Rgba {
    let surface = p().surface;
    color.map_or(surface, |rgb| {
        tint_like(hex(rgb & 0x00ff_ffff), surface, PAPER_TINT_CHROMA)
    })
}

/// Frame tinted with `color` in the current theme, for the settings preview (`None` is neutral gray).
pub(crate) fn tint_preview(color: Option<u32>) -> Rgba {
    let palette = p();
    color.map_or(palette.ground, |rgb| {
        tint_for(hex(rgb & 0x00ff_ffff), palette)
    })
}

/// A mini window drawn in the light or dark theme whatever the current one is, for the Theme
/// preview tiles in Settings. Follows the chosen accent and background tint.
pub(crate) struct ModePreview {
    pub(crate) frame: Rgba,
    pub(crate) paper: Rgba,
    /// Text lines and the selected row's label.
    pub(crate) ink: Rgba,
    /// Secondary lines and the sidebar.
    pub(crate) ink_faint: Rgba,
    pub(crate) accent: Rgba,
}

pub(crate) fn mode_preview(dark: bool) -> ModePreview {
    let palette = if dark { &DARK } else { &LIGHT };
    let slot = usize::from(dark);
    let stored = |value: u32| (value != NO_ACCENT).then(|| hex(value));
    ModePreview {
        frame: stored(TINT[slot].load(Ordering::Relaxed)).unwrap_or(palette.ground),
        paper: stored(PAPER_TINT[slot].load(Ordering::Relaxed)).unwrap_or(palette.surface),
        ink: alpha(palette.ink, palette.a_muted),
        ink_faint: alpha(palette.ink, palette.a_disabled),
        accent: stored(ACCENT[slot * 2].load(Ordering::Relaxed)).unwrap_or(palette.signal),
    }
}

/// Frame color: same lightness as the original ground (text contrast unchanged), with the hue and a little chroma of `color`.
fn tint_for(color: Rgba, palette: &Palette) -> Rgba {
    tint_like(color, palette.ground, TINT_CHROMA)
}

/// Lightness of `base`, hue of `color`, chroma capped at `max_chroma`.
fn tint_like(color: Rgba, base: Rgba, max_chroma: f32) -> Rgba {
    let (_, chroma, hue) = to_oklch(color);
    let (lightness, _, _) = to_oklch(base);
    in_gamut(lightness, chroma.min(max_chroma), hue)
}

/// Tinted paper (`which`: 0 is surface, 2 the Mica paper base), or `None` if untinted.
fn paper_tint(which: usize) -> Option<Rgba> {
    let value = PAPER_TINT[which + usize::from(is_dark())].load(Ordering::Relaxed);
    (value != NO_ACCENT).then(|| hex(value))
}

/// Window ground, tinted when a background tint is set.
fn ground() -> Rgba {
    let value = TINT[usize::from(is_dark())].load(Ordering::Relaxed);
    if value == NO_ACCENT {
        p().ground
    } else {
        hex(value)
    }
}

fn tinted() -> bool {
    TINT[usize::from(is_dark())].load(Ordering::Relaxed) != NO_ACCENT
}

/// Switches theme to follow the system light/dark mode. Windows must repaint afterwards.
pub(crate) fn set_dark(dark: bool) {
    if IS_DARK.swap(dark, Ordering::Relaxed) != dark {
        LOOK.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn is_dark() -> bool {
    IS_DARK.load(Ordering::Relaxed)
}

fn p() -> &'static Palette {
    if is_dark() { &DARK } else { &LIGHT }
}

const fn alpha(c: Rgba, a: f32) -> Rgba {
    Rgba { a, ..c }
}

/// Interpolates between two colors including alpha, for animation frames.
pub(crate) fn blend(from: Rgba, to: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0., 1.);
    Rgba {
        r: from.r + (to.r - from.r) * t,
        g: from.g + (to.g - from.g) * t,
        b: from.b + (to.b - from.b) * t,
        a: from.a + (to.a - from.a) * t,
    }
}

fn mix(base: Rgba, other: Rgba, t: f32) -> Rgba {
    Rgba {
        r: base.r + (other.r - base.r) * t,
        g: base.g + (other.g - base.g) * t,
        b: base.b + (other.b - base.b) * t,
        a: 1.,
    }
}

// --- Semantic: surfaces ---

/// Frame the sidebar and top bar are drawn on. Nearly transparent under Mica, with a slight scrim
/// so text stays readable on bright wallpapers.
pub(crate) fn frame() -> Rgba {
    let base = mix(ground(), signal(), p().frame_tint);
    if !mica() {
        return base;
    }
    // Use a heavier layer when tinted, otherwise the material washes out the color.
    alpha(base, if tinted() { 0.78 } else { 0.35 })
}

/// Paper (list, preview): a translucent layer over the material under Mica (like WinUI's Layer), solid surface otherwise.
pub(crate) fn paper() -> Rgba {
    if !mica() {
        return surface();
    }
    let base = paper_tint(2).unwrap_or_else(|| hex(MICA_PAPER[usize::from(is_dark())]));
    alpha(base, if is_dark() { 0.45 } else { 0.7 })
}

/// Selected items in navigation-style lists (sidebar, recycle bin rows, settings pages): a
/// neutral wash strong enough to read at a glance on its own.
pub(crate) fn selection() -> Rgba {
    alpha(p().ink, if is_dark() { 0.13 } else { 0.10 })
}

/// Selected rows in the file list and keyboard-driven pickers: the accent wash while the list
/// has keyboard focus (like Files and File Explorer), a neutral one once focus moves elsewhere,
/// so the active pane stands out in split view.
pub(crate) fn list_selection(focused: bool) -> Rgba {
    if focused {
        signal_wash()
    } else {
        wash_strong()
    }
}

/// Settings cards: on the paper, slightly lighter (like Windows 11 Settings cards). Border uses `line()`.
pub(crate) fn card() -> Rgba {
    if is_dark() {
        alpha(hex(0xffffff), 0.045)
    } else {
        alpha(hex(0xffffff), 0.8)
    }
}
/// Paper: file list, preview.
pub(crate) fn surface() -> Rgba {
    paper_tint(0).unwrap_or_else(|| p().surface)
}
/// Overlays: menus, tooltips, dialogs.
pub(crate) fn overlay() -> Rgba {
    p().overlay
}
/// Dialog backdrop.
pub(crate) fn veil() -> Rgba {
    alpha(p().veil, 0.32)
}
/// Lighter backdrop for transient overlays (Ctrl+T): dims the window enough to focus the panel
/// without hiding where you are.
pub(crate) fn scrim() -> Rgba {
    alpha(p().veil, 0.20)
}
/// Quick Look backdrop: darker than the dialog veil, so the window recedes and the media in front
/// reads like a photo viewer.
pub(crate) fn scrim_strong() -> Rgba {
    alpha(p().veil, if is_dark() { 0.55 } else { 0.40 })
}
/// Quick Look's stage behind images and PDF pages: the frame color, a step below the card, so
/// media edges stay visible without a border.
pub(crate) fn stage() -> Rgba {
    p().ground
}
pub(crate) fn clear() -> Rgba {
    alpha(p().ink, 0.)
}

// --- Semantic: text ---

pub(crate) fn text() -> Rgba {
    p().ink
}
/// Secondary (>= 7:1): dates, types, sizes.
pub(crate) fn text_muted() -> Rgba {
    alpha(p().ink, p().a_muted)
}
/// Tertiary (>= 4.5:1): column headers, section labels, shortcut hints.
pub(crate) fn text_subtle() -> Rgba {
    alpha(p().ink, p().a_subtle)
}
/// Tertiary text on floating surfaces (Quick Look's details line and key hints): the dark
/// overlay is lighter than the paper, so the subtle ink is lifted a little to keep 4.5:1.
pub(crate) fn text_subtle_raised() -> Rgba {
    alpha(p().ink, subtle_raised(p()))
}
const fn subtle_raised(p: &Palette) -> f32 {
    p.a_subtle + RAISED_LIFT
}
/// Extra ink for subtle text on overlays.
const RAISED_LIFT: f32 = 0.05;
pub(crate) fn text_disabled() -> Rgba {
    alpha(p().ink, p().a_disabled)
}
pub(crate) fn placeholder() -> Hsla {
    Hsla::from(text_subtle())
}

// --- Semantic: lines and fills ---

pub(crate) fn line() -> Rgba {
    alpha(p().ink, p().a_line)
}
pub(crate) fn line_strong() -> Rgba {
    alpha(p().ink, p().a_line_strong)
}
/// Hover, input background.
pub(crate) fn wash() -> Rgba {
    alpha(p().ink, p().a_wash)
}
/// Pressed, active toggles, unfocused selection.
pub(crate) fn wash_strong() -> Rgba {
    alpha(p().ink, p().a_wash_strong)
}

// --- Semantic: signal ---

/// Accent (Arke orange by default): selection, focus, primary buttons, progress.
pub(crate) fn signal() -> Rgba {
    accent(0).unwrap_or_else(|| p().signal)
}
/// Selected row.
pub(crate) fn signal_wash() -> Rgba {
    alpha(signal(), p().a_signal_wash)
}
/// Keyboard cursor and focus rings.
pub(crate) fn signal_line() -> Rgba {
    alpha(signal(), 0.55)
}
/// Text selection.
pub(crate) fn text_selection() -> Rgba {
    alpha(signal(), 0.28)
}
/// Text on the signal color.
pub(crate) fn on_signal() -> Rgba {
    accent(1).unwrap_or_else(|| p().on_signal)
}
/// Only for destructive actions and errors, always paired with an icon.
pub(crate) fn danger() -> Rgba {
    p().danger
}
/// Error InfoBar fill; text on it stays `text()` (>= 10:1), only the icon uses `danger()`.
pub(crate) fn danger_surface() -> Rgba {
    p().danger_surface
}
/// Cloud file available on this device (status icon, always paired with an icon).
pub(crate) fn synced() -> Rgba {
    p().synced
}
// Folder compare badge for an item only one side has; always a text label, never color alone.
pub(crate) fn diff_unique() -> Rgba {
    p().diff_unique
}
/// Folder compare badge for an item that differs between the sides (newer, older, different).
pub(crate) fn diff_changed() -> Rgba {
    p().diff_changed
}
/// Fill behind a folder compare badge, from its text color.
pub(crate) const fn diff_wash(color: Rgba) -> Rgba {
    alpha(color, 0.12)
}
/// Cloud file waiting for or in the middle of a transfer (status icon, paired with its tooltip).
pub(crate) fn syncing() -> Rgba {
    text_muted()
}

// --- Shadows ---

fn hairline(color: Rgba) -> BoxShadow {
    BoxShadow {
        color: color.into(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(1.),
        inset: false,
    }
}

/// A 1 px ring just inside the element, over its background (but under its children).
fn inner_hairline(color: Rgba) -> BoxShadow {
    BoxShadow {
        inset: true,
        ..hairline(color)
    }
}

/// Dark theme: the 1 px near-black outline around raised surfaces. Gray-on-gray layers need a
/// crisp dark edge to separate (WinUI does the same); the light theme doesn't.
fn dark_outline() -> BoxShadow {
    hairline(alpha(hex(0x000000), 0.6))
}

/// Dark theme: the light inner edge of floating surfaces (ink at this opacity), so menus and
/// popovers read as lit from above instead of blending into the paper.
const FLOAT_EDGE: f32 = 0.14;

fn drop_shadow(color: Rgba, y: f32, blur: f32) -> BoxShadow {
    BoxShadow {
        color: color.into(),
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    }
}

/// Cards: a single hairline border.
pub(crate) fn elev_1() -> Vec<BoxShadow> {
    vec![hairline(line())]
}
/// Raised paper (the list sheet, Home, Settings).
///
/// The first shadow is always the outer edge (`elev_2_active` swaps it for the accent). In the
/// dark theme the paper gets a near-black outline plus a faint inner line, except under Mica:
/// drop shadows are painted as full rectangles under the element, and Mica paper is
/// translucent, so a dark outline there would darken the whole sheet.
pub(crate) fn elev_2() -> Vec<BoxShadow> {
    let drop = drop_shadow(
        alpha(p().veil, if is_dark() { 0.35 } else { 0.10 }),
        8.,
        24.,
    );
    if is_dark() && !mica() {
        vec![dark_outline(), drop, inner_hairline(line())]
    } else {
        vec![hairline(line()), drop]
    }
}
/// Active paper in dual pane: the outer edge uses the signal color.
pub(crate) fn elev_2_active() -> Vec<BoxShadow> {
    let mut shadows = elev_2();
    shadows[0] = hairline(signal_line());
    shadows
}
/// Floating surfaces: menus, popovers, tooltips, toasts, drag previews. They are always opaque
/// (`overlay()`), so the dark theme can use the outline plus a lit inner edge.
pub(crate) fn elev_float() -> Vec<BoxShadow> {
    let drop = drop_shadow(
        alpha(p().veil, if is_dark() { 0.35 } else { 0.10 }),
        8.,
        24.,
    );
    if is_dark() {
        vec![
            dark_outline(),
            drop,
            inner_hairline(alpha(p().ink, FLOAT_EDGE)),
        ]
    } else {
        vec![hairline(line()), drop]
    }
}
/// Dialogs, Ctrl+T, Quick Look.
pub(crate) fn elev_3() -> Vec<BoxShadow> {
    let drop = drop_shadow(
        alpha(p().veil, if is_dark() { 0.5 } else { 0.22 }),
        24.,
        56.,
    );
    if is_dark() {
        vec![
            dark_outline(),
            drop,
            inner_hairline(alpha(p().ink, FLOAT_EDGE)),
        ]
    } else {
        vec![hairline(line()), drop]
    }
}

// --- Segmented controls (view switcher, search scope) ---

/// The groove the segments sit in. It is recessed (darker than its background) so the selected
/// segment reads as a raised pill; the dark theme darkens instead of lightening, because the
/// thumb there is the overlay gray.
pub(crate) fn segment_track() -> Rgba {
    if is_dark() {
        alpha(hex(0x000000), 0.22)
    } else {
        alpha(p().ink, 0.06)
    }
}
/// The selected segment: white in the light theme, the overlay gray in the dark one.
pub(crate) fn segment_thumb() -> Rgba {
    p().overlay
}
/// The selected segment's lift: a hairline and a 1 px soft shadow.
pub(crate) fn elev_thumb() -> Vec<BoxShadow> {
    vec![
        hairline(line()),
        drop_shadow(alpha(p().veil, if is_dark() { 0.3 } else { 0.08 }), 1., 2.),
    ]
}

// --- File icons ---

/// Two-tone folder colors (same in both themes).
pub(crate) fn folder_colors() -> (Rgba, Rgba) {
    static C: OnceLock<(Rgba, Rgba)> = OnceLock::new();
    *C.get_or_init(|| (oklch(0.74, 0.14, 75.), oklch(0.85, 0.14, 87.)))
}

/// Large folder art (grid, preview), same in both themes: the back panel a little deeper than the
/// small icon's so the front flap stands off it, the paper peeking out between them, and the lit
/// top edge of the front flap.
pub(crate) fn folder_depth_colors() -> (Rgba, Rgba, Rgba) {
    static C: OnceLock<(Rgba, Rgba, Rgba)> = OnceLock::new();
    *C.get_or_init(|| {
        (
            oklch(0.72, 0.14, 72.),
            page_colors().0,
            oklch(0.92, 0.09, 92.),
        )
    })
}

/// Line and soft fill of a colored place icon (known folders, OneDrive, drives) in the sidebar
/// and on Home, or `None` for icons drawn in the text color. Same rule as the file types: this
/// theme's fixed lightness and chroma with one hue per place, so no place shouts; devices are a
/// muted steel. The fill is the line color, translucent, so it follows the surface under it.
pub(crate) fn place_tint(icon: Icon) -> Option<(Rgba, Rgba)> {
    place_tint_in(icon, p(), is_dark())
}

fn place_tint_in(icon: Icon, palette: &Palette, dark: bool) -> Option<(Rgba, Rgba)> {
    let (hue, chroma) = match icon {
        Icon::KfHome => (50., 1.),
        Icon::KfUser => (295., 1.),
        Icon::KfDesktop => (215., 1.),
        Icon::KfDownload => (145., 1.),
        Icon::KfDocuments => (260., 1.),
        Icon::KfPictures => (325., 1.),
        Icon::KfMusic => (180., 1.),
        Icon::KfVideos => (10., 1.),
        Icon::Cloud => (235., 1.),
        Icon::Drive | Icon::NetDrive | Icon::Terminal => (250., 0.4),
        _ => return None,
    };
    let line = in_gamut(palette.kind_l, palette.kind_c * chroma, hue);
    Some((line, alpha(line, if dark { 0.24 } else { 0.18 })))
}

/// Page fill and outline (same in both themes: white paper).
pub(crate) const fn page_colors() -> (Rgba, Rgba) {
    (hex(0xfdfdfd), hex(0xa3a8b0))
}

/// Paper for small icons (16 px in the list). The glyph is too small to read, so the page is tinted with the type color instead.
pub(crate) fn small_page_colors(glyph: Rgba) -> (Rgba, Rgba) {
    let (fill, line) = page_colors();
    (mix(fill, glyph, 0.22), mix(line, glyph, 0.45))
}

/// Neutral glyph on the file paper (text, apps, shortcuts, ...): the light theme's subtle text over
/// white, kept opaque so the small-icon paper tint mixed from it stays opaque too.
const PAPER_NEUTRAL: Rgba = hex(0x6f6f6f);

/// Glyph and color per file type. Hues are evenly spaced with fixed lightness and chroma so no type stands out.
/// Uncommon types and types with their own system icon use a neutral color.
///
/// The glyph sits on the paper, which is white in both themes ([`page_colors`]), so its colors
/// don't follow the theme: the dark theme's light text and lighter type colors would wash out on
/// white (a text file's lines disappeared entirely).
pub(crate) fn file_glyph(kind: FileKind) -> Option<(Icon, Rgba)> {
    let hue = |h: f32| oklch(LIGHT.kind_l, LIGHT.kind_c, h);
    let neutral = PAPER_NEUTRAL;
    Some(match kind {
        FileKind::Folder | FileKind::Other => return None,
        FileKind::Pdf => (Icon::FtPdf, hue(25.)),
        FileKind::Presentation => (Icon::FtSlides, hue(60.)),
        FileKind::Archive => (Icon::FtArchive, hue(90.)),
        FileKind::Spreadsheet => (Icon::FtSheet, hue(145.)),
        FileKind::Audio => (Icon::FtAudio, hue(190.)),
        FileKind::Document => (Icon::FtDoc, hue(250.)),
        FileKind::Code => (Icon::FtCode, hue(280.)),
        FileKind::Image => (Icon::FtImage, hue(315.)),
        FileKind::Video => (Icon::FtVideo, hue(355.)),
        FileKind::Text => (Icon::FtText, neutral),
        FileKind::Executable => (Icon::FtApp, neutral),
        FileKind::Shortcut => (Icon::FtShortcut, neutral),
        FileKind::Font => (Icon::FtFont, neutral),
        FileKind::DiskImage => (Icon::FtDisk, neutral),
    })
}

/// Code colors in the preview: same lightness and chroma as file types, different hues; comments use subtle text.
pub(crate) fn code_color(token: arke_core::highlight::Token) -> Rgba {
    use arke_core::highlight::Token;
    let hue = |h: f32| oklch(p().kind_l, p().kind_c, h);
    match token {
        Token::Keyword => hue(300.),
        Token::String => hue(145.),
        Token::Number => hue(60.),
        Token::Comment => text_subtle(),
    }
}

/// OKLCH to sRGB (`h` in degrees).
fn oklch(l: f32, c: f32, h: f32) -> Rgba {
    let lin = oklch_linear(l, c, h);
    let gamma = |x: f32| {
        let x = x.clamp(0., 1.);
        if x <= 0.003_130_8 {
            12.92 * x
        } else {
            1.055 * x.powf(1. / 2.4) - 0.055
        }
    };
    Rgba {
        r: gamma(lin[0]),
        g: gamma(lin[1]),
        b: gamma(lin[2]),
        a: 1.,
    }
}

/// OKLCH to linear sRGB, unclamped; values outside 0-1 are out of the display gamut.
fn oklch_linear(l: f32, c: f32, h: f32) -> [f32; 3] {
    let (sin, cos) = h.to_radians().sin_cos();
    let (a, b) = (c * cos, c * sin);
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
    [
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
    ]
}

/// Displayable color at this lightness and hue with chroma at most `c`. Reduces chroma when out of gamut so the hue does not shift.
fn in_gamut(l: f32, c: f32, h: f32) -> Rgba {
    let mut chroma = c;
    for _ in 0..40 {
        let lin = oklch_linear(l, chroma, h);
        if lin.iter().all(|v| (-0.0005..=1.0005).contains(v)) {
            break;
        }
        chroma *= 0.92;
    }
    oklch(l, chroma, h)
}

/// sRGB to OKLCH (lightness, chroma, hue in degrees).
fn to_oklch(color: Rgba) -> (f32, f32, f32) {
    let lin = |v: f32| {
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(color.r), lin(color.g), lin(color.b));
    let l_ = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m_ = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s_ = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    let l = 0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.004_072_047 * s_;
    let a = 1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_;
    let b = 0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_;
    (l, a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colored place icons read as graphics (3:1) on the sidebar and the paper in both themes,
    /// and no two places share a color.
    #[test]
    fn place_tints_read_in_both_themes() {
        let places = [
            Icon::KfHome,
            Icon::KfUser,
            Icon::KfDesktop,
            Icon::KfDownload,
            Icon::KfDocuments,
            Icon::KfPictures,
            Icon::KfMusic,
            Icon::KfVideos,
            Icon::Cloud,
            Icon::Drive,
        ];
        for (dark, p) in [(false, &LIGHT), (true, &DARK)] {
            let lines = places.map(|icon| {
                let (line, _) = place_tint_in(icon, p, dark).expect("tinted");
                for bg in [p.surface, p.ground] {
                    assert!(
                        contrast(line, bg) >= 3.0,
                        "{icon:?} dark={dark}: {:.2}",
                        contrast(line, bg)
                    );
                }
                pack(line)
            });
            let mut unique = lines.to_vec();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), lines.len(), "dark={dark}");
        }
        assert!(place_tint_in(Icon::Copy, &LIGHT, false).is_none());
    }

    /// Every text role meets its contrast target in both themes on both backgrounds.
    #[test]
    fn text_roles_meet_contrast_targets() {
        // Read the palettes directly instead of the global mode and accent, which other tests change concurrently.
        for (dark, p) in [(false, &LIGHT), (true, &DARK)] {
            let selection = if dark { 0.13 } else { 0.10 };
            for bg in [p.surface, p.ground] {
                assert!(
                    contrast(alpha(p.ink, p.a_muted), bg) >= 7.0,
                    "muted dark={dark}"
                );
                assert!(
                    contrast(alpha(p.ink, p.a_subtle), bg) >= 4.5,
                    "subtle dark={dark}"
                );
                assert!(contrast(p.signal, bg) >= 4.5, "signal dark={dark}");
            }
            assert!(contrast(p.ink, mix(p.surface, p.signal, p.a_signal_wash)) >= 10.0);
            // The neutral selection is deliberately strong (it has to show on gray paper), which
            // costs a little contrast in the dark theme: still far above AAA's 7:1.
            assert!(
                contrast(p.ink, mix(p.surface, p.ink, selection)) >= 9.0,
                "selection dark={dark}: {:.2}",
                contrast(p.ink, mix(p.surface, p.ink, selection))
            );
            assert!(contrast(p.ink, mix(p.surface, p.ink, p.a_wash_strong)) >= 10.0);
            // Folder compare badges: their text on their own wash over the paper.
            for color in [p.diff_unique, p.diff_changed] {
                let washed = mix(p.surface, color, 0.12);
                assert!(
                    contrast(color, washed) >= 4.5,
                    "compare badge dark={dark}: {:.2}",
                    contrast(color, washed)
                );
            }
            // Error InfoBar: body text stays ink, and the danger icon still reads on the fill.
            assert!(
                contrast(p.ink, p.danger_surface) >= 10.0,
                "infobar text dark={dark}"
            );
            assert!(
                contrast(p.danger, p.danger_surface) >= 3.0,
                "infobar icon dark={dark}"
            );
            // Quick Look's stage (the frame color) is darker than the card around it, and the
            // hints and counters drawn on the card read.
            assert!(
                luminance(p.ground) < luminance(p.overlay),
                "stage dark={dark}"
            );
            assert!(
                contrast(alpha(p.ink, subtle_raised(p)), p.overlay) >= 4.5,
                "subtle on card dark={dark}"
            );
            // Floating surfaces sit above the paper they cover.
            assert!(
                luminance(p.overlay) >= luminance(p.surface),
                "overlay dark={dark}"
            );
        }
    }

    /// Any accent stays legible in both themes, and text on its wash still reaches 10:1.
    #[test]
    fn any_accent_is_legible_in_both_themes() {
        let mut colors: Vec<u32> = ACCENT_PRESETS.iter().map(|&(_, c)| c).collect();
        colors.extend((0..36).map(|i| accent_from_hue(i as f32 * 10.)));
        // Extremes: black, white, pure colors, very light, very dark.
        colors.extend([
            0x000000, 0xffffff, 0xff0000, 0x00ff00, 0x0000ff, 0xffff00, 0x00ffff, 0xff00ff,
            0xfff4c2, 0x10203a, 0x808080,
        ]);
        for rgb in colors {
            for palette in [&LIGHT, &DARK] {
                let (signal, on) = accent_for(hex(rgb), palette);
                for bg in [palette.surface, palette.ground] {
                    assert!(
                        contrast(signal, bg) >= ACCENT_CONTRAST,
                        "{rgb:06x} as signal color is too faint: {:.2}",
                        contrast(signal, bg)
                    );
                }
                assert!(
                    contrast(on, signal) >= 4.5,
                    "{rgb:06x} text on it is too faint: {:.2}",
                    contrast(on, signal)
                );
                let washed = mix(palette.surface, signal, palette.a_signal_wash);
                assert!(
                    contrast(palette.ink, washed) >= 10.0,
                    "{rgb:06x} text on its wash is below 10:1: {:.2}",
                    contrast(palette.ink, washed)
                );
            }
        }
    }

    /// Text and signal colors stay legible on tinted frames.
    #[test]
    fn tinted_frames_keep_text_legible() {
        let mut tints: Vec<u32> = ACCENT_PRESETS.iter().map(|&(_, c)| c).collect();
        tints.extend((0..36).map(|i| accent_from_hue(i as f32 * 10.)));
        tints.extend([0x000000, 0xffffff, 0xff0000, 0xffff00, 0x0000ff]);
        for rgb in tints {
            for palette in [&LIGHT, &DARK] {
                let bg = tint_for(hex(rgb), palette);
                let paper = tint_like(hex(rgb), palette.surface, PAPER_TINT_CHROMA);
                let ink = |a: f32| alpha(palette.ink, a);
                for under in [bg, paper] {
                    assert!(
                        contrast(ink(palette.a_muted), under) >= 7.0,
                        "{rgb:06x} muted {:.2}",
                        contrast(ink(palette.a_muted), under)
                    );
                    assert!(
                        contrast(ink(palette.a_subtle), under) >= 4.5,
                        "{rgb:06x} subtle {:.2}",
                        contrast(ink(palette.a_subtle), under)
                    );
                }
                // Default accent and every swatch, on the tinted frame.
                let accents = std::iter::once(palette.signal).chain(
                    ACCENT_PRESETS
                        .iter()
                        .map(|&(_, c)| accent_for(hex(c), palette).0),
                );
                for signal in accents {
                    assert!(
                        contrast(signal, bg) >= 4.3,
                        "{rgb:06x} signal color {:06x} on it: {:.2}",
                        pack(signal),
                        contrast(signal, bg)
                    );
                }
            }
        }
    }

    /// CJK fallbacks follow the UI language, whatever the system is set to; English follows the
    /// system.
    #[test]
    fn cjk_fallbacks_follow_the_ui_language() {
        let simplified = [
            "Microsoft YaHei UI",
            "Microsoft YaHei",
            "Microsoft JhengHei UI",
            "Microsoft JhengHei",
        ];
        let traditional = [
            "Microsoft JhengHei UI",
            "Microsoft JhengHei",
            "Microsoft YaHei UI",
            "Microsoft YaHei",
        ];
        for system in ["zh-TW", "zh-CN", "en"] {
            assert_eq!(cjk_fallbacks("zh-CN", system), simplified, "{system}");
            assert_eq!(cjk_fallbacks("zh-TW", system), traditional, "{system}");
        }
        assert_eq!(cjk_fallbacks("en", "zh-CN"), simplified);
        assert_eq!(cjk_fallbacks("en", "zh-TW"), traditional);
        // Elsewhere DirectWrite keeps choosing by the system locale (Japanese, Korean, ...).
        assert!(cjk_fallbacks("en", "en").is_empty());
    }

    /// The UI font keeps a custom family and only takes the fallbacks.
    #[test]
    fn ui_font_carries_language_fallbacks() {
        use gpui::Styled as _;
        set_ui_language("zh-CN", "zh-TW");
        let mut el = gpui::div().ui_font();
        let style = el.text_style();
        assert_eq!(style.font_family.as_deref(), Some(DEFAULT_UI_FONT));
        let fallbacks = style.font_fallbacks.clone();
        assert_eq!(
            fallbacks.as_ref().map(|f| f.fallback_list()[0].as_str()),
            Some("Microsoft YaHei UI")
        );
        set_ui_language("zh-TW", "zh-TW");
        let mut el = gpui::div().ui_font();
        assert_eq!(
            el.text_style()
                .font_fallbacks
                .as_ref()
                .map(|f| f.fallback_list()[0].as_str()),
            Some("Microsoft JhengHei UI")
        );
        set_ui_language("en", "en");
        assert!(gpui::div().ui_font().text_style().font_fallbacks.is_none());
    }

    /// File type glyphs stay visible on their paper, small (tinted) and large, in either theme.
    /// The paper is white in both, so the glyph colors must not follow the theme.
    #[test]
    fn file_glyphs_read_on_their_paper_in_both_themes() {
        let kinds = [
            FileKind::Pdf,
            FileKind::Presentation,
            FileKind::Archive,
            FileKind::Spreadsheet,
            FileKind::Audio,
            FileKind::Document,
            FileKind::Code,
            FileKind::Image,
            FileKind::Video,
            FileKind::Text,
            FileKind::Executable,
            FileKind::Shortcut,
            FileKind::Font,
            FileKind::DiskImage,
        ];
        let glyphs = |dark: bool| {
            set_dark(dark);
            kinds.map(|k| file_glyph(k).map(|(_, color)| pack(color)))
        };
        // Dark first so the default light mode is restored for other tests.
        assert_eq!(glyphs(true), glyphs(false));
        for kind in kinds {
            let Some((_, color)) = file_glyph(kind) else {
                continue;
            };
            for paper in [page_colors().0, small_page_colors(color).0] {
                assert!(
                    contrast(color, paper) >= 2.5,
                    "{kind:?} glyph on paper: {:.2}",
                    contrast(color, paper)
                );
            }
        }
    }

    /// Colors that already pass are untouched; others only change lightness, never hue.
    #[test]
    fn accents_keep_their_hue() {
        // Windows blue is just under 4.5 in the light theme, so it only darkens slightly.
        let (light, on) = accent_for(hex(0x0078d4), &LIGHT);
        assert_eq!(pack(on), 0xffffff);
        let (_, _, hue) = to_oklch(hex(0x0078d4));
        let (l_light, _, h_light) = to_oklch(light);
        assert!((h_light - hue).abs() < 2., "{h_light} vs {hue}");
        // Dark theme: same hue, much lighter.
        let (dark, _) = accent_for(hex(0x0078d4), &DARK);
        let (l_dark, _, h_dark) = to_oklch(dark);
        assert!((h_dark - hue).abs() < 2., "{h_dark} vs {hue}");
        assert!(l_dark > l_light + 0.1);
        // A dark enough color stays unchanged in the light theme.
        let (same, _) = accent_for(hex(0x8e4ec6), &LIGHT);
        assert_eq!(pack(same), 0x8e4ec6);
    }

    #[test]
    fn oklch_round_trips() {
        for rgb in [
            0xc2410c_u32,
            0xfb923c,
            0x0078d4,
            0x2e8b3a,
            0x808080,
            0xffffff,
        ] {
            let (l, c, h) = to_oklch(hex(rgb));
            assert_eq!(pack(oklch(l, c, h)), rgb, "{rgb:06x}");
        }
    }

    #[test]
    fn oklch_matches_reference() {
        // oklch(0.628 0.2577 29.23) is pure sRGB red.
        let red = oklch(0.627_955, 0.257_683, 29.233_885);
        assert!((red.r - 1.).abs() < 0.01 && red.g < 0.01 && red.b < 0.01);
    }
}
