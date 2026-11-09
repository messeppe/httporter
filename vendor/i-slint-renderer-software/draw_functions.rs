// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore premultiply
#![allow(clippy::identity_op)] // We use x + 0 a lot here for symmetry

//! This is the module for the functions that are drawing the pixels
//! on the line buffer

use super::{Fixed, PhysicalLength, PhysicalRect};
use derive_more::{Add, Mul, Sub};
use i_slint_core::Color;
use i_slint_core::graphics::{Rgb8Pixel, TexturePixelFormat};
use i_slint_core::lengths::{PointLengths, SizeLengths};
#[allow(unused_imports)]
use num_traits::Float;

/// Draw one line of the texture in the line buffer
///
pub(super) fn draw_texture_line(
    span: &PhysicalRect,
    line: PhysicalLength,
    texture: &super::SceneTexture,
    line_buffer: &mut [impl TargetPixel],
    extra_clip_begin: i16,
    extra_clip_end: i16,
) {
    let super::SceneTexture {
        data,
        format,
        pixel_stride,
        extra:
            super::SceneTextureExtra { colorize, alpha, rotation, dx, dy, off_x, off_y, glyph_blend }, // HTTPorter patch: glyph_blend
    } = *texture;

    let source_size = texture.source_size().cast::<i32>();
    let len = line_buffer.len();
    let y = line - span.origin.y_length();
    let y = if rotation.mirror_width() { span.size.height - y.get() - 1 } else { y.get() } as i32;

    let off_y = Fixed::<i32, 8>::from_fixed(off_y);
    let dx = Fixed::<i32, 8>::from_fixed(dx);
    let dy = Fixed::<i32, 8>::from_fixed(dy);
    let off_x = Fixed::<i32, 8>::from_fixed(off_x);

    if !rotation.is_transpose() {
        let mut delta = dx;
        let row = off_y + dy * y;
        // The position where to start in the image array for a this row
        let row_offset = (row.truncate() % source_size.height) as usize * pixel_stride as usize;
        let mut tile_start = 0;

        // the size of the tile in physical pixels in the target
        let tile_len = (Fixed::from_integer(source_size.width) / delta) as usize;
        // the amount of missing image pixel on one tile
        let mut remainder = Fixed::from_integer(source_size.width) % delta;
        // The position in image pixel where to get the image
        let mut pos;
        // the end index in the target buffer
        let mut end;
        // the accumulated error in image pixels
        let mut acc_err;
        if rotation.mirror_height() {
            let o = (off_x + (delta * (extra_clip_end as i32 + len as i32 - 1)))
                % Fixed::from_integer(source_size.width);
            pos = o;
            tile_start = source_size.width;
            end = (o / delta) as usize + 1;
            acc_err = -delta + o % delta;
            delta = -delta;
            remainder = -remainder;
        } else {
            let o =
                (off_x + delta * extra_clip_begin as i32) % Fixed::from_integer(source_size.width);
            pos = o;
            end = ((Fixed::from_integer(source_size.width) - o) / delta) as usize;
            acc_err = (Fixed::from_integer(source_size.width) - o) % delta;
            if acc_err != Fixed::default() {
                acc_err = delta - acc_err;
                end += 1;
            }
        }
        end = end.min(len);
        let mut begin = 0;
        let row_fract = row.fract();
        while begin < len {
            fetch_blend_pixel(
                &mut line_buffer[begin..end],
                format,
                data,
                alpha,
                colorize,
                glyph_blend, // HTTPorter patch
                (pixel_stride as usize, dy),
                #[inline(always)]
                |bpp| {
                    let p = ((row_offset + pos.truncate() as usize) * bpp, pos.fract(), row_fract);
                    pos += delta;
                    p
                },
            );
            begin = end;
            end += tile_len;
            pos = acc_err + Fixed::from_integer(tile_start);
            if remainder != Fixed::from_integer(0) {
                acc_err -= remainder;
                let wrap = if rotation.mirror_height() {
                    acc_err >= Fixed::from_integer(0)
                } else {
                    acc_err < Fixed::from_integer(0)
                };
                if wrap {
                    acc_err += delta;
                    end += 1;
                }
            };
            end = end.min(len);
        }
    } else {
        let bpp = format.bpp();
        let col = off_x + dx * y;
        let col_fract = col.fract();
        let col = (col.truncate() % source_size.width) as usize * bpp;
        let stride = pixel_stride as usize * bpp;
        let mut row_delta = dy;
        let tile_len = (Fixed::from_integer(source_size.height) / row_delta) as usize;
        let mut remainder = Fixed::from_integer(source_size.height) % row_delta;
        let mut end;
        let mut row_init = Fixed::default();
        let mut row;
        let mut acc_err;
        if rotation.mirror_height() {
            row_init = Fixed::from_integer(source_size.height);
            row = (off_y + (row_delta * (extra_clip_end as i32 + len as i32 - 1)))
                % Fixed::from_integer(source_size.height);
            end = (row / row_delta) as usize + 1;
            acc_err = -row_delta + row % row_delta;
            row_delta = -row_delta;
            remainder = -remainder;
        } else {
            row = (off_y + row_delta * extra_clip_begin as i32)
                % Fixed::from_integer(source_size.height);
            end = ((Fixed::from_integer(source_size.height) - row) / row_delta) as usize;
            acc_err = (Fixed::from_integer(source_size.height) - row) % row_delta;
            if acc_err != Fixed::default() {
                acc_err = row_delta - acc_err;
                end += 1;
            }
        };
        end = end.min(len);
        let mut begin = 0;
        while begin < len {
            fetch_blend_pixel(
                &mut line_buffer[begin..end],
                format,
                data,
                alpha,
                colorize,
                glyph_blend, // HTTPorter patch
                (stride, dy),
                #[inline(always)]
                |_| {
                    let pos = (row.truncate() as usize * stride + col, col_fract, row.fract());
                    row += row_delta;
                    pos
                },
            );
            begin = end;
            end += tile_len;
            row = row_init;
            row += acc_err;
            if remainder != Fixed::from_integer(0) {
                acc_err -= remainder;
                let wrap = if rotation.mirror_height() {
                    acc_err >= Fixed::from_integer(0)
                } else {
                    acc_err < Fixed::from_integer(0)
                };
                if wrap {
                    acc_err += row_delta;
                    end += 1;
                }
            };
            end = end.min(len);
        }
    };

    fn fetch_blend_pixel(
        line_buffer: &mut [impl TargetPixel],
        format: TexturePixelFormat,
        data: &[u8],
        alpha: u8,
        color: Color,
        glyph_blend: Option<GlyphBlend>, // HTTPorter patch
        (stride, delta): (usize, Fixed<i32, 8>),
        mut pos: impl FnMut(usize) -> (usize, u8, u8),
    ) {
        // HTTPorter patch: a DirectWrite glyph, with the gamma and contrast correction.
        if let Some(blend) = glyph_blend {
            if format == TexturePixelFormat::Rgb {
                blend_lcd_line(line_buffer, data, color, blend, pos);
            } else {
                blend_gray_glyph_line(line_buffer, data, color, alpha, blend, pos);
            }
            return;
        }
        match format {
            TexturePixelFormat::Rgb => {
                for pix in line_buffer {
                    let pos = pos(3).0;
                    let p: &[u8] = &data[pos..pos + 3];
                    if alpha == 0xff {
                        *pix = TargetPixel::from_rgb(p[0], p[1], p[2]);
                    } else {
                        pix.blend(PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
                            alpha, p[0], p[1], p[2],
                        )))
                    }
                }
            }
            TexturePixelFormat::Rgba => {
                if color.alpha() == 0 {
                    for pix in line_buffer {
                        let pos = pos(4).0;
                        let p: &[u8] = &data[pos..pos + 4];
                        let alpha = ((p[3] as u16 * alpha as u16) / 255) as u8;
                        if alpha == 0xff {
                            *pix = TargetPixel::from_rgb(p[0], p[1], p[2]);
                        } else {
                            pix.blend(PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
                                alpha, p[0], p[1], p[2],
                            )));
                        }
                    }
                } else {
                    for pix in line_buffer {
                        let pos = pos(4).0;
                        let alpha = ((data[pos + 3] as u16 * alpha as u16) / 255) as u8;
                        let c = PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
                            alpha,
                            color.red(),
                            color.green(),
                            color.blue(),
                        ));
                        pix.blend(c);
                    }
                }
            }
            TexturePixelFormat::RgbaPremultiplied => {
                if color.alpha() > 0 {
                    for pix in line_buffer {
                        let pos = pos(4).0;
                        let c = PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
                            ((data[pos + 3] as u16 * alpha as u16) / 255) as u8,
                            color.red(),
                            color.green(),
                            color.blue(),
                        ));
                        pix.blend(c);
                    }
                } else if alpha == 0xff {
                    for pix in line_buffer {
                        let pos = pos(4).0;
                        let c = PremultipliedRgbaColor {
                            alpha: data[pos + 3],
                            red: data[pos + 0],
                            green: data[pos + 1],
                            blue: data[pos + 2],
                        };
                        pix.blend(c);
                    }
                } else {
                    for pix in line_buffer {
                        let pos = pos(4).0;
                        let c = PremultipliedRgbaColor {
                            alpha: (data[pos + 3] as u16 * alpha as u16 / 255) as u8,
                            red: (data[pos + 0] as u16 * alpha as u16 / 255) as u8,
                            green: (data[pos + 1] as u16 * alpha as u16 / 255) as u8,
                            blue: (data[pos + 2] as u16 * alpha as u16 / 255) as u8,
                        };
                        pix.blend(c);
                    }
                }
            }
            TexturePixelFormat::AlphaMap => {
                for pix in line_buffer {
                    let pos = pos(1).0;
                    let c = PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
                        ((data[pos] as u16 * alpha as u16) / 255) as u8,
                        color.red(),
                        color.green(),
                        color.blue(),
                    ));
                    pix.blend(c);
                }
            }
            TexturePixelFormat::SignedDistanceField => {
                const RANGE: i32 = 6;
                let factor = (362 * 256 / delta.0) * RANGE; // 362 ≃ 255 * sqrt(2)
                for pix in line_buffer {
                    let (pos, col_f, row_f) = pos(1);
                    let (col_f, row_f) = (col_f as i32, row_f as i32);
                    let mut dist = ((data[pos] as i8 as i32) * (256 - col_f)
                        + (data[pos + 1] as i8 as i32) * col_f)
                        * (256 - row_f);
                    if pos + stride + 1 < data.len() {
                        dist += ((data[pos + stride] as i8 as i32) * (256 - col_f)
                            + (data[pos + stride + 1] as i8 as i32) * col_f)
                            * row_f
                    } else {
                        debug_assert_eq!(row_f, 0);
                    }
                    let a = ((((dist >> 8) * factor) >> 16) + 128).clamp(0, 255) * alpha as i32;
                    let c = PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
                        (a / 255) as u8,
                        color.red(),
                        color.green(),
                        color.blue(),
                    ));
                    pix.blend(c);
                }
            }
        };
    }
}

