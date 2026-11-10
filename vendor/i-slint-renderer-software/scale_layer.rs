// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! HTTPorter patch: this file is new in the vendored copy of the crate.
//!
//! `transform-scale` in the software renderer, drawn the way compositors and Core Animation draw
//! a scaled layer: the element and its children are drawn once, unscaled, into an offscreen
//! buffer of premultiplied pixels, which is then resampled to its scaled size about the
//! transform origin and blended into the target with the element's opacity and clip.
//!
//! Slint lowers `transform-scale` to a `Transform` item whose `render` calls
//! `translate(origin)`, `scale(x, y)`, `rotate(angle)`, `translate(-origin)` and then lets the
//! children render; the item's `restore_state` follows the last child. So `scale` opens the
//! layer and the matching `restore_state` closes it (see `SceneBuilder::scale`). A scale of
//! exactly 1 opens nothing: the old path, at no cost.
//!
//! The resampling is a separable tent (bilinear) filter whose support widens with the
//! reduction (1 / scale source pixels), as image libraries do for "bilinear" with antialiasing,
//! so a reduced element averages its pixels instead of dropping some.

use super::*;
use i_slint_core::graphics::Rgba8Pixel;
use i_slint_core::lengths::ItemTransform;

/// The offscreen pixels a scaled element is drawn into, at its unscaled size.
pub(super) struct LayerTarget {
    pixels: Vec<PremultipliedRgbaColor>,
    width: usize,
    height: usize,
    dirty_range_cache: Vec<core::ops::Range<i16>>,
    dirty_region: PhysicalRegion,
    scale_factor: ScaleFactor,
}

impl LayerTarget {
    fn new(width: usize, height: usize, scale_factor: ScaleFactor) -> Self {
        let full =
            euclid::Box2D::new(euclid::point2(0, 0), euclid::point2(width as i16, height as i16));
        Self {
            pixels: alloc::vec![PremultipliedRgbaColor::default(); width * height],
            width,
            height,
            dirty_range_cache: Vec::new(),
            dirty_region: PhysicalRegion {
                rectangles: [full, Default::default(), Default::default()],
                count: 1,
            },
            scale_factor,
        }
    }

    /// Runs `f` with the ordinary buffer processor over the layer's pixels.
    fn with<R>(
        &mut self,
        f: impl FnOnce(&mut RenderToBuffer<'_, TargetPixelSlice<'_, PremultipliedRgbaColor>>) -> R,
    ) -> R {
        let mut slice = TargetPixelSlice { data: &mut self.pixels, pixel_stride: self.width };
        let mut processor = RenderToBuffer {
            buffer: &mut slice,
            dirty_range_cache: core::mem::take(&mut self.dirty_range_cache),
            dirty_region: self.dirty_region.clone(),
            scale_factor: self.scale_factor,
        };
        let result = f(&mut processor);
        self.dirty_range_cache = processor.dirty_range_cache;
        result
    }
}

impl ProcessScene for LayerTarget {
    fn process_scene_texture(&mut self, geometry: PhysicalRect, texture: SceneTexture<'static>) {
        self.with(|p| p.process_scene_texture(geometry, texture))
    }

    fn process_target_texture(
        &mut self,
        texture: &target_pixel_buffer::DrawTextureArgs,
        clip: PhysicalRect,
    ) {
        self.with(|p| p.process_target_texture(texture, clip))
    }

    fn process_rectangle(
        &mut self,
        args: &target_pixel_buffer::DrawRectangleArgs,
        clip: PhysicalRect,
    ) {
        self.with(|p| p.process_rectangle(args, clip))
    }

    fn process_simple_rectangle(&mut self, geometry: PhysicalRect, color: PremultipliedRgbaColor) {
        self.with(|p| p.process_simple_rectangle(geometry, color))
    }

    fn process_rounded_rectangle(&mut self, geometry: PhysicalRect, data: RoundedRectangle) {
        self.with(|p| p.process_rounded_rectangle(geometry, data))
    }

    fn process_linear_gradient(&mut self, geometry: PhysicalRect, gradient: LinearGradientCommand) {
        self.with(|p| p.process_linear_gradient(geometry, gradient))
    }

