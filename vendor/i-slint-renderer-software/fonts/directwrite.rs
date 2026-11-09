// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! HTTPorter patch: this file is new in the vendored copy of the crate.
//!
//! On Windows the glyphs are rasterised by DirectWrite, the established practice (Windows
//! Terminal, Zed, Alacritty, Chromium and Firefox all do it), so that shapes, spacing and
//! smoothing are Windows' own. The call sequence is the one Zed uses in
//! `crates/gpui_windows/src/direct_write.rs`: in-memory font files, one font face per (blob,
//! index), `GetRecommendedRenderingMode` with natural measuring, and `CreateGlyphRunAnalysis` +
//! `CreateAlphaTexture` for the coverage.
//!
//! `CreateAlphaTexture` gives plain coverage: the gamma and contrast correction Windows applies
//! when it draws text itself is done at blend time (`draw_functions::GlyphBlend`), with the
//! system's rendering parameters read here.
//!
//! COM objects stay on the thread that made them, like the rest of the renderer, so the state
//! is thread-local.

#![allow(unsafe_code)] // COM calls; each one states why it is sound.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem::ManuallyDrop;
use std::collections::HashMap;

use windows::Win32::Graphics::DirectWrite::*;
use windows::core::{BOOL, Interface};

use crate::draw_functions::{GammaTables, TextGamma, gamma_ratios};
use i_slint_common::sharedfontique::fontique;

/// One glyph's coverage from DirectWrite, positioned relative to its pen position.
pub(crate) struct Raster {
    pub left: i32,
    /// Distance from the baseline up to the top of the map.
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// One byte per pixel, or three (red, green, blue) with `lcd`.
    pub data: Vec<u8>,
    pub lcd: bool,
}

struct DirectWrite {
    factory: IDWriteFactory5,
    loader: IDWriteInMemoryFontFileLoader,
    /// `None` for a face DirectWrite could not load: that face keeps using swash.
    faces: HashMap<(u64, u32), Option<IDWriteFontFace3>>,
    gamma: TextGamma,
}

impl Drop for DirectWrite {
    fn drop(&mut self) {
        // SAFETY: the loader was registered on this factory in `new`.
        let _ = unsafe { self.factory.UnregisterFontFileLoader(&self.loader) };
    }
}

impl DirectWrite {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: plain factory creation, and the loader is unregistered in `drop`. The
        // in-memory loader needs Windows 10 1703 or later; on anything older this fails and
        // every glyph falls back to swash.
        unsafe {
            let factory: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let loader = factory.CreateInMemoryFontFileLoader()?;
            factory.RegisterFontFileLoader(&loader)?;
            let gamma = read_gamma(&factory, None)?;
            Ok(Self { factory, loader, faces: HashMap::new(), gamma })
        }
    }

    fn face(&mut self, blob: &fontique::Blob<u8>, index: u32) -> Option<IDWriteFontFace3> {
        if !self.faces.contains_key(&(blob.id(), index)) {
            let face = load_face(&self.factory, &self.loader, blob.data(), index);
            self.faces.insert((blob.id(), index), face);
        }
        self.faces.get(&(blob.id(), index))?.clone()
    }
}

/// Reads Windows' current text settings: what "Adjust ClearType text" stores. The blend tables
/// are kept while the gamma is unchanged (they live as long as the process does, and only a
/// change of gamma in the settings makes new ones).
fn read_gamma(
    factory: &IDWriteFactory5,
    previous: Option<&TextGamma>,
) -> windows::core::Result<TextGamma> {
    // SAFETY: plain COM getters on an object owned for the duration of the call.
    unsafe {
        let params = factory.CreateRenderingParams()?;
        let params: IDWriteRenderingParams1 = params.cast()?;
        let gamma = params.GetGamma();
        let tables = match previous {
            Some(previous) if previous.gamma == gamma => previous.tables,
            _ => &*Box::leak(Box::new(GammaTables::new(gamma))),
        };
        Ok(TextGamma {
            gamma,
            gamma_ratios: gamma_ratios(gamma),
            enhanced_contrast: params.GetEnhancedContrast(),
            grayscale_enhanced_contrast: params.GetGrayscaleEnhancedContrast(),
            cleartype_level: params.GetClearTypeLevel(),
            tables,
        })
    }
}

fn load_face(
    factory: &IDWriteFactory5,
    loader: &IDWriteInMemoryFontFileLoader,
    data: &[u8],
    index: u32,
) -> Option<IDWriteFontFace3> {
    let size = u32::try_from(data.len()).ok()?;
    // SAFETY: `data` is valid for `size` bytes for the call; with no owner object the loader
    // copies the font data, so it does not have to outlive this function. The locals written by
    // `Analyze` are valid for the call.
    unsafe {
        let file = loader
            .CreateInMemoryFontFileReference(factory, data.as_ptr().cast(), size, None)
            .ok()?;
        let mut supported = BOOL(0);
        let mut file_type = DWRITE_FONT_FILE_TYPE_UNKNOWN;
        let mut face_type = DWRITE_FONT_FACE_TYPE_UNKNOWN;
        let mut faces = 0;
        file.Analyze(&mut supported, &mut file_type, Some(&mut face_type), &mut faces).ok()?;
        if !supported.as_bool() {
            return None;
        }
        let face = factory
            .CreateFontFace(face_type, &[Some(file)], index, DWRITE_FONT_SIMULATIONS_NONE)
            .ok()?;
        face.cast().ok()
    }
}