/// HTTPorter patch: where a pixel type keeps its channels, for per-channel (LCD) blending.
///
/// `TargetPixel` can only blend one coverage for all channels, and the pixel type the winit
/// software backend really draws into (`SoftBufferPixel`, a private `u32` wrapper, 0xAARRGGBB)
/// lives in another crate, so the channels are found by probing the layout of `from_rgb`.
struct ByteChannels {
    red: usize,
    green: usize,
    blue: usize,
    /// The fourth byte of a pixel with one, which `from_rgb` set to 255.
    alpha: Option<usize>,
}

impl ByteChannels {
    /// The layout of `T`, or `None` when it is not 3 or 4 plain bytes (RGB565 and the like).
    fn of<T: TargetPixel>() -> Option<Self> {
        let size = core::mem::size_of::<T>();
        if size != 3 && size != 4 {
            return None;
        }
        let probe = T::from_rgb(0x12, 0x34, 0x56);
        let mut bytes = [0u8; 4];
        // SAFETY: `size` bytes of `probe`, a plain `Copy` value, are read into a buffer that
        // holds at least that many. The only pixel types with the sizes accepted here are
        // Rgb8Pixel, PremultipliedRgbaColor and the winit backend's u32 SoftBufferPixel, none of
        // which has padding.
        #[allow(unsafe_code)]
        unsafe {
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!(probe).cast::<u8>(),
                bytes.as_mut_ptr(),
                size,
            );
        }
        let find = |value: u8| bytes[..size].iter().position(|&b| b == value);
        let (red, green, blue) = (find(0x12)?, find(0x34)?, find(0x56)?);
        let alpha = (0..size).find(|&i| i != red && i != green && i != blue);
        if alpha.is_some_and(|i| bytes[i] != 0xff) {
            return None;
        }
        Some(Self { red, green, blue, alpha })
    }

    /// Replaces each colour channel of the pixel with what `mix` makes of its old value.
    /// A destination that is not opaque keeps premultiplied blending, through `fallback`.
    #[inline]
    fn blend<T: TargetPixel>(
        &self,
        pixel: &mut T,
        mix: impl Fn(u8, usize) -> u8,
        fallback: impl FnOnce(&mut T),
    ) {
        let ptr = (pixel as *mut T).cast::<u8>();
        // SAFETY: `of::<T>` found these offsets for this very `T`, so they are within its size,
        // and the bytes of a pixel are plain channel values. `ptr` is not used after `pixel` is.
        #[allow(unsafe_code)]
        unsafe {
            if self.alpha.is_some_and(|i| *ptr.add(i) != 0xff) {
                fallback(pixel);
                return;
            }
            for (channel, offset) in [self.red, self.green, self.blue].into_iter().enumerate() {
                let byte = ptr.add(offset);
                *byte = mix(*byte, channel);
            }
        }
    }
}

