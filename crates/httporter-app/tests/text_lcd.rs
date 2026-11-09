// SPDX-License-Identifier: MIT OR Apache-2.0

//! ClearType-style text in the patched software renderer (`vendor/i-slint-renderer-software`),
//! driven through Slint's real renderer into a real pixel buffer with the pixel type the winit
//! software backend draws into (`SoftBufferPixel`, copied here because it is private to that
//! backend). The pixels are what is checked: a mistake anywhere in the chain (the glyph path,
//! the layout probe in `draw_functions.rs`, the blend) shows up here.
//!
//! The text mode is a process-wide atomic, so the tests take [`mode_lock`] in turn.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel, TextMode,
    set_text_mode,
};
use slint::platform::{Platform, WindowAdapter};
use slint::{Color, ComponentHandle, Image, PhysicalSize, Rgba8Pixel, SharedPixelBuffer};

slint::slint! {
    import "../ui/fonts/Inter-Regular.ttf";

    export component TextLine inherits Window {
        in property <color> ink: #f5f5f7;
        in property <color> paper: #1e1e20;
        in property <float> fade: 1.0;
        in property <string> line: "l I l I l I l";
        width: 200px;
        height: 30px;
        background: paper;
        Text {
            x: 8px;
            y: 7px;
            text: line;
            color: ink;
            font-family: "Inter";
            font-size: 13px;
            opacity: fade;
        }
    }

    export component Icon inherits Window {
        in property <image> mark;
        width: 200px;
        height: 30px;
        background: #1e1e20;
        Image {
            x: 8px;
            y: 7px;
            width: 16px;
            height: 16px;
            source: mark;
            colorize: #33cc66;
        }
    }
}

/// The pixel type `i-slint-backend-winit`'s software renderer hands to Slint: 0xAARRGGBB, one
/// u32, with `blend` and `from_rgb` as that backend defines them.
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
const HEIGHT: u32 = 30;
const DARK_INK: [u8; 3] = [0xf5, 0xf5, 0xf7];
const DARK_PAPER: [u8; 3] = [0x1e, 0x1e, 0x20];
const LIGHT_INK: [u8; 3] = [0x1d, 0x1d, 0x1f];
const LIGHT_PAPER: [u8; 3] = [0xff, 0xff, 0xff];

/// One test at a time may change the text mode.
fn mode_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

fn color(rgb: [u8; 3]) -> Color {
    Color::from_rgb_u8(rgb[0], rgb[1], rgb[2])
}

/// Renders `window` into a fresh buffer of the app's pixel type.
fn frame(window: &MinimalSoftwareWindow) -> Vec<[u8; 3]> {
    let mut pixels = vec![SoftBufferPixel::background(); (WIDTH * HEIGHT) as usize];
    let drew = window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, WIDTH as usize);
    });
    assert!(drew, "the window should have needed a redraw");
    unpack(&pixels)
}

fn unpack(pixels: &[SoftBufferPixel]) -> Vec<[u8; 3]> {
    pixels
        .iter()
        .map(|p| [(p.0 >> 16) as u8, (p.0 >> 8) as u8, p.0 as u8])
        .collect()
}

/// Renders the line in `mode`: each frame gets its own window on its own component, so one
/// frame's glyphs can't leak into the next.
fn line(mode: TextMode, ink: [u8; 3], paper: [u8; 3], fade: f32) -> Vec<[u8; 3]> {
    set_text_mode(mode);
    let window = window(RepaintBufferType::NewBuffer);
    let line = TextLine::new().unwrap();
    line.set_ink(color(ink));
    line.set_paper(color(paper));
    line.set_fade(fade);
    line.show().unwrap();
    frame(&window)
}

/// Coverage of `ink` over `paper` per channel, 0.0 to 1.0.
fn coverage(pixel: [u8; 3], ink: [u8; 3], paper: [u8; 3]) -> [f32; 3] {
    core::array::from_fn(|c| {
        (f32::from(pixel[c]) - f32::from(paper[c])) / (f32::from(ink[c]) - f32::from(paper[c]))
    })
}

