// SPDX-License-Identifier: MIT OR Apache-2.0

//! Text in the patched software renderer (`vendor/i-slint-renderer-software`), driven through
//! Slint's real renderer into a real pixel buffer with the pixel type the winit software backend
//! draws into (`SoftBufferPixel`, copied here because it is private to that backend). The
//! pixels are compared with what Windows draws itself for the same glyphs, colours and
//! background: `IDWriteBitmapRenderTarget::DrawGlyphRun` with the same rendering parameters, the
//! same Inter file and size, at the same integer origins.
//!
//! The text mode is a process-wide atomic, so the tests take [`mode_lock`] in turn.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel, TextMode,
    set_text_mode,
};
use slint::platform::{Platform, WindowAdapter};
use slint::{
    Color, ComponentHandle, Image, ModelRc, PhysicalSize, Rgba8Pixel, SharedPixelBuffer,
    SharedString, VecModel,
};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::{DIBSECTION, GetCurrentObject, GetObjectW, OBJ_BITMAP};
use windows::core::{BOOL, Interface};

slint::slint! {
    import "../ui/fonts/Inter-Regular.ttf";

    export component TextLine inherits Window {
        in property <color> ink: #f5f5f7;
        in property <color> paper: #1e1e20;
        in property <float> fade: 1.0;
        in property <string> line: "l I l I l I l";
        width: 400px;
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

    // One glyph per Text element, each at an integer x, so that every pen position is whole.
    export component LetterRow inherits Window {
        in property <color> ink: #f5f5f7;
        in property <color> paper: #1e1e20;
        in property <[string]> letters;
        width: 400px;
        height: 30px;
        background: paper;
        for letter[i] in letters: Text {
            x: 8px + i * 18px;
            y: 6px;
            width: 18px;
            height: 22px;
            text: letter;
            color: ink;
            font-family: "Inter";
            font-size: 13px;
        }
    }

    export component Icon inherits Window {
        in property <image> mark;
        width: 400px;
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

const WIDTH: u32 = 400;
const HEIGHT: u32 = 30;
const DARK_INK: [u8; 3] = [0xf5, 0xf5, 0xf7];
const DARK_PAPER: [u8; 3] = [0x1e, 0x1e, 0x20];
const LIGHT_INK: [u8; 3] = [0x1d, 0x1d, 0x1f];
const LIGHT_PAPER: [u8; 3] = [0xff, 0xff, 0xff];
const INTER_REGULAR: &[u8] = include_bytes!("../ui/fonts/Inter-Regular.ttf");
/// The letters of the body line drawn in the comparison tests, one per integer position.
const LETTERS: &str = "TalkstrghBunySoe3-HPd";
/// How far (0 to 255 per channel) a pixel may be from Windows' own rendering. Set by reasoning,
/// not by measurement: the patched path rounds three times (coverage to 8 bits, the corrected
/// coverage to 8 bits, the blended channel to 8 bits), half a level each, and Windows' own path
/// rounds at least once more, so 3 levels. A larger difference is a real difference.
const TOLERANCE: u8 = 3;

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

fn letter_row(mode: TextMode, ink: [u8; 3], paper: [u8; 3]) -> Vec<[u8; 3]> {
    set_text_mode(mode);
    let window = window(RepaintBufferType::NewBuffer);
    let row = LetterRow::new().unwrap();
    row.set_ink(color(ink));
    row.set_paper(color(paper));
    let letters: Vec<SharedString> = LETTERS.chars().map(|c| c.to_string().into()).collect();
    row.set_letters(ModelRc::new(VecModel::from(letters)));
    row.show().unwrap();
    frame(&window)
}

fn coverage(pixel: [u8; 3], ink: [u8; 3], paper: [u8; 3]) -> [f32; 3] {
    core::array::from_fn(|c| {
        (f32::from(pixel[c]) - f32::from(paper[c])) / (f32::from(ink[c]) - f32::from(paper[c]))
    })
}

fn fringed(frame: &[[u8; 3]], ink: [u8; 3], paper: [u8; 3]) -> usize {
    frame
        .iter()
        .filter(|&&p| {
            let c = coverage(p, ink, paper);
            c.iter().cloned().fold(f32::MIN, f32::max) - c.iter().cloned().fold(f32::MAX, f32::min)
                > 0.15
        })
        .count()
}

/// What Windows draws itself for one glyph at each integer position in `xs`, on a bitmap
/// render target, with the system's rendering parameters.
fn windows_row(
    grayscale: bool,
    ink: [u8; 3],
    paper: [u8; 3],
    baseline: f32,
    xs: &[f32],
    glyphs: &[u16],
) -> Vec<[u8; 3]> {
    // SAFETY: plain COM calls; every pointer is a local valid for the call, and the bitmap bits
    // are read while the render target (and so the bitmap) is alive.
    unsafe {
        let factory: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap();
        let loader = factory.CreateInMemoryFontFileLoader().unwrap();
        factory.RegisterFontFileLoader(&loader).unwrap();
        let file = loader
            .CreateInMemoryFontFileReference(
                &factory,
                INTER_REGULAR.as_ptr().cast(),
                INTER_REGULAR.len() as u32,
                None,
            )
            .unwrap();
        let mut supported = BOOL(0);
        let mut file_type = DWRITE_FONT_FILE_TYPE_UNKNOWN;
        let mut face_type = DWRITE_FONT_FACE_TYPE_UNKNOWN;
        let mut count = 0;
        file.Analyze(
            &mut supported,
            &mut file_type,
            Some(&mut face_type),
            &mut count,
        )
        .unwrap();
        let face = factory
            .CreateFontFace(face_type, &[Some(file)], 0, DWRITE_FONT_SIMULATIONS_NONE)
            .unwrap();
        // Windows' own settings, with the mode and the hinting the renderer uses at this size.
        let system = factory.CreateRenderingParams().unwrap();
        let params = factory
            .CreateCustomRenderingParams(
                system.GetGamma(),
                system.GetEnhancedContrast(),
                system
                    .cast::<IDWriteRenderingParams1>()
                    .unwrap()
                    .GetGrayscaleEnhancedContrast(),
                system.GetClearTypeLevel(),
                system.GetPixelGeometry(),
                DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
                DWRITE_GRID_FIT_MODE_DISABLED,
            )
            .unwrap();
        let interop = factory.GetGdiInterop().unwrap();
        let target = interop
            .CreateBitmapRenderTarget(None, WIDTH, HEIGHT)
            .unwrap();
        target.SetPixelsPerDip(1.0).unwrap();
        let target1: IDWriteBitmapRenderTarget1 = target.cast().unwrap();
        target1
            .SetTextAntialiasMode(if grayscale {
                DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE
            } else {
                DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE
            })
            .unwrap();

        let dc = target.GetMemoryDC();
        let mut dib: DIBSECTION = std::mem::zeroed();
        let bitmap = GetCurrentObject(dc, OBJ_BITMAP);
        let read = GetObjectW(
            bitmap,
            std::mem::size_of::<DIBSECTION>() as i32,
            Some((&raw mut dib).cast()),
        );
        assert_ne!(read, 0, "the bitmap of the render target");
        let (width, height) = (WIDTH as usize, HEIGHT as usize);
        assert_eq!(
            dib.dsBm.bmBitsPixel, 32,
            "the target should be a 32-bit DIB"
        );
        let bits = std::slice::from_raw_parts_mut(dib.dsBm.bmBits.cast::<u8>(), width * height * 4);
        for px in bits.as_chunks_mut::<4>().0 {
            *px = [paper[2], paper[1], paper[0], 0xff];
        }

        let text_color = COLORREF(ink[0] as u32 | (ink[1] as u32) << 8 | (ink[2] as u32) << 16);
        for (&x, &glyph) in xs.iter().zip(glyphs) {
            let glyph_indices = [glyph];
            let advances = [0.0f32];
            let offsets = [DWRITE_GLYPH_OFFSET::default()];
            let run = DWRITE_GLYPH_RUN {
                fontFace: ManuallyDrop::new(Some(face.clone())),
                fontEmSize: 13.0,
                glyphCount: 1,
                glyphIndices: glyph_indices.as_ptr(),
                glyphAdvances: advances.as_ptr(),
                glyphOffsets: offsets.as_ptr(),
                isSideways: BOOL(0),
                bidiLevel: 0,
            };
            target
                .DrawGlyphRun(
                    x,
                    baseline,
                    DWRITE_MEASURING_MODE_NATURAL,
                    &run,
                    &params,
                    text_color,
                    None,
                )
                .unwrap();
            drop(ManuallyDrop::into_inner(run.fontFace));
        }

        // The render target's bitmap is top-down.
        let pixels = bits
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| [p[2], p[1], p[0]])
            .collect();
        let _ = factory.UnregisterFontFileLoader(&loader);
        pixels
    }
}

/// The glyph index of each letter of [`LETTERS`], from the font directly.
fn letter_glyphs() -> Vec<u16> {
    // SAFETY: as in `windows_row`.
    unsafe {
        let factory: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap();
        let loader = factory.CreateInMemoryFontFileLoader().unwrap();
        factory.RegisterFontFileLoader(&loader).unwrap();
        let file = loader
            .CreateInMemoryFontFileReference(
                &factory,
                INTER_REGULAR.as_ptr().cast(),
                INTER_REGULAR.len() as u32,
                None,
            )
            .unwrap();
        let mut supported = BOOL(0);
        let mut file_type = DWRITE_FONT_FILE_TYPE_UNKNOWN;
        let mut face_type = DWRITE_FONT_FACE_TYPE_UNKNOWN;
        let mut count = 0;
        file.Analyze(
            &mut supported,
            &mut file_type,
            Some(&mut face_type),
            &mut count,
        )
        .unwrap();
        let face = factory
            .CreateFontFace(face_type, &[Some(file)], 0, DWRITE_FONT_SIMULATIONS_NONE)
            .unwrap();
        let codepoints: Vec<u32> = LETTERS.chars().map(|c| c as u32).collect();
        let mut glyphs = vec![0u16; codepoints.len()];
        face.GetGlyphIndices(
            codepoints.as_ptr(),
            codepoints.len() as u32,
            glyphs.as_mut_ptr(),
        )
        .unwrap();
        let _ = factory.UnregisterFontFileLoader(&loader);
        glyphs
    }
}

/// Whether a pixel column can't match by construction: Slint clips a Text element to its box, so
/// the overhang of a glyph left of its pen (up to a pixel or two of ClearType fringe) is cut, as
/// it always was. Windows draws it; the columns just left of each letter are left out.
fn clipped_column(column: usize) -> bool {
    [column + 1, column + 2]
        .iter()
        .any(|&c| c >= 8 && (c - 8) % 18 == 0)
}

/// What a comparison found, for the failure message and the report.
#[derive(Clone, Copy, Debug)]
struct Comparison {
    baseline: i32,
    worst: [u8; 3],
    mean: [f64; 3],
    pixels: usize,
    over_tolerance: usize,
    worst_at: (usize, usize),
}

/// Compares the row against Windows' own, trying the integer baseline that lines the two up (the
/// renderer's baseline comes out of Slint's layout).
fn compare(slint: &[[u8; 3]], grayscale: bool, ink: [u8; 3], paper: [u8; 3]) -> Comparison {
    let glyphs = letter_glyphs();
    let xs: Vec<f32> = (0..glyphs.len()).map(|i| (8 + i * 18) as f32).collect();
    let mut best: Option<Comparison> = None;
    for baseline in 10..26 {
        let windows = windows_row(grayscale, ink, paper, baseline as f32, &xs, &glyphs);
        let (mut worst, mut sum, mut pixels, mut over, mut worst_at) =
            ([0u8; 3], [0f64; 3], 0usize, 0usize, (0, 0));
        let mut overall = 0u8;
        for (i, (a, b)) in slint.iter().zip(&windows).enumerate() {
            if (*a == paper && *b == paper) || clipped_column(i % WIDTH as usize) {
                continue;
            }
            pixels += 1;
            let mut any_over = false;
            for c in 0..3 {
                let d = a[c].abs_diff(b[c]);
                worst[c] = worst[c].max(d);
                sum[c] += f64::from(d);
                any_over |= d > TOLERANCE;
                if d > overall {
                    overall = d;
                    worst_at = (i % WIDTH as usize, i / WIDTH as usize);
                }
            }
            over += usize::from(any_over);
        }
        let comparison = Comparison {
            baseline,
            worst,
            mean: sum.map(|v| v / pixels.max(1) as f64),
            pixels,
            over_tolerance: over,
            worst_at,
        };
        if best.is_none_or(|b| {
            (overall, comparison.mean) < (b.worst.iter().copied().max().unwrap(), b.mean)
        }) {
            best = Some(comparison);
        }
    }
    best.expect("baselines were tried")
}

fn assert_matches_windows(slint: Vec<[u8; 3]>, grayscale: bool, ink: [u8; 3], paper: [u8; 3]) {
    let c = compare(&slint, grayscale, ink, paper);
    let report = format!(
        "worst {:?} mean {:.2?} per channel over {} pixels (baseline {}), {} over the tolerance, \
         the worst at {:?}",
        c.worst, c.mean, c.pixels, c.baseline, c.over_tolerance, c.worst_at
    );
    println!("difference from Windows' own: {report}");
    assert!(
        c.worst.iter().all(|&d| d <= TOLERANCE),
        "the text differs from DirectWrite's own beyond {TOLERANCE}: {report}"
    );
}

/// The dark theme's ClearType text is Windows' own, within the tolerance.
#[test]
fn lcd_text_matches_directwrite_on_the_dark_theme() {
    let _mode = mode_lock();
    let frame = letter_row(TextMode::LcdRgb, DARK_INK, DARK_PAPER);
    assert_matches_windows(frame, false, DARK_INK, DARK_PAPER);
}

/// The light theme's ClearType text is Windows' own, within the tolerance.
#[test]
fn lcd_text_matches_directwrite_on_the_light_theme() {
    let _mode = mode_lock();
    let frame = letter_row(TextMode::LcdRgb, LIGHT_INK, LIGHT_PAPER);
    assert_matches_windows(frame, false, LIGHT_INK, LIGHT_PAPER);
}

/// Grayscale mode is Windows' grayscale, and neutral.
#[test]
fn grayscale_text_matches_directwrite() {
    let _mode = mode_lock();
    let frame = letter_row(TextMode::Grayscale, DARK_INK, DARK_PAPER);
    assert_eq!(
        fringed(&frame, DARK_INK, DARK_PAPER),
        0,
        "grayscale has no colour fringes"
    );
    assert_matches_windows(frame, true, DARK_INK, DARK_PAPER);
}

/// The colours where an approximation of the blend (Terminal's shader, which can't read the
/// background) is furthest off, and grayscale on the light theme.
#[test]
fn text_matches_directwrite_in_other_colours() {
    let _mode = mode_lock();
    const ACCENT: [u8; 3] = [0x00, 0x71, 0xe3];
    for (ink, paper) in [
        ([0xa8, 0xa8, 0xad], DARK_PAPER),
        ([0x5c, 0x5c, 0x62], LIGHT_PAPER),
        ([0xff, 0xff, 0xff], ACCENT),
    ] {
        assert_matches_windows(letter_row(TextMode::LcdRgb, ink, paper), false, ink, paper);
    }
    let frame = letter_row(TextMode::Grayscale, LIGHT_INK, LIGHT_PAPER);
    assert_matches_windows(frame, true, LIGHT_INK, LIGHT_PAPER);
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
        let spread =
            c.iter().cloned().fold(f32::MIN, f32::max) - c.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 0.03, "channels should be equal, got {c:?}");
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
    assert_eq!(
        fringed(&unpack(&pixels), DARK_INK, DARK_PAPER),
        0,
        "the whole window should have been redrawn in the new mode"
    );
}