/// HTTPorter patch: the fallback for pixels without byte channels: the mean of the three
/// coverages, drawn as a plain alpha.
fn average_coverage(fg: [u8; 3], cov: [u8; 3]) -> PremultipliedRgbaColor {
    let alpha = ((cov[0] as u32 + cov[1] as u32 + cov[2] as u32) / 3) as u8;
    PremultipliedRgbaColor::premultiply(Color::from_argb_u8(alpha, fg[0], fg[1], fg[2]))
}

/// HTTPorter patch: Windows' text settings: what the correction below needs from them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextGamma {
    pub gamma: f32,
    pub gamma_ratios: [f32; 4],
    pub enhanced_contrast: f32,
    pub grayscale_enhanced_contrast: f32,
    pub cleartype_level: f32,
    pub tables: &'static GammaTables,
}

/// HTTPorter patch: 8-bit channel values to gamma-linear light and back, for the ClearType
/// blend, per pixel in `blend_lcd_line`.
#[derive(Debug, PartialEq)]
pub(crate) struct GammaTables {
    to_linear: [f32; 256],
    from_linear: alloc::vec::Vec<u8>,
}

impl GammaTables {
    /// Linear light is quantized finely enough to round-trip within a level at the dark end.
    const STEPS: usize = 1 << 14;

    pub(crate) fn new(gamma: f32) -> Self {
        let to_linear = core::array::from_fn(|i| (i as f32 / 255.0).powf(gamma));
        let from_linear = (0..=Self::STEPS)
            .map(|i| ((i as f32 / Self::STEPS as f32).powf(1.0 / gamma) * 255.0 + 0.5) as u8)
            .collect();
        Self { to_linear, from_linear }
    }

    #[inline]
    fn encode(&self, linear: f32) -> u8 {
        self.from_linear[(linear.clamp(0.0, 1.0) * Self::STEPS as f32 + 0.5) as usize]
    }
}

// HTTPorter patch: the correction DirectWrite applies when it blends text itself, which
// CreateAlphaTexture leaves out. The contrast enhancement and, for grayscale, the alpha
// correction are ported from Windows Terminal (as Zed does):
// https://github.com/microsoft/terminal/blob/main/src/renderer/atlas/dwrite_helpers.cpp and
// dwrite_helpers.hlsl, Copyright (c) Microsoft Corporation, MIT License (the text is in
// THIRD-PARTY-NOTICES). The "thin font" contrast boost is not ported: the fonts it is for
// (Courier New and the like) are not used here.
//
// ClearType is blended differently: `IDWriteBitmapRenderTarget::DrawGlyphRun` does the real
// gamma-correct blend against the destination pixel, which Terminal's shader can't read and
// approximates from the text colour alone (up to 22 levels off, measured against DrawGlyphRun).
// This renderer can read the destination, so it does what Windows does.

/// Terminal's `DWrite_GetGammaRatios`: the constants for [`GlyphBlend`] at a given gamma.
pub(crate) fn gamma_ratios(gamma: f32) -> [f32; 4] {
    const GAMMA_INCORRECT_TARGET_RATIOS: [[f32; 4]; 13] = [
        [0.0000 / 4., 0.0000 / 4., 0.0000 / 4., 0.0000 / 4.], // gamma = 1.0
        [0.0166 / 4., -0.0807 / 4., 0.2227 / 4., -0.0751 / 4.], // gamma = 1.1
        [0.0350 / 4., -0.1760 / 4., 0.4325 / 4., -0.1370 / 4.], // gamma = 1.2
        [0.0543 / 4., -0.2821 / 4., 0.6302 / 4., -0.1876 / 4.], // gamma = 1.3
        [0.0739 / 4., -0.3963 / 4., 0.8167 / 4., -0.2287 / 4.], // gamma = 1.4
        [0.0933 / 4., -0.5161 / 4., 0.9926 / 4., -0.2616 / 4.], // gamma = 1.5
        [0.1121 / 4., -0.6395 / 4., 1.1588 / 4., -0.2877 / 4.], // gamma = 1.6
        [0.1300 / 4., -0.7649 / 4., 1.3159 / 4., -0.3080 / 4.], // gamma = 1.7
        [0.1469 / 4., -0.8911 / 4., 1.4644 / 4., -0.3234 / 4.], // gamma = 1.8
        [0.1627 / 4., -1.0170 / 4., 1.6051 / 4., -0.3347 / 4.], // gamma = 1.9
        [0.1773 / 4., -1.1420 / 4., 1.7385 / 4., -0.3426 / 4.], // gamma = 2.0
        [0.1908 / 4., -1.2652 / 4., 1.8650 / 4., -0.3476 / 4.], // gamma = 2.1
        [0.2031 / 4., -1.3864 / 4., 1.9851 / 4., -0.3501 / 4.], // gamma = 2.2
    ];
    const NORM13: f32 = (0x10000 as f64 / (255 * 255) as f64 * 4.0) as f32;
    const NORM24: f32 = (0x100 as f64 / 255 as f64 * 4.0) as f32;

    let index = ((gamma * 10.0 + 0.5) as isize).clamp(10, 22) - 10;
    let ratios = &GAMMA_INCORRECT_TARGET_RATIOS[index as usize];
    [NORM13 * ratios[0], NORM24 * ratios[1], NORM13 * ratios[2], NORM24 * ratios[3]]
}

/// Terminal's `DWrite_ApplyLightOnDarkContrastAdjustment`.
fn light_on_dark_contrast(enhanced_contrast: f32, color: [f32; 3]) -> f32 {
    let lightness = 0.30 * color[0] + 0.59 * color[1] + 0.11 * color[2];
    enhanced_contrast * (4.0 * (0.75 - lightness)).clamp(0.0, 1.0)
}

/// Terminal's `DWrite_EnhanceContrast`.
fn enhance_contrast(alpha: f32, k: f32) -> f32 {
    alpha * (k + 1.0) / (alpha * k + 1.0)
}

/// Terminal's `DWrite_ApplyAlphaCorrection`.
fn apply_alpha_correction(a: f32, f: f32, g: [f32; 4]) -> f32 {
    a + a * (1.0 - a) * ((g[0] * f + g[1]) * a + (g[2] * f + g[3]))
}