/// Coverage as the eye weighs it (Rec. 709 luminance weights).
fn luminance(cov: [f32; 3]) -> f32 {
    0.2126 * cov[0] + 0.7152 * cov[1] + 0.0722 * cov[2]
}

/// How many pixels show clearly unequal coverage on their channels.
fn fringed(frame: &[[u8; 3]], ink: [u8; 3], paper: [u8; 3]) -> usize {
    frame
        .iter()
        .filter(|&&p| {
            let c = coverage(p, ink, paper);
            max(c) - min(c) > 0.15
        })
        .count()
}

fn max(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2])
}

fn min(c: [f32; 3]) -> f32 {
    c[0].min(c[1]).min(c[2])
}

/// The peak (as the eye weighs it) of each vertical stem: a stem is a run of columns with ink.
fn stem_peaks(frame: &[[u8; 3]], ink: [u8; 3], paper: [u8; 3]) -> Vec<f32> {
    let column: Vec<f32> = (0..WIDTH as usize)
        .map(|x| {
            (0..HEIGHT as usize)
                .map(|y| luminance(coverage(frame[y * WIDTH as usize + x], ink, paper)))
                .fold(0.0, f32::max)
        })
        .collect();
    let mut peaks = Vec::new();
    let mut run: Option<f32> = None;
    for value in column {
        if value > 0.15 {
            run = Some(run.unwrap_or(0.0).max(value));
        } else if let Some(peak) = run.take() {
            peaks.push(peak);
        }
    }
    peaks.extend(run);
    peaks
}

/// The stems of "l I l I l I l" alternate between the lowercase l and the capital I. The l must
/// reach 90 % of the text colour. Inter's capital I is narrower at 13 px and does not, with the
/// same glyph treatment or with Windows' own: measured here 0.89 on dark and 0.86 on light, where
/// GDI+ ClearType gives 0.82 and 0.71. Its bound pins that, a little below the measured values.
fn assert_stems(frame: &[[u8; 3]], ink: [u8; 3], paper: [u8; 3]) {
    let peaks = stem_peaks(frame, ink, paper);
    assert_eq!(
        peaks.len(),
        7,
        "l I l I l I l should give seven stems: {peaks:?}"
    );
    for (i, &peak) in peaks.iter().enumerate() {
        let (glyph, floor) = if i % 2 == 0 { ("l", 0.9) } else { ("I", 0.85) };
        assert!(
            peak >= floor,
            "the {glyph} stem should reach {floor} of the text colour: {peaks:?}"
        );
    }
}

/// The dark theme's text: edges fringe (unequal coverage per channel), stems are crisp.
#[test]
fn lcd_text_on_the_dark_theme() {
    let _mode = mode_lock();
    let frame = line(TextMode::LcdRgb, DARK_INK, DARK_PAPER, 1.0);
    assert!(fringed(&frame, DARK_INK, DARK_PAPER) > 8, "ClearType edges");
    assert_stems(&frame, DARK_INK, DARK_PAPER);
}

/// The light theme's dark text gets its own curve but the same treatment.
#[test]
fn lcd_text_on_the_light_theme() {
    let _mode = mode_lock();
    let frame = line(TextMode::LcdRgb, LIGHT_INK, LIGHT_PAPER, 1.0);
    assert!(
        fringed(&frame, LIGHT_INK, LIGHT_PAPER) > 8,
        "ClearType edges"
    );
    assert_stems(&frame, LIGHT_INK, LIGHT_PAPER);
}

/// Grayscale mode: neutral, and just as crisp.
#[test]
fn grayscale_mode_is_neutral() {
    let _mode = mode_lock();
    let frame = line(TextMode::Grayscale, DARK_INK, DARK_PAPER, 1.0);
    assert_eq!(
        fringed(&frame, DARK_INK, DARK_PAPER),
        0,
        "no colour fringes"
    );
    for &p in &frame {
        let c = coverage(p, DARK_INK, DARK_PAPER);
        assert!(
            max(c) - min(c) < 0.02,
            "channels should be equal, got {c:?}"
        );
    }
    assert_stems(&frame, DARK_INK, DARK_PAPER);
}

