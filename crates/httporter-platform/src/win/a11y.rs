//! The Windows accessibility settings the window follows: text size and "Animation effects".

/// Windows' text size (Settings › Accessibility › Text size): UISettings.TextScaleFactor (WinRT),
/// clamped 1.0..=2.25; 1.0 on error.
pub fn text_scale() -> f32 {
    // T5 reads the real factor; until then the default.
    1.0
}

/// Whether Windows shows animations (SPI_GETCLIENTAREAANIMATION); true on error.
pub fn animations_enabled() -> bool {
    // T5 reads the real setting; until then the default.
    true
}