/// HTTPorter patch: the correction for one text colour; computed once per glyph run.
#[derive(Clone, Copy, Debug)]
pub struct GlyphBlend {
    contrast: f32,
    /// The value each channel's correction depends on: the channel's own for ClearType, the
    /// colour's intensity for grayscale, as Terminal does it.
    f: [f32; 3],
    gamma_ratios: [f32; 4],
    cleartype_level: f32,
    /// The text colour in gamma-linear light, and the tables to mix in that space.
    fg_linear: [f32; 3],
    tables: &'static GammaTables,
}

impl GlyphBlend {
    pub(crate) fn new(gamma: TextGamma, color: Color, lcd: bool) -> Self {
        let channel = |c: u8| c as f32 / 255.0;
        let rgb = [channel(color.red()), channel(color.green()), channel(color.blue())];
        if lcd {
            Self {
                contrast: light_on_dark_contrast(gamma.enhanced_contrast, rgb),
                f: rgb,
                gamma_ratios: gamma.gamma_ratios,
                cleartype_level: gamma.cleartype_level,
                fg_linear: [color.red(), color.green(), color.blue()]
                    .map(|c| gamma.tables.to_linear[c as usize]),
                tables: gamma.tables,
            }
        } else {
            // Terminal's `DWrite_CalcColorIntensity`.
            let intensity = 0.25 * rgb[0] + 0.5 * rgb[1] + 0.25 * rgb[2];
            Self {
                contrast: light_on_dark_contrast(gamma.grayscale_enhanced_contrast, rgb),
                f: [intensity; 3],
                gamma_ratios: gamma.gamma_ratios,
                cleartype_level: 1.0,
                fg_linear: [0.0; 3],
                tables: gamma.tables,
            }
        }
    }