    fn process_radial_gradient(&mut self, geometry: PhysicalRect, gradient: RadialGradientCommand) {
        self.with(|p| p.process_radial_gradient(geometry, gradient))
    }

    fn process_conic_gradient(&mut self, geometry: PhysicalRect, gradient: ConicGradientCommand) {
        self.with(|p| p.process_conic_gradient(geometry, gradient))
    }

    #[cfg(feature = "path")]
    fn process_filled_path(
        &mut self,
        path_geometry: PhysicalRect,
        clip_geometry: PhysicalRect,
        commands: alloc::vec::Vec<path::Command>,
        color: PremultipliedRgbaColor,
    ) {
        self.with(|p| p.process_filled_path(path_geometry, clip_geometry, commands, color))
    }

    #[cfg(feature = "path")]
    fn process_stroked_path(
        &mut self,
        path_geometry: PhysicalRect,
        clip_geometry: PhysicalRect,
        commands: alloc::vec::Vec<path::Command>,
        color: PremultipliedRgbaColor,
        stroke_width: f32,
        stroke_line_cap: i_slint_core::items::LineCap,
        stroke_line_join: i_slint_core::items::LineJoin,
        stroke_miter_limit: f32,
    ) {
        self.with(|p| {
            p.process_stroked_path(
                path_geometry,
                clip_geometry,
                commands,
                color,
                stroke_width,
                stroke_line_cap,
                stroke_line_join,
                stroke_miter_limit,
            )
        })
    }
}

/// An open layer: a scaled element being drawn.
pub(super) struct ScaleLayer {
    pub target: LayerTarget,
    /// The render state when the layer was opened (at the transform origin), restored to draw it.
    pub outer: RenderState,
    /// The length of the state stack when the layer was opened: the `restore_state` that
    /// brings it back below this ends the element.
    pub depth: usize,
    /// From the layer's logical coordinates to the enclosing target's.
    pub to_outer: ItemTransform,
    /// The transform origin in the enclosing target, in physical pixels.
    anchor: euclid::Point2D<f32, PhysicalPx>,
    /// The layer's top left pixel, in unscaled physical pixels from the transform origin.
    start: (i32, i32),
    scale: (f32, f32),
}

/// What `SceneBuilder::scale` should do.
pub(super) enum ScaleOutcome {
    /// Draw on as before (unscaled): a scale this renderer doesn't draw.
    Unscaled,
    /// Nothing of the element can show: skip it and its children.
    Hidden,
    /// Draw the element into this layer, in this state.
    Layer(ScaleLayer, RenderState),
}

