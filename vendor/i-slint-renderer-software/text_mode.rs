// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! HTTPorter patch: this file is new in the vendored copy of the crate. Which kind of text
//! smoothing the renderer uses, kept in an atomic so the application can change it from anywhere
//! (HTTPorter follows Windows' ClearType settings).
//!
//! A mode change drops the glyph cache (`fonts::vectorfont`) and forces a full repaint of every
//! window (`SoftwareRenderer::sync_text_mode`).

use core::sync::atomic::{AtomicU8, Ordering};

/// How the software renderer smooths text. Set with [`set_text_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TextMode {
    /// Hinted grayscale, with the same coverage curve as the LCD modes.
    Grayscale,
    /// ClearType-style: red, green and blue get their own coverage, for panels whose stripes
    /// run red, green, blue from left to right.
    #[default]
    LcdRgb,
    /// As [`TextMode::LcdRgb`], for panels whose stripes run blue, green, red.
    LcdBgr,
}

static TEXT_MODE: AtomicU8 = AtomicU8::new(TextMode::LcdRgb as u8);

/// Sets how text is smoothed from the next glyph drawn on. Returns whether the mode changed; if
/// it did, the application should ask every window to redraw (nothing else marks them dirty).
pub fn set_text_mode(mode: TextMode) -> bool {
    TEXT_MODE.swap(mode as u8, Ordering::Relaxed) != mode as u8
}

/// The current text mode.
pub(crate) fn text_mode() -> TextMode {
    match TEXT_MODE.load(Ordering::Relaxed) {
        0 => TextMode::Grayscale,
        2 => TextMode::LcdBgr,
        _ => TextMode::LcdRgb,
    }
}