    /// The corrected coverage of one channel, 0 to 255.
    #[inline]
    fn correct(&self, coverage: u8, channel: usize) -> u8 {
        let alpha = coverage as f32 / 255.0;
        let contrasted = enhance_contrast(alpha, self.contrast);
        let corrected = apply_alpha_correction(contrasted, self.f[channel], self.gamma_ratios);
        (corrected.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }

    /// ClearType's blend of one channel: the gamma-correct mix of the text colour and the
    /// destination, with the channel's contrast-enhanced coverage.
    #[inline]
    fn lcd_channel(&self, dst: u8, coverage: f32, channel: usize) -> u8 {
        let a = enhance_contrast(coverage, self.contrast);
        self.tables.encode(self.fg_linear[channel] * a + self.tables.to_linear[dst as usize] * (1.0 - a))
    }

    /// The ClearType level (0 is grayscale, 1 is full per-channel coverage).
    #[inline]
    fn level(&self, cov: [u8; 3]) -> [f32; 3] {
        let average = (cov[0] as f32 + cov[1] as f32 + cov[2] as f32) / 3.0;
        let level = self.cleartype_level;
        [0, 1, 2].map(|c| average + (cov[c] as f32 - average) * level)
    }
}

/// HTTPorter patch: one line of a glyph with three coverage bytes per pixel, in the given color.
/// Only ever used with opaque text (`draw_glyph_run` falls back to grayscale below alpha 255).
fn blend_lcd_line<T: TargetPixel>(
    line_buffer: &mut [T],
    data: &[u8],
    color: Color,
    blend: GlyphBlend,
    mut pos: impl FnMut(usize) -> (usize, u8, u8),
) {
    let fg = [color.red(), color.green(), color.blue()];
    // Without byte channels (RGB565 and the like) there is no destination to read: the average
    // coverage, with the correction Terminal does without one.
    let average = |cov: [u8; 3]| {
        let levelled = blend.level(cov);
        average_coverage(fg, [0, 1, 2].map(|c| blend.correct((levelled[c] + 0.5) as u8, c)))
    };
    match ByteChannels::of::<T>() {
        Some(channels) => {
            for pix in line_buffer {
                let p = pos(3).0;
                let cov = [data[p], data[p + 1], data[p + 2]];
                if cov != [0, 0, 0] {
                    let levelled = blend.level(cov);
                    channels.blend(
                        pix,
                        |dst, c| blend.lcd_channel(dst, levelled[c] / 255.0, c),
                        |pix| pix.blend(average(cov)),
                    );
                }
            }
        }
        None => {
            for pix in line_buffer {
                let p = pos(3).0;
                let cov = [data[p], data[p + 1], data[p + 2]];
                pix.blend(average(cov));
            }
        }
    }
}

/// HTTPorter patch: one line of a DirectWrite grayscale glyph, with the correction applied.
fn blend_gray_glyph_line(
    line_buffer: &mut [impl TargetPixel],
    data: &[u8],
    color: Color,
    alpha: u8,
    blend: GlyphBlend,
    mut pos: impl FnMut(usize) -> (usize, u8, u8),
) {
    for pix in line_buffer {
        let p = pos(1).0;
        if data[p] == 0 {
            continue;
        }
        let corrected = blend.correct(data[p], 0);
        pix.blend(PremultipliedRgbaColor::premultiply(Color::from_argb_u8(
            ((corrected as u16 * alpha as u16) / 255) as u8,
            color.red(),
            color.green(),
            color.blue(),
        )));
    }
}

/// draw one line of the rounded rectangle in the line buffer
#[allow(clippy::unnecessary_cast)] // Coord
pub(super) fn draw_rounded_rectangle_line(
    span: &PhysicalRect,
    line: PhysicalLength,
    rr: &super::RoundedRectangle,
    line_buffer: &mut [impl TargetPixel],
    extra_left_clip: i16,
    extra_right_clip: i16,
) {
    /// This is an integer shifted by 4 bits.
    /// Note: this is not a "fixed point" because multiplication and sqrt operation operate to
    /// the shifted integer
    #[derive(Clone, Copy, PartialEq, Ord, PartialOrd, Eq, Add, Sub, Mul)]
    struct Shifted(u32);
    impl Shifted {
        const ONE: Self = Shifted(1 << 4);
        #[track_caller]
        #[inline]
        pub fn new(value: impl TryInto<u32> + core::fmt::Debug + Copy) -> Self {
            Self(value.try_into().unwrap_or_else(|_| panic!("Overflow {value:?}")) << 4)
        }
        #[inline(always)]
        pub fn floor(self) -> u32 {
            self.0 >> 4
        }
        #[inline(always)]
        pub fn ceil(self) -> u32 {
            (self.0 + Self::ONE.0 - 1) >> 4
        }
        #[inline(always)]
        pub fn saturating_sub(self, other: Self) -> Self {
            Self(self.0.saturating_sub(other.0))
        }
        #[inline(always)]
        pub fn sqrt(self) -> Self {
            Self(self.0.isqrt())
        }
    }
    impl core::ops::Mul for Shifted {
        type Output = Shifted;
        #[inline(always)]
        fn mul(self, rhs: Self) -> Self::Output {
            Self(self.0 * rhs.0)
        }
    }
    let width = line_buffer.len();
    let y1 = (line - span.origin.y_length()) + rr.top_clip;
    let y2 = (span.origin.y_length() + span.size.height_length() - line) + rr.bottom_clip
        - PhysicalLength::new(1);
    let y = y1.min(y2);
    debug_assert!(y.get() >= 0,);
    let border = Shifted::new(rr.width.get());
    const ONE: Shifted = Shifted::ONE;
    const ZERO: Shifted = Shifted(0);
    let anti_alias = |x1: Shifted, x2: Shifted, process_pixel: &mut dyn FnMut(usize, u32)| {
        // x1 and x2 are the coordinate on the top and bottom of the intersection of the pixel
        // line and the curve.
        // `process_pixel` be called for the coordinate in the array and a coverage between 0..255
        // This algorithm just go linearly which is not perfect, but good enough.
        for x in x1.floor()..x2.ceil() {
            // the coverage is basically how much of the pixel should be used
            let cov = ((ONE + Shifted::new(x) - x1).0 << 8) / (ONE + x2 - x1).0;
            process_pixel(x as usize, cov);
        }
    };
    let rev = |x: Shifted| {
        (Shifted::new(width) + Shifted::new(rr.right_clip.get() + extra_right_clip))
            .saturating_sub(x)
    };
    let calculate_xxxx = |r: i16, y: i16| {
        let r = Shifted::new(r);
        // `y` is how far away from the center of the circle the current line is.
        let y = r - Shifted::new(y);
        // Circle equation: x = √(r² - y²)
        // Coordinate from the left edge: x' = r - x
        let x2 = r - (r * r).saturating_sub(y * y).sqrt();
        let x1 = r - (r * r).saturating_sub((y - ONE) * (y - ONE)).sqrt();
        let r2 = r.saturating_sub(border);
        let x4 = r - (r2 * r2).saturating_sub(y * y).sqrt();
        let x3 = r - (r2 * r2).saturating_sub((y - ONE) * (y - ONE)).sqrt();
        (x1, x2, x3, x4)
    };

    let (x1, x2, x3, x4, x5, x6, x7, x8) = if let Some(r) = rr.radius.as_uniform() {
        let (x1, x2, x3, x4) =
            if y.get() < r { calculate_xxxx(r, y.get()) } else { (ZERO, ZERO, border, border) };
        (x1, x2, x3, x4, rev(x4), rev(x3), rev(x2), rev(x1))
    } else {
        let (x1, x2, x3, x4) = if y1 < PhysicalLength::new(rr.radius.top_left) {
            calculate_xxxx(rr.radius.top_left, y.get())
        } else if y2 < PhysicalLength::new(rr.radius.bottom_left) {
            calculate_xxxx(rr.radius.bottom_left, y.get())
        } else {
            (ZERO, ZERO, border, border)
        };
        let (x5, x6, x7, x8) = if y1 < PhysicalLength::new(rr.radius.top_right) {
            let x = calculate_xxxx(rr.radius.top_right, y.get());
            (x.3, x.2, x.1, x.0)
        } else if y2 < PhysicalLength::new(rr.radius.bottom_right) {
            let x = calculate_xxxx(rr.radius.bottom_right, y.get());
            (x.3, x.2, x.1, x.0)
        } else {
            (border, border, ZERO, ZERO)
        };
        (x1, x2, x3, x4, rev(x5), rev(x6), rev(x7), rev(x8))
    };
    anti_alias(
        x1.saturating_sub(Shifted::new(rr.left_clip.get() + extra_left_clip)),
        x2.saturating_sub(Shifted::new(rr.left_clip.get() + extra_left_clip)),
        &mut |x, cov| {
            if x >= width {
                return;
            }
            let c = if border == ZERO { rr.inner_color } else { rr.border_color };
            let col = PremultipliedRgbaColor {
                alpha: (((c.alpha as u32) * cov as u32) / 255) as u8,
                red: (((c.red as u32) * cov as u32) / 255) as u8,
                green: (((c.green as u32) * cov as u32) / 255) as u8,
                blue: (((c.blue as u32) * cov as u32) / 255) as u8,
            };
            line_buffer[x].blend(col);
        },
    );
    if y < rr.width {
        // up or down border (x2 .. x7)
        let l = x2
            .ceil()
            .saturating_sub((rr.left_clip.get() + extra_left_clip) as u32)
            .min(width as u32) as usize;
        let r = x7.floor().min(width as u32) as usize;
        if l < r {
            TargetPixel::blend_slice(&mut line_buffer[l..r], rr.border_color)
        }
    } else {
        if border > ZERO {
            // 3. draw the border (between x2 and x3)
            if ONE + x2 <= x3 {
                TargetPixel::blend_slice(
                    &mut line_buffer[x2
                        .ceil()
                        .saturating_sub((rr.left_clip.get() + extra_left_clip) as u32)
                        .min(width as u32) as usize
                        ..x3.floor()
                            .saturating_sub((rr.left_clip.get() + extra_left_clip) as u32)
                            .min(width as u32) as usize],
                    rr.border_color,
                )
            }
            // 4. anti-aliasing for the contents (x3 .. x4)
            anti_alias(
                x3.saturating_sub(Shifted::new(rr.left_clip.get() + extra_left_clip)),
                x4.saturating_sub(Shifted::new(rr.left_clip.get() + extra_left_clip)),
                &mut |x, cov| {
                    if x >= width {
                        return;
                    }
                    let col = interpolate_color(cov, rr.border_color, rr.inner_color);
                    line_buffer[x].blend(col);
                },
            );
        }
        if rr.inner_color.alpha > 0 {
            // 5. inside (x4 .. x5)
            let begin = x4
                .ceil()
                .saturating_sub((rr.left_clip.get() + extra_left_clip) as u32)
                .min(width as u32);
            let end = x5.floor().min(width as u32);
            if begin < end {
                TargetPixel::blend_slice(
                    &mut line_buffer[begin as usize..end as usize],
                    rr.inner_color,
                )
            }
        }
        if border > ZERO {
            // 6. border anti-aliasing: x5..x6
            anti_alias(x5, x6, &mut |x, cov| {
                if x >= width {
                    return;
                }
                let col = interpolate_color(cov, rr.inner_color, rr.border_color);
                line_buffer[x].blend(col)
            });
            // 7. border x6 .. x7
            if ONE + x6 <= x7 {
                TargetPixel::blend_slice(
                    &mut line_buffer[x6.ceil().min(width as u32) as usize
                        ..x7.floor().min(width as u32) as usize],
                    rr.border_color,
                )
            }
        }
    }
    anti_alias(x7, x8, &mut |x, cov| {
        if x >= width {
            return;
        }
        let c = if border == ZERO { rr.inner_color } else { rr.border_color };
        let col = PremultipliedRgbaColor {
            alpha: (((c.alpha as u32) * (255 - cov) as u32) / 255) as u8,
            red: (((c.red as u32) * (255 - cov) as u32) / 255) as u8,
            green: (((c.green as u32) * (255 - cov) as u32) / 255) as u8,
            blue: (((c.blue as u32) * (255 - cov) as u32) / 255) as u8,
        };
        line_buffer[x].blend(col);
    });
}

// a is between 0 and 255. When 0, we get color1, when 255 we get color2
fn interpolate_color(
    a: u32,
    color1: PremultipliedRgbaColor,
    color2: PremultipliedRgbaColor,
) -> PremultipliedRgbaColor {
    let b = 255 - a;

    let al1 = color1.alpha as u32;
    let al2 = color2.alpha as u32;

    let a_ = a * al2;
    let b_ = b * al1;
    let m = a_ + b_;

    if m == 0 {
        return PremultipliedRgbaColor::default();
    }

    PremultipliedRgbaColor {
        alpha: (m / 255) as u8,
        red: ((b * color1.red as u32 + a * color2.red as u32) / 255) as u8,
        green: ((b * color1.green as u32 + a * color2.green as u32) / 255) as u8,
        blue: ((b * color1.blue as u32 + a * color2.blue as u32) / 255) as u8,
    }
}

pub(super) fn draw_linear_gradient(
    rect: &PhysicalRect,
    line: PhysicalLength,
    g: &super::LinearGradientCommand,
    mut buffer: &mut [impl TargetPixel],
    extra_left_clip: i16,
) {
    let fill_col1 = g.flags & 0b010 != 0;
    let fill_col2 = g.flags & 0b100 != 0;
    let invert_slope = g.flags & 0b1 != 0;

    let y = (line.get() - rect.min_y() + g.top_clip.get()) as i32;
    let size_y = (rect.height() + g.top_clip.get() + g.bottom_clip.get()) as i32;
    let start = g.start as i32;

    let (mut color1, mut color2) = (g.color1, g.color2);

    if g.start == 0 {
        let p = if invert_slope {
            (255 - start) * y / size_y
        } else {
            start + (255 - start) * y / size_y
        };
        if (fill_col1 || p >= 0) && (fill_col2 || p < 255) {
            let col = interpolate_color(p.clamp(0, 255) as u32, color1, color2);
            TargetPixel::blend_slice(buffer, col);
        }
        return;
    }

    let size_x = (rect.width() + g.left_clip.get() + g.right_clip.get()) as i32;

    let mut x = if invert_slope {
        (y * size_x * (255 - start)) / (size_y * start)
    } else {
        (size_y - y) * size_x * (255 - start) / (size_y * start)
    } + g.left_clip.get() as i32
        + extra_left_clip as i32;

    let len = ((255 * size_x) / start) as usize;

    if x < 0 {
        let l = (-x as usize).min(buffer.len());
        if invert_slope {
            if fill_col1 {
                TargetPixel::blend_slice(&mut buffer[..l], g.color1);
            }
        } else if fill_col2 {
            TargetPixel::blend_slice(&mut buffer[..l], g.color2);
        }
        buffer = &mut buffer[l..];
        x = 0;
    }

    if buffer.len() + x as usize > len {
        let l = len.saturating_sub(x as usize);
        if invert_slope {
            if fill_col2 {
                TargetPixel::blend_slice(&mut buffer[l..], g.color2);
            }
        } else if fill_col1 {
            TargetPixel::blend_slice(&mut buffer[l..], g.color1);
        }
        buffer = &mut buffer[..l];
    }

    if buffer.is_empty() {
        return;
    }

    if !invert_slope {
        core::mem::swap(&mut color1, &mut color2);
    }

    let dr = (((color2.red as i32 - color1.red as i32) * start) << 15) / (255 * size_x);
    let dg = (((color2.green as i32 - color1.green as i32) * start) << 15) / (255 * size_x);
    let db = (((color2.blue as i32 - color1.blue as i32) * start) << 15) / (255 * size_x);
    let da = (((color2.alpha as i32 - color1.alpha as i32) * start) << 15) / (255 * size_x);

    let mut r = ((color1.red as u32) << 15).wrapping_add((x * dr) as _);
    let mut g = ((color1.green as u32) << 15).wrapping_add((x * dg) as _);
    let mut b = ((color1.blue as u32) << 15).wrapping_add((x * db) as _);
    let mut a = ((color1.alpha as u32) << 15).wrapping_add((x * da) as _);

    if color1.alpha == 255 && color2.alpha == 255 {
        buffer.fill_with(|| {
            let pix = TargetPixel::from_rgb((r >> 15) as u8, (g >> 15) as u8, (b >> 15) as u8);
            r = r.wrapping_add(dr as _);
            g = g.wrapping_add(dg as _);
            b = b.wrapping_add(db as _);
            pix
        })
    } else {
        for pix in buffer {
            pix.blend(PremultipliedRgbaColor {
                red: (r >> 15) as u8,
                green: (g >> 15) as u8,
                blue: (b >> 15) as u8,
                alpha: (a >> 15) as u8,
            });
            r = r.wrapping_add(dr as _);
            g = g.wrapping_add(dg as _);
            b = b.wrapping_add(db as _);
            a = a.wrapping_add(da as _);
        }
    }
}

/// Draw a radial gradient on a line
pub(super) fn draw_radial_gradient(
    rect: &PhysicalRect,
    line: PhysicalLength,
    g: &super::RadialGradientCommand,
    buffer: &mut [impl TargetPixel],
    extra_left_clip: i16,
    _extra_right_clip: i16,
) {
    if g.stops.is_empty() {
        return;
    }

    let center_x = rect.min_x() as f32 + g.center_x;
    let center_y = rect.min_y() as f32 + g.center_y;

    debug_assert!(
        g.radius >= 0.0,
        "radius must be resolved before constructing RadialGradientCommand"
    );
    let max_radius = g.radius.max(f32::EPSILON);

    let start_x = rect.min_x() + extra_left_clip;
    let dy = line.get() as f32 - center_y;
    let dy_squared = dy * dy;

    for (i, pixel) in buffer.iter_mut().enumerate() {
        let x = start_x + i as i16;
        let dx = x as f32 - center_x;
        let distance = (dx * dx + dy_squared).sqrt();
        let position = (distance / max_radius).clamp(0.0, 1.0);

        // Find the two gradient stops to interpolate between
        let mut color = g.stops.first().map(|s| s.color).unwrap_or_default();

        for window in g.stops.windows(2) {
            let stop1 = &window[0];
            let stop2 = &window[1];

            if position >= stop1.position && position <= stop2.position {
                // Interpolate between the two stops
                let t = if stop2.position == stop1.position {
                    0.0
                } else {
                    (position - stop1.position) / (stop2.position - stop1.position)
                };

                let c1 = stop1.color.to_argb_u8();
                let c2 = stop2.color.to_argb_u8();

                let alpha = ((1.0 - t) * c1.alpha as f32 + t * c2.alpha as f32) as u8;
                let red = ((1.0 - t) * c1.red as f32 + t * c2.red as f32) as u8;
                let green = ((1.0 - t) * c1.green as f32 + t * c2.green as f32) as u8;
                let blue = ((1.0 - t) * c1.blue as f32 + t * c2.blue as f32) as u8;

                color = Color::from_argb_u8(alpha, red, green, blue);
                break;
            } else if position > stop2.position {
                color = stop2.color;
            }
        }

        pixel.blend(super::PremultipliedRgbaColor::from(color));
    }
}

/// Draw a conic gradient on a line
pub(super) fn draw_conic_gradient(
    rect: &PhysicalRect,
    line: PhysicalLength,
    g: &super::ConicGradientCommand,
    buffer: &mut [impl TargetPixel],
    extra_left_clip: i16,
    _extra_right_clip: i16,
) {
    if g.stops.is_empty() {
        return;
    }

    let center_x = rect.min_x() as f32 + g.center_x;
    let center_y = rect.min_y() as f32 + g.center_y;

    let start_x = rect.min_x() + extra_left_clip;
    let y = line.get() as f32;

    for (i, pixel) in buffer.iter_mut().enumerate() {
        let x = (start_x + i as i16) as f32;

        // Calculate angle from center to current pixel
        let dx = x - center_x;
        let dy = y - center_y;

        // atan2 returns angle in radians from -π to π
        // For 0deg at north (12 o'clock), we need to rotate by -90 degrees
        let mut angle = dy.atan2(dx) + core::f32::consts::FRAC_PI_2;

        // Normalize angle to [0, 2π]
        while angle < 0.0 {
            angle += 2.0 * core::f32::consts::PI;
        }
        while angle >= 2.0 * core::f32::consts::PI {
            angle -= 2.0 * core::f32::consts::PI;
        }

        // Convert to position in [0, 1]
        let position = angle / (2.0 * core::f32::consts::PI);

        // Find the two gradient stops to interpolate between
        let mut color = g.stops.first().map(|s| s.color).unwrap_or_default();

        for window in g.stops.windows(2) {
            let stop1 = &window[0];
            let stop2 = &window[1];

            if position >= stop1.position && position <= stop2.position {
                // Interpolate between the two stops
                let t = if stop2.position == stop1.position {
                    0.0
                } else {
                    (position - stop1.position) / (stop2.position - stop1.position)
                };

                let c1 = stop1.color.to_argb_u8();
                let c2 = stop2.color.to_argb_u8();

                let alpha = ((1.0 - t) * c1.alpha as f32 + t * c2.alpha as f32) as u8;
                let red = ((1.0 - t) * c1.red as f32 + t * c2.red as f32) as u8;
                let green = ((1.0 - t) * c1.green as f32 + t * c2.green as f32) as u8;
                let blue = ((1.0 - t) * c1.blue as f32 + t * c2.blue as f32) as u8;

                color = Color::from_argb_u8(alpha, red, green, blue);
                break;
            } else if position > stop2.position {
                color = stop2.color;
            }
        }

        pixel.blend(super::PremultipliedRgbaColor::from(color));
    }
}

/// A color whose component have been pre-multiplied by alpha
///
/// The renderer operates faster on pre-multiplied color since it
/// caches the multiplication of its component
///
/// PremultipliedRgbaColor can be constructed from a [`Color`] with
/// the [`From`] trait. This conversion will pre-multiply the color
/// components
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct PremultipliedRgbaColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

/// Convert a non-premultiplied color to a premultiplied one
impl From<Color> for PremultipliedRgbaColor {
    fn from(col: Color) -> Self {
        Self::premultiply(col)
    }
}

impl PremultipliedRgbaColor {
    /// Convert a non premultiplied color to a premultiplied one
    fn premultiply(col: Color) -> Self {
        let a = col.alpha() as u16;
        Self {
            alpha: col.alpha(),
            red: (col.red() as u16 * a / 255) as u8,
            green: (col.green() as u16 * a / 255) as u8,
            blue: (col.blue() as u16 * a / 255) as u8,
        }
    }
}

/// Trait for the pixels in the buffer
pub trait TargetPixel: Sized + Copy {
    /// Blend a single pixel with a color
    fn blend(&mut self, color: PremultipliedRgbaColor);
    /// Blend a color to all the pixel in the slice.
    fn blend_slice(slice: &mut [Self], color: PremultipliedRgbaColor) {
        if color.alpha == u8::MAX {
            slice.fill(Self::from_rgb(color.red, color.green, color.blue))
        } else {
            for x in slice {
                Self::blend(x, color);
            }
        }
    }
    /// Create a pixel from the red, gree, blue component in the range 0..=255
    fn from_rgb(red: u8, green: u8, blue: u8) -> Self;