/// Opens a layer for an element scaled by (`sx`, `sy`) about the current offset.
///
/// `state` is the current state (its offset is the transform origin), `dirty` the part of the
/// target this frame repaints (physical), `window` the window's size (physical).
pub(super) fn open(
    sx: f32,
    sy: f32,
    state: RenderState,
    dirty: Option<PhysicalRect>,
    window: PhysicalSize,
    scale_factor: ScaleFactor,
    depth: usize,
) -> ScaleOutcome {
    if !(sx.is_finite() && sy.is_finite()) || sx < 0. || sy < 0. {
        // A mirror (or nonsense) is not drawn; as before, the element stays as it is.
        return ScaleOutcome::Unscaled;
    }
    if sx == 0. || sy == 0. || state.alpha <= 0. {
        return ScaleOutcome::Hidden;
    }
    let sf = scale_factor.get();
    let o = state.offset;

    // What of the target can show the element, relative to the origin, in scaled coordinates.
    let mut visible = state.clip;
    if let Some(dirty) = dirty {
        let dirty = LogicalRect::new(
            LogicalPoint::new(dirty.origin.x as f32 / sf - o.x, dirty.origin.y as f32 / sf - o.y),
            LogicalSize::new(dirty.size.width as f32 / sf, dirty.size.height as f32 / sf),
        );
        match visible.intersection(&dirty) {
            Some(v) => visible = v,
            None => return ScaleOutcome::Hidden,
        }
    }
    if visible.is_empty() {
        return ScaleOutcome::Hidden;
    }
    // The same in the element's own (unscaled) coordinates.
    let unscaled = LogicalRect::new(
        LogicalPoint::new(visible.origin.x / sx, visible.origin.y / sy),
        LogicalSize::new(visible.size.width / sx, visible.size.height / sy),
    );
    // An element is not larger than the window around its own origin: this bounds the layer
    // of a very small scale, whose visible part would otherwise unscale to something huge.
    let (ww, wh) = (window.width as f32 / sf, window.height as f32 / sf);
    let bound = LogicalRect::new(LogicalPoint::new(-ww, -wh), LogicalSize::new(2. * ww, 2. * wh));
    let Some(unscaled) = unscaled.intersection(&bound) else {
        return ScaleOutcome::Hidden;
    };

    // On whole physical pixels, with the filter's reach around it, so that the pixels at the
    // edge of the visible part are filtered from what really is beyond it.
    let reach = |s: f32| (1. / s).max(1.).ceil() + 1.;
    let x0 = (unscaled.min_x() * sf).floor() - reach(sx);
    let y0 = (unscaled.min_y() * sf).floor() - reach(sy);
    let x1 = (unscaled.max_x() * sf).ceil() + reach(sx);
    let y1 = (unscaled.max_y() * sf).ceil() + reach(sy);
    let (width, height) = ((x1 - x0) as usize, (y1 - y0) as usize);
    if width == 0 || height == 0 {
        return ScaleOutcome::Hidden;
    }
    if width > i16::MAX as usize || height > i16::MAX as usize {
        return ScaleOutcome::Unscaled;
    }
    let start = LogicalVector::new(x0 / sf, y0 / sf);

    let layer = ScaleLayer {
        target: LayerTarget::new(width, height, scale_factor),
        outer: state,
        depth,
        to_outer: ItemTransform::translation(start.x, start.y)
            .then_scale(sx, sy)
            .then_translate(euclid::vec2(o.x, o.y)),
        anchor: euclid::point2(o.x * sf, o.y * sf),
        start: (x0 as i32, y0 as i32),
        scale: (sx, sy),
    };
    // In the layer, the origin sits at `-start`; its clip is the whole layer, and the opacity is
    // the layer's, applied once when it is drawn.
    let inner = RenderState {
        alpha: 1.,
        offset: LogicalPoint::new(-start.x, -start.y),
        clip: LogicalRect::new(
            start.to_point(),
            LogicalSize::new(width as f32 / sf, height as f32 / sf),
        ),
    };
    ScaleOutcome::Layer(layer, inner)
}

/// The layer's pixels resampled to its scaled size, with the physical position of the result
/// in the enclosing target. `None` when nothing of it remains.
fn resample(layer: &ScaleLayer) -> Option<(SharedPixelBuffer<Rgba8Pixel>, i32, i32)> {
    let (sx, sy) = layer.scale;
    let src = &layer.target;
    let (x_start, columns) = taps(src.width, layer.anchor.x, layer.start.0, sx);
    let (y_start, rows) = taps(src.height, layer.anchor.y, layer.start.1, sy);
    if columns.is_empty() || rows.is_empty() {
        return None;
    }
    let (out_w, out_h) = (columns.len(), rows.len());
    if out_w > i16::MAX as usize || out_h > i16::MAX as usize {
        return None;
    }

    // Horizontally: every source row to the output width.
    let mut across = alloc::vec![[0f32; 4]; out_w * src.height];
    for y in 0..src.height {
        let line = &src.pixels[y * src.width..(y + 1) * src.width];
        let out = &mut across[y * out_w..(y + 1) * out_w];
        for (o, (first, weights)) in out.iter_mut().zip(&columns) {
            let mut acc = [0f32; 4];
            for (p, w) in line[*first..].iter().zip(weights) {
                acc[0] += w * p.red as f32;
                acc[1] += w * p.green as f32;
                acc[2] += w * p.blue as f32;
                acc[3] += w * p.alpha as f32;
            }
            *o = acc;
        }
    }

    // Vertically, into the result.
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(out_w as u32, out_h as u32);
    let pixels = buffer.make_mut_slice();
    for (y, (first, weights)) in rows.iter().enumerate() {
        let out = &mut pixels[y * out_w..(y + 1) * out_w];
        for (x, px) in out.iter_mut().enumerate() {
            let mut acc = [0f32; 4];
            for (k, w) in weights.iter().enumerate() {
                let c = &across[(first + k) * out_w + x];
                for i in 0..4 {
                    acc[i] += w * c[i];
                }
            }
            let a = (acc[3] + 0.5).clamp(0., 255.) as u8;
            // Premultiplied: no channel above the alpha (rounding could otherwise overflow
            // the blend).
            let channel = |v: f32| ((v + 0.5).clamp(0., 255.) as u8).min(a);
            *px = Rgba8Pixel { r: channel(acc[0]), g: channel(acc[1]), b: channel(acc[2]), a };
        }
    }
    Some((buffer, x_start, y_start))
}

