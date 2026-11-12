//! Generates Arke's icons:
//!
//!   cargo run -p genicon
//!
//! Reads `crates/app/assets/brand/arke.svg` (large sizes, detailed) and `arke-small.svg` (40 px
//! and below: solid, no gradients or shadows, aligned to a 32 px grid so the taskbar and
//! notification area stay crisp), and writes
//! - `crates/app/assets/brand/arke.ico`: the exe icon (taskbar, title bar, Explorer), multi-size
//! - `crates/app/assets/brand/arke-32.png`: the sidebar mark (16 px at 2x, small design)
//! - `crates/app/assets/brand/arke-96.png`: the About page mark (48 px at 2x)
//!
//! The outputs are committed; rerun only after changing the SVGs.

#![allow(clippy::print_stdout, clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use resvg::{tiny_skia, usvg};

/// Sizes Windows uses (16/24/32/48 at 100%-300% scaling, plus large icons).
const ICO_SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];
/// Below this size the small variant is used.
const SMALL_MAX: u32 = 40;

fn render(tree: &usvg::Tree, size: u32) -> tiny_skia::Pixmap {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// tiny-skia uses premultiplied alpha; ico and PNG want straight RGBA.
fn straight_rgba(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

fn main() {
    // Preview: `cargo run -p genicon -- preview <svg> <out dir>` writes PNGs at a few taskbar
    // sizes for comparing designs.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("preview") && args.len() == 3 {
        let svg = std::fs::read(&args[1]).expect("read SVG");
        let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("parse SVG");
        let out = Path::new(&args[2]);
        for size in [16, 24, 32, 48, 256] {
            let pixmap = render(&tree, size);
            let image = ico::IconImage::from_rgba_data(size, size, straight_rgba(&pixmap));
            let file = std::fs::File::create(out.join(format!("{size}.png"))).expect("write PNG");
            image.write_png(file).expect("write PNG");
        }
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/app/assets/brand");
    let load = |name: &str| {
        let svg = std::fs::read(root.join(name)).expect("read SVG");
        usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("parse SVG")
    };
    let tree = load("arke.svg");
    let small = load("arke-small.svg");

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in ICO_SIZES {
        let pixmap = render(if size <= SMALL_MAX { &small } else { &tree }, size);
        let image = ico::IconImage::from_rgba_data(size, size, straight_rgba(&pixmap));
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode icon"));
    }
    let file = std::fs::File::create(root.join("arke.ico")).expect("write arke.ico");
    dir.write(file).expect("write arke.ico");

    // In-app marks at 2x: the sidebar's 16 px mark uses the small design, the About page's 48 px
    // mark the detailed one.
    for (name, source, size) in [("arke-32.png", &small, 32), ("arke-96.png", &tree, 96)] {
        let png = render(source, size);
        let image = ico::IconImage::from_rgba_data(size, size, straight_rgba(&png));
        let file = std::fs::File::create(root.join(name)).expect("write PNG");
        image.write_png(file).expect("write PNG");
    }
    println!("generated {}", root.display());
}
