// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use core::num::NonZeroU16;

use alloc::rc::Rc;
use alloc::vec::Vec;
use skrifa::MetadataProvider;

use crate::PhysicalLength;
use crate::fixed::Fixed;
use i_slint_common::sharedfontique::fontique;
use i_slint_core::lengths::PhysicalPx;
use i_slint_core::textlayout::{Glyph, TextShaper};

use super::RenderableVectorGlyph;

// A length in font design space.
struct FontUnit;
type FontLength = euclid::Length<i32, FontUnit>;
type FontScaleFactor = euclid::Scale<f32, FontUnit, PhysicalPx>;

// HTTPorter patch: the glyph is hinted and placed on a whole pixel (see `draw_glyph_run`), so the
// sub-pixel bins (SUBPIXEL_BIN_COUNT) are gone.

/// HTTPorter patch: how a glyph's coverage is rasterised and shaped.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GlyphRaster {
    /// Per-channel coverage (ClearType-style) rather than one coverage per pixel.
    pub lcd: bool,
    /// The coverage curve, chosen by the text colour.
    pub curve: CoverageCurve,
}

/// HTTPorter patch: the curve applied to the coverage, `cov^(1/boost)`, chosen by the luminance
/// of the text colour because the same curve cannot match Windows for both polarities: light
/// text on a dark background needs more ink than the linear blend gives, dark text on a light
/// background slightly less (measured against GDI+ ClearType for both themes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CoverageCurve {
    /// Light text (usually on dark): the boost the user chose, next to real ClearType.
    LightText,
    /// Dark text (usually on light).
    DarkText,
}

impl CoverageCurve {
    const LIGHT_BOOST: f32 = 1.25;
    const DARK_BOOST: f32 = 0.95;

    pub(crate) fn for_text(color: i_slint_core::Color) -> Self {
        let luma = 299 * color.red() as u32 + 587 * color.green() as u32 + 114 * color.blue() as u32;
        if luma >= 128 * 1000 { Self::LightText } else { Self::DarkText }
    }

    fn table(self) -> &'static [u8; 256] {
        static LIGHT: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
        static DARK: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
        let (cell, boost) = match self {
            Self::LightText => (&LIGHT, Self::LIGHT_BOOST),
            Self::DarkText => (&DARK, Self::DARK_BOOST),
        };
        cell.get_or_init(|| {
            core::array::from_fn(|i| {
                let cov = (i as f32 / 255.0).powf(1.0 / boost);
                (cov * 255.0).round().clamp(0.0, 255.0) as u8
            })
        })
    }
}

/// Cache key includes blob id, font index, pixel size, glyph id, a hash of normalized
/// variation coordinates (so different variable font instances produce distinct cache
/// entries) and the way the glyph is rasterised (HTTPorter patch).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
    /// Font blob id.
    font_blob_id: u64,
    /// Font index within the blob.
    font_index: u32,
    /// Rendered pixel size.
    pixel_size: PhysicalLength,
    /// Glyph id.
    glyph_id: core::num::NonZeroU16,
    /// Hash of the normalized variation coordinates.
    coords_hash: u64,
    /// HTTPorter patch: LCD or grayscale, and the coverage curve.
    raster: GlyphRaster,
}

struct RenderableGlyphWeightScale;

impl clru::WeightScale<GlyphCacheKey, RenderableVectorGlyph> for RenderableGlyphWeightScale {
    fn weight(&self, _: &GlyphCacheKey, value: &RenderableVectorGlyph) -> usize {
        // HTTPorter patch: the byte length also counts an LCD map's three bytes per pixel.
        value.alpha_map.len()
    }
}

type GlyphCache = clru::CLruCache<
    GlyphCacheKey,
    RenderableVectorGlyph,
    std::collections::hash_map::RandomState,
    RenderableGlyphWeightScale,
>;

i_slint_core::thread_local!(static GLYPH_CACHE: core::cell::RefCell<GlyphCache>  =
    core::cell::RefCell::new(
        clru::CLruCache::with_config(
            clru::CLruCacheConfig::new(core::num::NonZeroUsize::new(1024 * 1024).unwrap())
                .with_scale(RenderableGlyphWeightScale)
        )
    )
);
// HTTPorter patch: the text mode the cached glyphs were made for.
i_slint_core::thread_local!(static GLYPH_CACHE_MODE: core::cell::Cell<crate::TextMode> =
    core::cell::Cell::new(crate::TextMode::default()));

pub struct VectorFont {
    font_index: u32,
    font_blob: fontique::Blob<u8>,
    swash_key: swash::CacheKey,
    swash_offset: u32,
    ascender: PhysicalLength,
    descender: PhysicalLength,
    height: PhysicalLength,
    pixel_size: PhysicalLength,
    x_height: PhysicalLength,
    cap_height: PhysicalLength,
    /// Normalized variation coordinates (F2Dot14, fvar axis order) for variable font rendering.
    normalized_coords: Vec<i16>,
    /// Hash of normalized_coords for use in the glyph cache key.
    coords_hash: u64,
}