    /// Pixel which will be filled as the background in case the slint view has transparency
    fn background() -> Self {
        Self::from_rgb(0, 0, 0)
    }
}

impl TargetPixel for Rgb8Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let a = (u8::MAX - color.alpha) as u16;
        self.r = (self.r as u16 * a / 255) as u8 + color.red;
        self.g = (self.g as u16 * a / 255) as u8 + color.green;
        self.b = (self.b as u16 * a / 255) as u8 + color.blue;
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b)
    }
}

impl TargetPixel for PremultipliedRgbaColor {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let a = (u8::MAX - color.alpha) as u16;
        self.red = (self.red as u16 * a / 255) as u8 + color.red;
        self.green = (self.green as u16 * a / 255) as u8 + color.green;
        self.blue = (self.blue as u16 * a / 255) as u8 + color.blue;
        self.alpha = (self.alpha as u16 + color.alpha as u16
            - (self.alpha as u16 * color.alpha as u16) / 255) as u8;
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { red: r, green: g, blue: b, alpha: 255 }
    }

    fn background() -> Self {
        Self { red: 0, green: 0, blue: 0, alpha: 0 }
    }
}

/// A 16bit pixel that has 5 red bits, 6 green bits and  5 blue bits
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Rgb565Pixel(pub u16);

