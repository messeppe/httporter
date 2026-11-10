// SPDX-License-Identifier: MIT OR Apache-2.0

//! `transform-scale` in the patched software renderer (`vendor/i-slint-renderer-software`,
//! `scale_layer.rs`), driven through Slint's real renderer into a real pixel buffer with the
//! pixel type the winit software backend draws into (as in `text_lcd.rs`).
//!
//! A coloured card with text, scaled about its centre: at 0.9 it covers 90% of its unscaled
//! size around the same centre; at 1.0 it is pixel for pixel the card without the property; and
//! the partial renderer repaints everything the scale moves.

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize};

slint::slint! {
    import "../ui/fonts/Inter-Regular.ttf";

    export component Scaled inherits Window {
        in property <float> zoom: 1.0;
        in property <string> label: "Scaled card";
        width: 200px;
        height: 120px;
        background: #ffffff;
        Rectangle {
            x: 40px;
            y: 20px;
            width: 120px;
            height: 80px;
            background: #3366cc;
            transform-scale: zoom;
            transform-origin: { x: self.width / 2, y: self.height / 2 };
            Text {
                x: 10px;
                y: 30px;
                text: label;
                color: #ffffff;
                font-family: "Inter";
                font-size: 13px;
            }
        }
    }

    export component Plain inherits Window {
        in property <string> label: "Scaled card";
        width: 200px;
        height: 120px;
        background: #ffffff;
        Rectangle {
            x: 40px;
            y: 20px;
            width: 120px;
            height: 80px;
            background: #3366cc;
            Text {
                x: 10px;
                y: 30px;
                text: label;
                color: #ffffff;
                font-family: "Inter";
                font-size: 13px;
            }
        }
    }
}

/// The pixel type `i-slint-backend-winit`'s software renderer hands to Slint: 0xAARRGGBB.
#[repr(transparent)]
#[derive(Copy, Clone)]
struct SoftBufferPixel(u32);

impl From<SoftBufferPixel> for PremultipliedRgbaColor {
    fn from(pixel: SoftBufferPixel) -> Self {
        let v = pixel.0;
        PremultipliedRgbaColor {
            red: (v >> 16) as u8,
            green: (v >> 8) as u8,
            blue: v as u8,
            alpha: (v >> 24) as u8,
        }
    }
}

impl From<PremultipliedRgbaColor> for SoftBufferPixel {
    fn from(pixel: PremultipliedRgbaColor) -> Self {
        Self(
            (pixel.alpha as u32) << 24
                | ((pixel.red as u32) << 16)
                | ((pixel.green as u32) << 8)
                | (pixel.blue as u32),
        )
    }
}

impl TargetPixel for SoftBufferPixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let mut x = PremultipliedRgbaColor::from(*self);
        x.blend(color);
        *self = x.into();
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(0xff000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    fn background() -> Self {
        Self(0)
    }
}

const WIDTH: u32 = 200;
const HEIGHT: u32 = 120;
const CARD: [u8; 3] = [0x33, 0x66, 0xcc];

thread_local! {
    // The next window a component on this thread will open (Slint's platform is per thread).
    static NEXT_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = RefCell::default();
}

/// The headless Slint platform of a test: it hands out the window [`window`] just made.
struct Headless;

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        NEXT_WINDOW
            .with(|next| next.borrow_mut().take())
            .map(|window| window as Rc<dyn WindowAdapter>)
            .ok_or_else(|| "no window prepared for this component".into())
    }
}

fn window(repaint: RepaintBufferType) -> Rc<MinimalSoftwareWindow> {
    let window = MinimalSoftwareWindow::new(repaint);
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    NEXT_WINDOW.with(|next| *next.borrow_mut() = Some(window.clone()));
    // Already set for a second window on the same thread, which is fine.
    let _ = slint::platform::set_platform(Box::new(Headless));
    window
}

/// Draws what the window needs into `pixels` (all of it for a new buffer).
fn draw(window: &MinimalSoftwareWindow, pixels: &mut [SoftBufferPixel]) {
    let drew = window.draw_if_needed(|renderer| {
        renderer.render(pixels, WIDTH as usize);
    });
    assert!(drew, "the window should have needed a redraw");
}

fn unpack(pixels: &[SoftBufferPixel]) -> Vec<[u8; 3]> {
    pixels
        .iter()
        .map(|p| [(p.0 >> 16) as u8, (p.0 >> 8) as u8, p.0 as u8])
        .collect()
}

fn blank() -> Vec<SoftBufferPixel> {
    vec![SoftBufferPixel::background(); (WIDTH * HEIGHT) as usize]
}

/// A whole frame of the scaled card.
fn scaled(zoom: f32) -> Vec<[u8; 3]> {
    let window = window(RepaintBufferType::NewBuffer);
    let card = Scaled::new().unwrap();
    card.set_zoom(zoom);
    card.show().unwrap();
    let mut pixels = blank();
    draw(&window, &mut pixels);
    unpack(&pixels)
}

/// A whole frame of the card without `transform-scale`.
fn plain() -> Vec<[u8; 3]> {
    let window = window(RepaintBufferType::NewBuffer);
    let card = Plain::new().unwrap();
    card.show().unwrap();
    let mut pixels = blank();
    draw(&window, &mut pixels);
    unpack(&pixels)
}