fn hash_coords(coords: &[i16]) -> u64 {
    use core::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    coords.hash(&mut hasher);
    hasher.finish()
}

impl VectorFont {
    fn swash_font_ref(&self) -> swash::FontRef<'_> {
        swash::FontRef {
            data: self.font_blob.data(),
            offset: self.swash_offset,
            key: self.swash_key,
        }
    }

    pub fn new(
        font: fontique::QueryFont,
        swash_key: swash::CacheKey,
        swash_offset: u32,
        pixel_size: PhysicalLength,
    ) -> Self {
        Self::new_from_blob_and_index(font.blob, font.index, swash_key, swash_offset, pixel_size)
    }

    pub fn new_from_blob_and_index(
        font_blob: fontique::Blob<u8>,
        font_index: u32,
        swash_key: swash::CacheKey,
        swash_offset: u32,
        pixel_size: PhysicalLength,
    ) -> Self {
        Self::new_from_blob_and_index_with_coords(
            font_blob,
            font_index,
            swash_key,
            swash_offset,
            pixel_size,
            &[],
        )
    }

    pub fn new_from_blob_and_index_with_coords(
        font_blob: fontique::Blob<u8>,
        font_index: u32,
        swash_key: swash::CacheKey,
        swash_offset: u32,
        pixel_size: PhysicalLength,
        normalized_coords: &[i16],
    ) -> Self {
        let face = skrifa::FontRef::from_index(font_blob.data(), font_index).unwrap();

        let skrifa_coords: Vec<skrifa::instance::NormalizedCoord> = normalized_coords
            .iter()
            .map(|&c| skrifa::instance::NormalizedCoord::from_bits(c))
            .collect();
        let location = skrifa::instance::LocationRef::new(&skrifa_coords);

        let metrics = face.metrics(skrifa::instance::Size::unscaled(), location);

        let ascender = FontLength::new(metrics.ascent as _);
        let descender = FontLength::new(metrics.descent as _);
        let height = FontLength::new((metrics.ascent - metrics.descent) as _);
        let x_height = FontLength::new(metrics.x_height.unwrap_or_default() as _);
        let cap_height = FontLength::new(metrics.cap_height.unwrap_or_default() as _);
        let units_per_em = metrics.units_per_em;
        let scale = FontScaleFactor::new(pixel_size.get() as f32 / units_per_em as f32);
        let coords_hash = hash_coords(normalized_coords);
        Self {
            font_index,
            font_blob,
            swash_key,
            swash_offset,
            ascender: (ascender.cast() * scale).cast(),
            descender: (descender.cast() * scale).cast(),
            height: (height.cast() * scale).cast(),
            pixel_size,
            x_height: (x_height.cast() * scale).cast(),
            cap_height: (cap_height.cast() * scale).cast(),
            normalized_coords: normalized_coords.to_vec(),
            coords_hash,
        }
    }

    pub fn render_vector_glyph(
        &self,
        glyph_id: core::num::NonZeroU16,
        raster: GlyphRaster, // HTTPorter patch: replaces the sub-pixel bin.
        slint_context: &i_slint_core::SlintContext,
    ) -> Option<RenderableVectorGlyph> {
        GLYPH_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();

            // HTTPorter patch: a mode change drops the maps made for the old mode.
            let mode = crate::text_mode::text_mode();
            if GLYPH_CACHE_MODE.with(|m| m.replace(mode)) != mode {
                cache.clear();
            }

            let cache_key = GlyphCacheKey {
                font_blob_id: self.font_blob.id(),
                font_index: self.font_index,
                pixel_size: self.pixel_size,
                glyph_id,
                coords_hash: self.coords_hash,
                raster,
            };

            if let Some(entry) = cache.get(&cache_key) {
                return Some(entry.clone());
            }

            let glyph = {
                let font_ref = self.swash_font_ref();
                let mut ctx = slint_context.swash_scale_context().borrow_mut();
                let mut scaler = ctx
                    .builder(font_ref)
                    .size(self.pixel_size.get() as f32)
                    .hint(true) // HTTPorter patch
                    .normalized_coords(&self.normalized_coords)
                    .build();
                // HTTPorter patch: per-channel coverage for ClearType-style text; no sub-pixel
                // offset, the glyph lands on a whole pixel.
                let image = swash::scale::Render::new(&[swash::scale::Source::Outline])
                    .format(if raster.lcd {
                        swash::zeno::Format::Subpixel
                    } else {
                        swash::zeno::Format::Alpha
                    })
                    .render(&mut scaler, glyph_id.get())?;

                let placement = image.placement;
                // HTTPorter patch: the coverage curve, and three bytes per pixel for LCD maps
                // (swash delivers RGBA; BGR panels swap red and blue here, at the source).
                let curve = raster.curve.table();
                let lcd = raster.lcd && image.content == swash::scale::image::Content::SubpixelMask;
                let alpha_map: Rc<[u8]> = if lcd {
                    let bgr = mode == crate::TextMode::LcdBgr;
                    image
                        .data
                        .chunks_exact(4)
                        .flat_map(|p| {
                            let (r, g, b) = (
                                curve[p[0] as usize],
                                curve[p[1] as usize],
                                curve[p[2] as usize],
                            );
                            if bgr { [b, g, r] } else { [r, g, b] }
                        })
                        .collect()
                } else {
                    image.data.iter().map(|&c| curve[c as usize]).collect()
                };

                Some(RenderableVectorGlyph {
                    x: Fixed::from_integer(placement.left),
                    y: Fixed::from_integer(placement.top - placement.height as i32),
                    width: PhysicalLength::new(placement.width.try_into().unwrap()),
                    height: PhysicalLength::new(placement.height.try_into().unwrap()),
                    alpha_map,
                    pixel_stride: placement.width.try_into().unwrap(),
                    glyph_origin_x: placement.left as f32,
                    lcd, // HTTPorter patch
                })
            };

            if let Some(ref glyph) = glyph {
                cache.put_with_weight(cache_key, glyph.clone()).ok();
            }
            glyph
        })
    }
}