impl Rgb565Pixel {
    const R_MASK: u16 = 0b1111_1000_0000_0000;
    const G_MASK: u16 = 0b0000_0111_1110_0000;
    const B_MASK: u16 = 0b0000_0000_0001_1111;

    /// Return the red component as a u8.
    ///
    /// The bits are shifted so that the result is between 0 and 255
    fn red(self) -> u8 {
        ((self.0 & Self::R_MASK) >> 8) as u8
    }
    /// Return the green component as a u8.
    ///
    /// The bits are shifted so that the result is between 0 and 255
    fn green(self) -> u8 {
        ((self.0 & Self::G_MASK) >> 3) as u8
    }
    /// Return the blue component as a u8.
    ///
    /// The bits are shifted so that the result is between 0 and 255
    fn blue(self) -> u8 {
        ((self.0 & Self::B_MASK) << 3) as u8
    }
}

impl TargetPixel for Rgb565Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let a = (u8::MAX - color.alpha) as u32;
        // convert to 5 bits
        let a = (a + 4) >> 3;

        // 00000ggg_ggg00000_rrrrr000_000bbbbb
        let expanded = (self.0 & (Self::R_MASK | Self::B_MASK)) as u32
            | (((self.0 & Self::G_MASK) as u32) << 16);

        // gggggggg_000rrrrr_rrr000bb_bbbbbb00
        let c =
            ((color.red as u32) << 13) | ((color.green as u32) << 24) | ((color.blue as u32) << 2);
        // gggggg00_000rrrrr_000000bb_bbb00000
        let c = c & 0b11111100_00011111_00000011_11100000;

        let res = expanded * a + c;

        self.0 = ((res >> 21) as u16 & Self::G_MASK)
            | ((res >> 5) as u16 & (Self::R_MASK | Self::B_MASK));
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(((r as u16 & 0b11111000) << 8) | ((g as u16 & 0b11111100) << 3) | (b as u16 >> 3))
    }
}