i_slint_core::thread_local!(static STATE: RefCell<Option<DirectWrite>> = RefCell::new(None));

fn with_state<R>(f: impl FnOnce(&mut DirectWrite) -> Option<R>) -> Option<R> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.is_none() {
            *state = DirectWrite::new().ok();
        }
        f(state.as_mut()?)
    })
}

/// The system's gamma and contrast, or `None` when DirectWrite is unavailable.
pub(crate) fn current_gamma() -> Option<TextGamma> {
    with_state(|dw| Some(dw.gamma))
}

/// Re-reads Windows' text settings. Returns whether they changed; then every window needs a
/// repaint, as nothing else marks them dirty.
pub fn refresh_text_settings() -> bool {
    with_state(|dw| {
        let gamma = read_gamma(&dw.factory, Some(&dw.gamma)).ok()?;
        let changed = gamma != dw.gamma;
        dw.gamma = gamma;
        Some(changed)
    })
    .inspect(|&changed| {
        if changed {
            crate::text_mode::note_settings_changed();
        }
    })
    .unwrap_or(false)
}

/// Rasterises one glyph, or returns `None` when DirectWrite can't, so that swash can.
///
/// `subpixel_x` is the fraction of a pixel the pen is past the glyph's left pixel edge.
pub(crate) fn rasterize(
    blob: &fontique::Blob<u8>,
    index: u32,
    em_size: f32,
    glyph: u16,
    subpixel_x: f32,
    lcd: bool,
    bgr: bool,
) -> Option<Raster> {
    with_state(|dw| {
        let face = dw.face(blob, index)?;
        // SAFETY: every call gets pointers to locals valid for the call; `CreateAlphaTexture`
        // fills a buffer of exactly the size asked for by `GetAlphaTextureBounds`.
        unsafe {
            let (mode, grid_fit) = rendering_mode(&face, em_size)?;
            let antialias = if lcd {
                DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE
            } else {
                DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE
            };
            let glyphs = [glyph];
            let advances = [0.0f32];
            let offsets = [DWRITE_GLYPH_OFFSET::default()];
            let run = DWRITE_GLYPH_RUN {
                fontFace: ManuallyDrop::new(Some(face.cast::<IDWriteFontFace>().ok()?)),
                fontEmSize: em_size,
                glyphCount: 1,
                glyphIndices: glyphs.as_ptr(),
                glyphAdvances: advances.as_ptr(),
                glyphOffsets: offsets.as_ptr(),
                isSideways: BOOL(0),
                bidiLevel: 0,
            };
            let analysis = dw.factory.CreateGlyphRunAnalysis(
                &run,
                None,
                mode,
                DWRITE_MEASURING_MODE_NATURAL,
                grid_fit,
                antialias,
                subpixel_x,
                0.0,
            );
            drop(ManuallyDrop::into_inner(run.fontFace));
            let analysis = analysis.ok()?;

            let texture =
                if lcd { DWRITE_TEXTURE_CLEARTYPE_3x1 } else { DWRITE_TEXTURE_ALIASED_1x1 };
            let bounds = analysis.GetAlphaTextureBounds(texture).ok()?;
            let width = u32::try_from(bounds.right - bounds.left).ok()?;
            let height = u32::try_from(bounds.bottom - bounds.top).ok()?;
            let bytes_per_pixel = if lcd { 3 } else { 1 };
            let mut data = vec![0u8; (width * height) as usize * bytes_per_pixel];
            if !data.is_empty() {
                analysis.CreateAlphaTexture(texture, &bounds, &mut data).ok()?;
            }
            if lcd && bgr {
                data.chunks_exact_mut(3).for_each(|p| p.swap(0, 2));
            }
            Some(Raster { left: bounds.left, top: -bounds.top, width, height, data, lcd })
        }
    })
}

/// DirectWrite's recommended mode for this size, natural-measured at 96 DPI (the size is already
/// in physical pixels). Outlines can't be rasterised to a bitmap: those become natural symmetric.
unsafe fn rendering_mode(
    face: &IDWriteFontFace3,
    em_size: f32,
) -> Option<(DWRITE_RENDERING_MODE1, DWRITE_GRID_FIT_MODE)> {
    let mut mode = DWRITE_RENDERING_MODE1::default();
    let mut grid_fit = DWRITE_GRID_FIT_MODE::default();
    // SAFETY: the two locals are valid for the call.
    unsafe {
        face.GetRecommendedRenderingMode(
            em_size,
            96.0,
            96.0,
            None,
            false,
            DWRITE_OUTLINE_THRESHOLD_ANTIALIASED,
            DWRITE_MEASURING_MODE_NATURAL,
            None,
            &mut mode,
            &mut grid_fit,
        )
        .ok()?;
    }
    if mode == DWRITE_RENDERING_MODE1_OUTLINE {
        mode = DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC;
    }
    Some((mode, grid_fit))
}