impl TextShaper for VectorFont {
    type LengthPrimitive = i16;
    type Length = PhysicalLength;
    fn shape_text<GlyphStorage: core::iter::Extend<Glyph<PhysicalLength>>>(
        &self,
        text: &str,
        glyphs: &mut GlyphStorage,
    ) {
        let font_ref = self.swash_font_ref();
        let charmap = font_ref.charmap();
        let gm = font_ref.glyph_metrics(&[]);
        let metrics = font_ref.metrics(&[]);
        let scale = self.pixel_size.get() as f32 / metrics.units_per_em as f32;

        glyphs.extend(text.char_indices().map(|(byte_offset, char)| {
            let glyph_id = NonZeroU16::try_from(charmap.map(char)).ok();
            let x_advance = glyph_id.map_or_else(
                || self.pixel_size.get(),
                |id| (gm.advance_width(id.get()) * scale) as _,
            );

            Glyph {
                glyph_id,
                advance: PhysicalLength::new(x_advance),
                text_byte_offset: byte_offset,
                ..Default::default()
            }
        }));
    }

    fn glyph_for_char(&self, ch: char) -> Option<Glyph<PhysicalLength>> {
        let font_ref = self.swash_font_ref();
        let charmap = font_ref.charmap();
        let gm = font_ref.glyph_metrics(&[]);
        let metrics = font_ref.metrics(&[]);
        let scale = self.pixel_size.get() as f32 / metrics.units_per_em as f32;

        NonZeroU16::try_from(charmap.map(ch)).ok().map(|glyph_id| Glyph {
            glyph_id: Some(glyph_id),
            advance: PhysicalLength::new((gm.advance_width(glyph_id.get()) * scale) as _),
            ..Default::default()
        })
    }
}

impl i_slint_core::textlayout::FontMetrics<PhysicalLength> for VectorFont {
    fn ascent(&self) -> PhysicalLength {
        self.ascender
    }

    fn height(&self) -> PhysicalLength {
        self.height
    }

    fn descent(&self) -> PhysicalLength {
        self.descender
    }

    fn x_height(&self) -> PhysicalLength {
        self.x_height
    }

    fn cap_height(&self) -> PhysicalLength {
        self.cap_height
    }
}

impl super::GlyphRenderer for VectorFont {
    fn render_glyph(
        &self,
        glyph_id: core::num::NonZeroU16,
        slint_context: &i_slint_core::SlintContext,
    ) -> Option<super::RenderableGlyph> {
        // HTTPorter patch: this non-parley path takes plain alpha maps; the curve for light text.
        let raster = GlyphRaster { lcd: false, curve: CoverageCurve::LightText };
        self.render_vector_glyph(glyph_id, raster, slint_context).map(|glyph| super::RenderableGlyph {
            x: glyph.x,
            y: glyph.y,
            width: glyph.width,
            height: glyph.height,
            alpha_map: glyph.alpha_map.into(),
            pixel_stride: glyph.pixel_stride,
            sdf: false,
        })
    }

    fn scale_delta(&self) -> super::Fixed<u16, 8> {
        super::Fixed::from_integer(1)
    }
}
