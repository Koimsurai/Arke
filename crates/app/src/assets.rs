//! Embedded UI icons (simple SVGs drawn for this project).
//!
//! Each SVG is a single-color shape; GPUI draws it as a mask and `theme` picks the color.
//! Multi-color icons are stacked layers: folders and file pages (see `explorer::kind_icon`) and
//! colored place icons, a tinted fill under the line (see [`tinted`]).

use std::borrow::Cow;

use arke_core::filetype::FileKind;
use gpui::{
    AnyElement, AssetSource, Hsla, IntoElement, ParentElement, Pixels, Result, SharedString,
    Styled, div, svg,
};

use crate::theme;

pub(crate) struct Assets;

macro_rules! icons {
    ($($variant:ident => $name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] =
            &[$(($name, include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*];

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Icon {
            $($variant),*
        }

        impl Icon {
            pub(crate) const fn path(self) -> &'static str {
                match self {
                    $(Icon::$variant => concat!("icons/", $name, ".svg")),*
                }
            }
        }
    };
}

icons![
    ArrowLeft => "arrow-left",
    ArrowRight => "arrow-right",
    ArrowUp => "arrow-up",
    Refresh => "refresh",
    Bolt => "bolt",
    ChevronRight => "chevron-right",
    Drive => "drive",
    NetDrive => "net-drive",
    Close => "close",
    Plus => "plus",
    Search => "search",
    Subfolders => "subfolders",
    FolderPlus => "folder-plus",
    Cut => "cut",
    Copy => "copy",
    Paste => "paste",
    Rename => "rename",
    Trash => "trash",
    More => "more",
    Grip => "grip",
    Restore => "restore",
    ViewList => "view-list",
    ViewGrid => "view-grid",
    ViewColumns => "view-columns",
    Split => "split",
    Density => "density",
    Preview => "preview",
    Cloud => "cloud",
    CloudLocal => "cloud-local",
    CloudPinned => "cloud-pinned",
    CloudSyncing => "cloud-syncing",
    CloudError => "cloud-error",
    Shelf => "shelf",
    // Symlink/junction badge over an item's icon (fill, frame, arrow)
    LinkBadgeFill => "link-badge-fill",
    LinkBadgeFrame => "link-badge-frame",
    LinkBadgeArrow => "link-badge-arrow",
    TabsTop => "tabs-top",
    TabsSide => "tabs-side",
    // File icon layers
    FolderBack => "folder-back",
    FolderFront => "folder-front",
    PageFill => "page-fill",
    PageLine => "page-line",
    // Type glyphs on the file page
    FtImage => "ft-image",
    FtVideo => "ft-video",
    FtAudio => "ft-audio",
    FtArchive => "ft-archive",
    FtCode => "ft-code",
    FtText => "ft-text",
    FtPdf => "ft-pdf",
    FtDoc => "ft-doc",
    FtSheet => "ft-sheet",
    FtSlides => "ft-slides",
    FtApp => "ft-app",
    FtShortcut => "ft-shortcut",
    FtFont => "ft-font",
    FtDisk => "ft-disk",
    // Known folders (favorites tiles)
    KfHome => "kf-home",
    KfUser => "kf-user",
    KfDesktop => "kf-desktop",
    KfDownload => "kf-download",
    KfDocuments => "kf-documents",
    KfPictures => "kf-pictures",
    KfMusic => "kf-music",
    KfVideos => "kf-videos",
    Folder => "folder",
    // Tint layers under colored place icons (`theme::place_tint`), see `tinted`
    KfHomeFill => "kf-home-fill",
    KfUserFill => "kf-user-fill",
    KfDesktopFill => "kf-desktop-fill",
    KfDownloadFill => "kf-download-fill",
    KfDocumentsFill => "kf-documents-fill",
    KfPicturesFill => "kf-pictures-fill",
    KfMusicFill => "kf-music-fill",
    KfVideosFill => "kf-videos-fill",
    CloudFill => "cloud-fill",
    DriveFill => "drive-fill",
    NetDriveFill => "net-drive-fill",
    TerminalFill => "terminal-fill",
    PhoneFill => "phone-fill",
    // Large folder art (grid, preview): back panel, paper, front flap, lit edge
    FolderLgBack => "folder-lg-back",
    FolderLgPaper => "folder-lg-paper",
    FolderLgFront => "folder-lg-front",
    FolderLgLip => "folder-lg-lip",
    // Custom window buttons
    WinMin => "win-min",
    WinMax => "win-max",
    WinRestore => "win-restore",
    WinClose => "win-close",
    // Menu check mark
    Check => "check",
    Settings => "settings",
    Menu => "menu",
    Sidebar => "sidebar",
    // Settings card icons
    Theme => "theme",
    Accent => "accent",
    // Context menu
    Terminal => "terminal",
    Phone => "phone",
    Pin => "pin",
    External => "external",
    Open => "open",
    // "Open with" / "Open with <editor>" when the app's own icon can't be read. Not `FtApp`: that
    // is a glyph drawn small inside a file page and shrinks to a dot on its own.
    OpenWith => "open-with",
    Zip => "zip",
    EyeOff => "eye-off",
    FileExt => "file-ext",
    Image => "image",
    Sort => "sort",
    Globe => "globe",
    Power => "power",
    Info => "info",
    History => "history",
    Clock => "clock",
    Calendar => "calendar",
    Dimensions => "dimensions",
    Camera => "camera",
    Lens => "lens",
    Sun => "sun",
    Window => "window",
    // One metaphor per command, so no two neighbors in a menu or settings page look alike
    Undo => "undo",
    Redo => "redo",
    CopyPath => "copy-path",
    Font => "font",
    Update => "update",
    Feedback => "feedback",
    Tray => "tray",
    TabNew => "tab-new",
    TabOpen => "tab-open",
    // Dual pane: synchronized browsing, folder compare; saved workspaces
    Link => "link",
    Compare => "compare",
    Workspaces => "workspaces",
    Explorer => "explorer",
    WindowStart => "window-start",
];

impl Icon {
    /// The fill drawn under a colored place icon, if it has one.
    const fn tint_layer(self) -> Option<Self> {
        Some(match self {
            Self::KfHome => Self::KfHomeFill,
            Self::KfUser => Self::KfUserFill,
            Self::KfDesktop => Self::KfDesktopFill,
            Self::KfDownload => Self::KfDownloadFill,
            Self::KfDocuments => Self::KfDocumentsFill,
            Self::KfPictures => Self::KfPicturesFill,
            Self::KfMusic => Self::KfMusicFill,
            Self::KfVideos => Self::KfVideosFill,
            Self::Cloud => Self::CloudFill,
            Self::Drive => Self::DriveFill,
            Self::NetDrive => Self::NetDriveFill,
            Self::Terminal => Self::TerminalFill,
            Self::Phone => Self::PhoneFill,
            _ => return None,
        })
    }
}

/// A place icon in its own color, like the known folders in File Explorer and Files: a soft
/// tinted fill under the line, both from [`theme::place_tint`]. A plain folder is the amber
/// folder; any other icon is a plain line in `color`. For the sidebar, Home and Ctrl+T; menus and
/// settings keep one-color line icons.
pub(crate) fn tinted(icon: Icon, size: Pixels, color: impl Into<Hsla>) -> AnyElement {
    if icon == Icon::Folder {
        return crate::explorer::kind_icon(FileKind::Folder, size);
    }
    let layer = |icon: Icon, color: Hsla| {
        svg()
            .absolute()
            .top_0()
            .left_0()
            .size(size)
            .path(icon.path())
            .text_color(color)
    };
    match (theme::place_tint(icon), icon.tint_layer()) {
        (Some((line, fill)), Some(under)) => div()
            .relative()
            .size(size)
            .flex_none()
            .child(layer(under, fill.into()))
            .child(layer(icon, line.into()))
            .into_any_element(),
        _ => svg()
            .path(icon.path())
            .size(size)
            .flex_none()
            .text_color(color)
            .into_any_element(),
    }
}

/// Progress ring track: the whole circle the arcs of [`ring_arc`] run along.
pub(crate) const RING_TRACK: &str = "ring/track.svg";
/// Arc lengths the ring is drawn with: `ring_arc` rounds to 1/RING_STEPS of a turn.
const RING_STEPS: u32 = 48;

/// Progress ring arc of `sweep` turns, clockwise from 12 o'clock. Made when first loaded (see
/// [`ring_svg`]); GPUI rasterizes each length once, and the spinner rotates the same sprite.
pub(crate) fn ring_arc(sweep: f32) -> String {
    let steps = (sweep.clamp(0., 1.) * RING_STEPS as f32).round() as u32;
    format!("ring/arc-{}.svg", steps.clamp(1, RING_STEPS))
}

/// The SVG for a [`RING_TRACK`] or [`ring_arc`] path: a 2-unit stroke of radius 7 on the 16-unit
/// grid, like the icons.
fn ring_svg(path: &str) -> Option<String> {
    let shape = if path == RING_TRACK {
        r#"<circle cx="8" cy="8" r="7"/>"#.to_owned()
    } else {
        let steps: u32 = path
            .strip_prefix("ring/arc-")?
            .strip_suffix(".svg")?
            .parse()
            .ok()?;
        if !(1..=RING_STEPS).contains(&steps) {
            return None;
        }
        if steps == RING_STEPS {
            r#"<circle cx="8" cy="8" r="7"/>"#.to_owned()
        } else {
            let turn = steps as f32 / RING_STEPS as f32;
            let angle = turn * std::f32::consts::TAU;
            let (x, y) = (8. + 7. * angle.sin(), 8. - 7. * angle.cos());
            let large = u8::from(turn > 0.5);
            format!(r#"<path d="M8 1A7 7 0 {large} 1 {x:.3} {y:.3}"/>"#)
        }
    };
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="#000" stroke-width="2">{shape}</svg>"##
    ))
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.starts_with("ring/") {
            return Ok(ring_svg(path).map(|svg| Cow::Owned(svg.into_bytes())));
        }
        let name = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"));
        Ok(name.and_then(|name| {
            ICONS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, bytes)| Cow::Borrowed(*bytes))
        }))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(n, _)| SharedString::from(format!("icons/{n}.svg")))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The progress ring's layers load for every length it can be drawn with, and nothing else.
    #[test]
    fn ring_arcs_load_for_every_length() {
        let load = |path: &str| Assets.load(path).unwrap();
        for sweep in [0., 0.01, 0.04, 0.25, 0.5, 0.51, 0.99, 1.] {
            let path = ring_arc(sweep);
            let svg = load(&path).unwrap_or_else(|| panic!("{path}"));
            let svg = String::from_utf8_lossy(&svg);
            assert!(svg.contains(r#"viewBox="0 0 16 16""#), "{svg}");
        }
        assert_eq!(ring_arc(0.25), "ring/arc-12.svg");
        assert!(load(RING_TRACK).is_some());
        assert!(load("ring/arc-0.svg").is_none());
        assert!(load("ring/arc-49.svg").is_none());
        // More than half a turn takes the long way round.
        let long = load(&ring_arc(0.75)).unwrap();
        assert!(String::from_utf8_lossy(&long).contains("0 1 1 "));
    }

    /// Every icon is drawn on the same 16-unit grid, so icons line up at any size.
    #[test]
    fn icons_share_the_16_unit_grid() {
        for (name, bytes) in ICONS {
            let svg = String::from_utf8_lossy(bytes);
            assert!(svg.contains(r#"viewBox="0 0 16 16""#), "{name}");
        }
    }

    /// A place gets a color exactly when it has a fill layer to show it with.
    #[test]
    fn tinted_places_have_a_fill_layer() {
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
            Icon::NetDrive,
            Icon::Terminal,
            Icon::Folder,
            Icon::Copy,
        ];
        for icon in places {
            assert_eq!(
                theme::place_tint(icon).is_some(),
                icon.tint_layer().is_some(),
                "{icon:?}"
            );
        }
    }
}