/// The filter taps of one axis: the first output pixel, and for each output pixel the first
/// source pixel and the weights from there.
///
/// The source pixels are numbered from the transform origin (`anchor`, in output pixels); the
/// layer holds `len` of them from `first_pixel`. Output pixel `x` (its centre at `x + 0.5`)
/// samples the source at `(x + 0.5 - anchor) / scale`; the tent's radius is one source pixel,
/// or `1 / scale` of them when reducing. Taps outside the layer count as transparent, so the
/// edges come out antialiased. Every weight depends only on the output pixel and the origin,
/// not on where the layer starts, so a partial frame (a smaller layer) matches a whole one.
fn taps(len: usize, anchor: f32, first_pixel: i32, scale: f32) -> (i32, Vec<(usize, Vec<f32>)>) {
    let start = (anchor + scale * first_pixel as f32).floor() as i32;
    let end = (anchor + scale * (first_pixel as f32 + len as f32)).ceil() as i32;
    let radius = (1. / scale).max(1.);
    let mut out = Vec::with_capacity((end - start).max(0) as usize);
    for x in start..end {
        // Pixel `j` has its centre at `j`.
        let u = (x as f32 + 0.5 - anchor) / scale - 0.5;
        let lo = (u - radius).floor() as i32 + 1;
        let hi = (u + radius).ceil() as i32 - 1;
        let weight = |j: i32| (1. - (j as f32 - u).abs() / radius).max(0.);
        let total: f32 = (lo..=hi).map(weight).sum();
        let first = lo.max(first_pixel);
        let last = hi.min(first_pixel + len as i32 - 1);
        if total <= 0. || first > last {
            out.push((0, Vec::new()));
            continue;
        }
        let weights = (first..=last).map(|j| weight(j) / total).collect();
        out.push(((first - first_pixel) as usize, weights));
    }
    (start, out)
}

/// Draws a closed layer into the enclosing target, in the state it was opened in.
pub(super) fn composite(
    layer: &ScaleLayer,
    scale_factor: ScaleFactor,
    target: &mut dyn ProcessScene,
) {
    let outer = layer.outer;
    let Some((buffer, x, y)) = resample(layer) else { return };
    let (width, height) = (buffer.width() as usize, buffer.height() as usize);
    let physical_clip: PhysicalRect =
        (outer.clip.translate(outer.offset.to_vector()).cast() * scale_factor).round().cast();
    let geometry = PhysicalRect::new(
        PhysicalPoint::new(x as i16, y as i16),
        PhysicalSize::new(width as i16, height as i16),
    );
    let Some(clipped) = physical_clip.intersection(&geometry) else { return };
    let t = target_pixel_buffer::DrawTextureArgs {
        data: target_pixel_buffer::TextureDataContainer::Shared {
            buffer: SharedBufferData::SharedImage(SharedImageBuffer::RGBA8Premultiplied(buffer)),
            source_rect: PhysicalRect::from_size(PhysicalSize::new(width as i16, height as i16)),
        },
        colorize: None,
        alpha: (outer.alpha * 255.) as u8,
        dst_x: x as _,
        dst_y: y as _,
        dst_width: width,
        dst_height: height,
        rotation: RenderingRotation::NoRotation,
        tiling: None,
        glyph_blend: None,
    };
    target.process_target_texture(&t, clipped);
}
