// SPDX-License-Identifier: MIT OR Apache-2.0

//! Renders `assets/icon/httporter.svg` into `httporter.ico` (16 to 256 px: 256 as PNG, the rest as
//! BMP) and `httporter-32.png`, next to the SVG. Run from this folder:
//!
//! ```text
//! cargo run --release -- ../../assets/icon
//! ```
//!
//! The mark's stroke is drawn thicker at the small sizes, where 3.2 units of the 48-unit mark
//! would be well under two pixels.

use std::fs::File;
use std::path::PathBuf;

use resvg::{tiny_skia, usvg};

const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// The mark's stroke width (in the 48-unit mark) at `size` px.
fn stroke_width(size: u32) -> f32 {
    match size {
        0..=24 => 5.0,
        25..=48 => 4.0,
        _ => 3.2,
    }
}

/// The icon at `size` px, as straight (not premultiplied) RGBA.
fn render(svg: &str, size: u32) -> ico::IconImage {
    let svg = svg.replace(
        "stroke-width=\"3.2\"",
        &format!("stroke-width=\"{}\"", stroke_width(size)),
    );
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).expect("the SVG parses");
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("a non-empty size");
    let scale = size as f32 / tree.size().width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let c = pixel.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    ico::IconImage::from_rgba_data(size, size, rgba)
}

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "../../assets/icon".into()),
    );
    let svg = std::fs::read_to_string(dir.join("httporter.svg")).expect("httporter.svg is there");
    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for size in SIZES {
        let image = render(&svg, size);
        let entry = if size >= 256 {
            ico::IconDirEntry::encode_as_png(&image)
        } else {
            ico::IconDirEntry::encode_as_bmp(&image)
        }
        .expect("the image encodes");
        icon.add_entry(entry);
        if size == 32 {
            let png = File::create(dir.join("httporter-32.png")).expect("the PNG can be written");
            image.write_png(png).expect("the PNG is written");
        }
    }
    let file = File::create(dir.join("httporter.ico")).expect("the icon can be written");
    icon.write(file).expect("the icon is written");
    println!("wrote {}", dir.join("httporter.ico").display());
}
