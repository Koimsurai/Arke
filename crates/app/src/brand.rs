//! Arke's mark in the UI: 16 px at the top of the sidebar and a larger one on the About page.
//!
//! The mark is multi-color, so it is a PNG (`cargo run -p genicon` renders it from the SVGs at 2x)
//! rather than an SVG drawn as a one-color mask like the UI icons.

use std::sync::{Arc, LazyLock};

use gpui::{AnyElement, Image, ImageFormat, IntoElement, ParentElement, Styled, div, img, px};

use crate::theme;

static SMALL: LazyLock<Arc<Image>> =
    LazyLock::new(|| png(include_bytes!("../assets/brand/arke-32.png")));
static LARGE: LazyLock<Arc<Image>> =
    LazyLock::new(|| png(include_bytes!("../assets/brand/arke-96.png")));

fn png(bytes: &'static [u8]) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Png, bytes.to_vec()))
}

/// Sidebar mark: 16 px, centered on the sidebar's icon column (the Home row and the tree icons
/// below it), with a little more room before the back button than between the nav buttons.
pub(crate) fn sidebar_mark() -> impl IntoElement {
    div()
        .flex_none()
        .ml(theme::S_4)
        .mr(theme::S_8)
        .child(img(SMALL.clone()).size(theme::ICON))
}

/// About page mark, shown next to the app name and version.
pub(crate) fn about_mark() -> AnyElement {
    img(LARGE.clone())
        .size(px(56.))
        .flex_none()
        .into_any_element()
}