/// The card's bounds: the first and last column and row where the card colour covers at least
/// half of the pixel (white paper elsewhere). Edges are antialiased, so this is the coverage
/// boundary, to a pixel.
fn card_bounds(frame: &[[u8; 3]]) -> (f32, f32, f32, f32) {
    let covered = |p: [u8; 3]| {
        // Red goes from 0xff (paper) to 0x33 (card): the card's share of the pixel. The
        // white text inside is paper-coloured; it is enclosed by the card on every side.
        let share = (255. - f32::from(p[0])) / (255. - f32::from(CARD[0]));
        share >= 0.5 && p[2] >= 0xb0
    };
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (i, &p) in frame.iter().enumerate() {
        if covered(p) {
            let (x, y) = (i % WIDTH as usize, i / WIDTH as usize);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    assert!(x0 <= x1, "no card in the frame");
    (x0 as f32, y0 as f32, (x1 + 1) as f32, (y1 + 1) as f32)
}

fn count_differences(a: &[[u8; 3]], b: &[[u8; 3]]) -> usize {
    a.iter().zip(b).filter(|(p, q)| p != q).count()
}

/// At 0.9 about its centre, the card is 90% of its unscaled size, around the same centre, and its
/// text is drawn (smaller) inside it.
#[test]
fn a_card_at_0_9_is_ninety_percent_around_its_centre() {
    let full = scaled(1.0);
    let small = scaled(0.9);
    let (fx0, fy0, fx1, fy1) = card_bounds(&full);
    let (sx0, sy0, sx1, sy1) = card_bounds(&small);
    println!("1.0: {fx0} {fy0} {fx1} {fy1}; 0.9: {sx0} {sy0} {sx1} {sy1}");
    assert_eq!(
        (fx0, fy0, fx1, fy1),
        (40., 20., 160., 100.),
        "the unscaled card"
    );

    let (fw, fh, sw, sh) = (fx1 - fx0, fy1 - fy0, sx1 - sx0, sy1 - sy0);
    assert!(
        (sw - 0.9 * fw).abs() <= 1.0,
        "width {sw}, expected {}",
        0.9 * fw
    );
    assert!(
        (sh - 0.9 * fh).abs() <= 1.0,
        "height {sh}, expected {}",
        0.9 * fh
    );
    let centre = |a: f32, b: f32| (a + b) / 2.;
    assert!(
        (centre(sx0, sx1) - centre(fx0, fx1)).abs() <= 0.5,
        "horizontal centre"
    );
    assert!(
        (centre(sy0, sy1) - centre(fy0, fy1)).abs() <= 0.5,
        "vertical centre"
    );

    // The text: light pixels inside the scaled card.
    let text = small
        .iter()
        .enumerate()
        .filter(|&(i, p)| {
            let (x, y) = ((i % WIDTH as usize) as f32, (i / WIDTH as usize) as f32);
            x > sx0 + 1. && x < sx1 - 1. && y > sy0 + 1. && y < sy1 - 1. && p[0] > 0xc0
        })
        .count();
    assert!(
        text > 20,
        "the text should be drawn inside the scaled card ({text} pixels)"
    );
    // Outside the scaled card everything is paper.
    for (i, p) in small.iter().enumerate() {
        let (x, y) = ((i % WIDTH as usize) as f32, (i / WIDTH as usize) as f32);
        if x < sx0 - 1. || x >= sx1 + 1. || y < sy0 - 1. || y >= sy1 + 1. {
            assert_eq!(*p, [0xff; 3], "paper at ({x}, {y})");
        }
    }
}

/// Scale 1 takes the old path: exactly the frame of the same card without the property.
#[test]
fn scale_one_is_the_unscaled_frame() {
    let with = scaled(1.0);
    let without = plain();
    assert_eq!(
        count_differences(&with, &without),
        0,
        "scale 1 should draw exactly as before"
    );
}

/// In a reused buffer, the partial renderer repaints what a scale change and a change inside the
/// scaled card touch: every partial frame equals a whole frame of the same state.
#[test]
fn partial_frames_cover_the_scaled_area() {
    let window = window(RepaintBufferType::ReusedBuffer);
    let card = Scaled::new().unwrap();
    card.show().unwrap();
    let mut pixels = blank();
    draw(&window, &mut pixels);
    assert_eq!(
        count_differences(&unpack(&pixels), &scaled(1.0)),
        0,
        "first frame"
    );

    for zoom in [0.9, 0.95, 0.5, 1.0, 0.9] {
        card.set_zoom(zoom);
        draw(&window, &mut pixels);
        assert_eq!(
            count_differences(&unpack(&pixels), &scaled(zoom)),
            0,
            "a partial frame at {zoom}"
        );
    }

    // A change inside the scaled card (the text) repaints where the card is drawn.
    card.set_label("Other text".into());
    draw(&window, &mut pixels);
    let fresh = {
        let window = self::window(RepaintBufferType::NewBuffer);
        let card = Scaled::new().unwrap();
        card.set_zoom(0.9);
        card.set_label("Other text".into());
        card.show().unwrap();
        let mut pixels = blank();
        draw(&window, &mut pixels);
        unpack(&pixels)
    };
    assert_eq!(
        count_differences(&unpack(&pixels), &fresh),
        0,
        "the partial frame after the text changed"
    );
}
