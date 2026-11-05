//! The Windows accessibility settings the window follows: text size and "Animation effects".
//!
//! Both are read when asked, never cached: the app reads them at start and again when Windows
//! may have changed them (a settings change, or the window coming back to the front), and they
//! cost a few microseconds each.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::{BOOL, w};

/// The smallest and largest text scale the app follows. Windows offers 100 % to 225 %.
const MIN_SCALE: f32 = 1.0;
const MAX_SCALE: f32 = 2.25;

/// Windows' text size (Settings › Accessibility › Text size), as UISettings.TextScaleFactor
/// gives it, clamped 1.0..=2.25; 1.0 on error.
///
/// The factor is read where Windows keeps it, `TextScaleFactor` (a percentage) under
/// `HKCU\Software\Microsoft\Accessibility`, which is what UISettings reports, rather than
/// through WinRT: windows-rs keeps UISettings' activation factory for the whole process, and a
/// second read made after the COM apartment of the first had closed used the factory of an
/// apartment that was gone and crashed (0xC0000005). With no value stored, the size is 100 %.
pub fn text_scale() -> f32 {
    let mut percent: u32 = 100;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
    // SAFETY: both strings are static and NUL-terminated; the value is a DWORD (the flag makes
    // Windows refuse anything else) written into `percent`, whose size `size` gives.
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Accessibility"),
            w!("TextScaleFactor"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut percent).cast()),
            Some(&raw mut size),
        )
    };
    let percent = (read == ERROR_SUCCESS).then_some(percent);
    clamp_scale(percent.map(|percent| f64::from(percent) / 100.0))
}

/// A text scale as the window uses it: within [`MIN_SCALE`]..=[`MAX_SCALE`], and 1.0 for none or
/// for one that is not a number.
fn clamp_scale(raw: Option<f64>) -> f32 {
    match raw {
        Some(scale) if scale.is_finite() => (scale as f32).clamp(MIN_SCALE, MAX_SCALE),
        _ => MIN_SCALE,
    }
}

/// Whether Windows shows animations (SPI_GETCLIENTAREAANIMATION); true on error.
pub fn animations_enabled() -> bool {
    let mut on = BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer, which points at `on`
    // for the length of the call.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_err() || on.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_scale_stays_within_what_windows_offers() {
        assert_eq!(clamp_scale(Some(1.0)), 1.0);
        assert_eq!(clamp_scale(Some(1.5)), 1.5);
        assert_eq!(clamp_scale(Some(2.25)), 2.25);
        assert_eq!(clamp_scale(Some(0.5)), 1.0);
        assert_eq!(clamp_scale(Some(3.0)), 2.25);
        assert_eq!(clamp_scale(Some(f64::NAN)), 1.0);
        assert_eq!(clamp_scale(Some(f64::INFINITY)), 1.0);
        assert_eq!(clamp_scale(None), 1.0);
    }

    /// Both are read from the desktop's settings, so only their range is known; reading them
    /// again (as the app does whenever Windows may have changed them) must keep working.
    #[test]
    fn the_settings_can_be_read_again_and_again() {
        for _ in 0..3 {
            let scale = text_scale();
            assert!((MIN_SCALE..=MAX_SCALE).contains(&scale), "{scale}");
            let _ = animations_enabled();
        }
    }
}