impl From<Rgb8Pixel> for Rgb565Pixel {
    fn from(p: Rgb8Pixel) -> Self {
        Self::from_rgb(p.r, p.g, p.b)
    }
}

impl From<Rgb565Pixel> for Rgb8Pixel {
    fn from(p: Rgb565Pixel) -> Self {
        Rgb8Pixel { r: p.red(), g: p.green(), b: p.blue() }
    }
}

// cSpell: ignore RRRRRGGG GGGBBBBB bswap

/// A 16bit RGB565 pixel stored in big-endian byte order.
///
/// The in-memory byte layout is `[RRRRRGGG, GGGBBBBB]` regardless of
/// host endianness, the format expected by most SPI display
/// controllers (ILI9341, ILI9342C, ST7789, etc.) without any
/// post-render byte swapping.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Rgb565BigEndianPixel(pub u16);

impl Rgb565BigEndianPixel {
    /// Return the red component as a u8.
    ///
    /// The bits are shifted so that the result is between 0 and 255
    pub fn red(self) -> u8 {
        Rgb565Pixel(u16::from_be(self.0)).red()
    }
    /// Return the green component as a u8.
    ///
    /// The bits are shifted so that the result is between 0 and 255
    pub fn green(self) -> u8 {
        Rgb565Pixel(u16::from_be(self.0)).green()
    }
    /// Return the blue component as a u8.
    ///
    /// The bits are shifted so that the result is between 0 and 255
    pub fn blue(self) -> u8 {
        Rgb565Pixel(u16::from_be(self.0)).blue()
    }
}

impl TargetPixel for Rgb565BigEndianPixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        // Reuse the canonical native-endian Rgb565Pixel::blend by decoding
        // from BE byte order, blending, and re-encoding. On targets with a
        // byte-swap instruction (ARM REV16, RISC-V Zbb rev8, x86 bswap) each
        // `to_be`/`from_be` is one cycle on a little-endian host and a no-op
        // on a big-endian host. Benchmarking this against a direct-BE
        // bit-reassembly variant on Cortex-M33 showed the swap-around-native
        // form generates tighter code.
        let mut native = Rgb565Pixel(u16::from_be(self.0));
        native.blend(color);
        self.0 = native.0.to_be();
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(Rgb565Pixel::from_rgb(r, g, b).0.to_be())
    }
}

impl From<Rgb8Pixel> for Rgb565BigEndianPixel {
    fn from(p: Rgb8Pixel) -> Self {
        Self(Rgb565Pixel::from(p).0.to_be())
    }
}

impl From<Rgb565BigEndianPixel> for Rgb8Pixel {
    fn from(p: Rgb565BigEndianPixel) -> Self {
        Rgb565Pixel(u16::from_be(p.0)).into()
    }
}

#[test]
fn rgb565() {
    let pix565 = Rgb565Pixel::from_rgb(0xff, 0x25, 0);
    let pix888: Rgb8Pixel = pix565.into();
    assert_eq!(pix565, pix888.into());

    let pix565 = Rgb565Pixel::from_rgb(0x56, 0x42, 0xe3);
    let pix888: Rgb8Pixel = pix565.into();
    assert_eq!(pix565, pix888.into());
}

#[test]
fn rgb565_be() {
    // BE should be byte-swapped LE for any color
    for &(r, g, b) in &[(0xff, 0x25, 0u8), (0x56, 0x42, 0xe3), (0, 0xff, 0), (0, 0, 0xff)] {
        let le = Rgb565Pixel::from_rgb(r, g, b);
        let be = Rgb565BigEndianPixel::from_rgb(r, g, b);
        assert_eq!(le.0.swap_bytes(), be.0, "mismatch for ({r}, {g}, {b})");
    }

    // Round-trip through Rgb8Pixel
    let pix_be = Rgb565BigEndianPixel::from_rgb(0xff, 0x25, 0);
    let pix888: Rgb8Pixel = pix_be.into();
    assert_eq!(pix_be, pix888.into());

    let pix_be = Rgb565BigEndianPixel::from_rgb(0x56, 0x42, 0xe3);
    let pix888: Rgb8Pixel = pix_be.into();
    assert_eq!(pix_be, pix888.into());
}

#[test]
fn rgb565_be_blend() {
    // Blending a BE pixel should produce the same visual result as LE
    let color = PremultipliedRgbaColor { red: 127, green: 0, blue: 0, alpha: 127 };

    let mut le = Rgb565Pixel::from_rgb(0, 0, 255);
    le.blend(color);
    let mut be = Rgb565BigEndianPixel::from_rgb(0, 0, 255);
    be.blend(color);
    assert_eq!(le.0.swap_bytes(), be.0);

    // Blend with green over a white background
    let color = PremultipliedRgbaColor { red: 0, green: 200, blue: 0, alpha: 200 };

    let mut le = Rgb565Pixel::from_rgb(255, 255, 255);
    le.blend(color);
    let mut be = Rgb565BigEndianPixel::from_rgb(255, 255, 255);
    be.blend(color);
    assert_eq!(le.0.swap_bytes(), be.0);
}