/// A BGR panel mirrors the red and blue coverage of an RGB one.
#[test]
fn bgr_swaps_red_and_blue() {
    let _mode = mode_lock();
    let rgb = line(TextMode::LcdRgb, DARK_INK, DARK_PAPER, 1.0);
    let bgr = line(TextMode::LcdBgr, DARK_INK, DARK_PAPER, 1.0);
    for (p, q) in rgb.iter().zip(&bgr) {
        let c = coverage(*p, DARK_INK, DARK_PAPER);
        let d = coverage(*q, DARK_INK, DARK_PAPER);
        assert!((c[0] - d[2]).abs() < 0.02, "red<->blue: {c:?} vs {d:?}");
        assert!((c[1] - d[1]).abs() < 0.02, "green stays: {c:?} vs {d:?}");
        assert!((c[2] - d[0]).abs() < 0.02, "blue<->red: {c:?} vs {d:?}");
    }
    assert!(fringed(&bgr, DARK_INK, DARK_PAPER) > 8);
}

/// Translucent text is drawn in grayscale even in an LCD mode, as Windows does.
#[test]
fn translucent_text_falls_back_to_grayscale() {
    let _mode = mode_lock();
    let frame = line(TextMode::LcdRgb, DARK_INK, DARK_PAPER, 0.5);
    for &p in &frame {
        let c = coverage(p, DARK_INK, DARK_PAPER);
        assert!(
            max(c) - min(c) < 0.03,
            "channels should be equal, got {c:?}"
        );
    }
}

/// A colorized icon goes through the ordinary alpha-map path: its pixels don't care about the
/// text mode.
#[test]
fn a_colorized_icon_is_unchanged_by_the_mode() {
    let _mode = mode_lock();
    let mark = {
        // A small filled ring: an alpha map with partial coverage on its edges.
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(16, 16);
        for (i, px) in buffer.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = ((i % 16) as f32 - 7.5, (i / 16) as f32 - 7.5);
            let r = (x * x + y * y).sqrt();
            let alpha = ((8.0 - r) * 255.0).clamp(0.0, 255.0) as u8;
            *px = Rgba8Pixel {
                r: 255,
                g: 255,
                b: 255,
                a: alpha,
            };
        }
        Image::from_rgba8(buffer)
    };
    let render = |mode| {
        set_text_mode(mode);
        let window = window(RepaintBufferType::NewBuffer);
        let icon = Icon::new().unwrap();
        icon.set_mark(mark.clone());
        icon.show().unwrap();
        frame(&window)
    };
    let first = render(TextMode::LcdRgb);
    assert!(
        first.contains(&[0x33, 0xcc, 0x66]),
        "the icon's solid pixels should be the colorize colour"
    );
    assert_eq!(first, render(TextMode::LcdBgr));
    assert_eq!(first, render(TextMode::Grayscale));
}

/// A mode change repaints the whole window even though no property changed, in a reused buffer
/// where the partial renderer would otherwise redraw nothing.
#[test]
fn a_mode_change_repaints_a_reused_buffer() {
    let _mode = mode_lock();
    set_text_mode(TextMode::LcdRgb);
    let window = window(RepaintBufferType::ReusedBuffer);
    let line = TextLine::new().unwrap();
    line.show().unwrap();
    let mut pixels = vec![SoftBufferPixel::background(); (WIDTH * HEIGHT) as usize];
    let draw = |pixels: &mut Vec<SoftBufferPixel>| {
        assert!(window.draw_if_needed(|renderer| {
            renderer.render(pixels, WIDTH as usize);
        }));
    };
    draw(&mut pixels);
    assert!(fringed(&unpack(&pixels), DARK_INK, DARK_PAPER) > 8);

    set_text_mode(TextMode::Grayscale);
    line.window().request_redraw();
    draw(&mut pixels);
    let repainted = unpack(&pixels);
    assert_eq!(
        fringed(&repainted, DARK_INK, DARK_PAPER),
        0,
        "the whole window should have been redrawn in the new mode"
    );
    assert_stems(&repainted, DARK_INK, DARK_PAPER);
}
