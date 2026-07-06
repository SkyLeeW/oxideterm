use super::SoftwareAtlas;
use crate::{
    AtlasTextureKind, Background, BackgroundTag, Bounds, ContentMask, Corners, DevicePixels, Edges,
    GpuSpecs, Hsla, MonochromeSprite, PaintSurface, Path, PlatformAtlas, PlatformRenderer, Point,
    PolychromeSprite, PrimitiveBatch, Quad, Rgba, ScaledPixels, Scene, Shadow, Size, Underline,
};
use anyhow::Result;
#[cfg(target_os = "macos")]
use core_video::{
    pixel_buffer::{
        kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        kCVPixelFormatType_422YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_422YpCbCr8BiPlanarVideoRange,
        kCVPixelFormatType_444YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
    },
    r#return::kCVReturnSuccess,
};
#[cfg(target_os = "macos")]
use std::slice;
use std::sync::Arc;

const SHADOW_SAMPLE_STEP: f32 = 1.5;
const WAVE_FREQUENCY: f32 = 2.0;
const WAVE_HEIGHT_RATIO: f32 = 0.8;

// Software mode repaints the whole window today. Shadows are intentionally
// degraded until dirty-region rendering or a cached shadow atlas exists.
const DRAW_SOFTWARE_SHADOWS: bool = false;

pub(crate) struct SoftwareRenderer {
    size: Size<DevicePixels>,
    framebuffer: Vec<u8>,
    atlas: Arc<SoftwareAtlas>,
}

#[derive(Clone, Copy)]
struct PixelRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Clone, Copy, Default)]
struct PixelCorners {
    top_left: f32,
    top_right: f32,
    bottom_right: f32,
    bottom_left: f32,
}

#[derive(Clone, Copy, Default)]
struct PixelEdges {
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
}

#[derive(Clone, Copy)]
struct PremulRgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[derive(Clone, Copy)]
enum BackgroundSampler {
    Solid(PremulRgba),
    LinearGradient {
        from: Rgba,
        to: Rgba,
        center_x: f32,
        center_y: f32,
        direction_x: f32,
        direction_y: f32,
        inverse_direction_length: f32,
        half_extent: f32,
        inverse_extent: f32,
        start: f32,
        inverse_stop_range: Option<f32>,
    },
}

impl SoftwareRenderer {
    pub(crate) fn new(size: Size<DevicePixels>) -> Self {
        let mut renderer = Self {
            size: Size::default(),
            framebuffer: Vec::new(),
            atlas: Arc::new(SoftwareAtlas::new()),
        };
        renderer.resize_framebuffer(size);
        renderer
    }

    pub(crate) fn framebuffer(&self) -> &[u8] {
        &self.framebuffer
    }

    pub(crate) fn size(&self) -> Size<DevicePixels> {
        self.size
    }

    fn resize_framebuffer(&mut self, size: Size<DevicePixels>) {
        self.size = size;
        let width = size.width.0.max(0) as usize;
        let height = size.height.0.max(0) as usize;
        self.framebuffer.resize(width * height * 4, 0);
    }

    fn clear(&mut self) {
        self.framebuffer.fill(0);
    }

    fn paint_scene(&mut self, scene: &Scene) {
        self.clear();
        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::BackdropBlurs(backdrop_blurs) => {
                    for backdrop_blur in backdrop_blurs {
                        self.draw_quad(&backdrop_blur.fallback_quad());
                    }
                }
                PrimitiveBatch::Quads(quads) => {
                    for quad in quads {
                        self.draw_quad(quad);
                    }
                }
                PrimitiveBatch::MonochromeSprites { sprites, .. } => {
                    for sprite in sprites {
                        self.draw_monochrome_sprite(sprite);
                    }
                }
                PrimitiveBatch::PolychromeSprites { sprites, .. } => {
                    for sprite in sprites {
                        self.draw_polychrome_sprite(sprite);
                    }
                }
                PrimitiveBatch::Shadows(shadows) => {
                    if DRAW_SOFTWARE_SHADOWS {
                        for shadow in shadows {
                            self.draw_shadow(shadow);
                        }
                    }
                }
                PrimitiveBatch::Paths(paths) => {
                    for path in paths {
                        self.draw_path(path);
                    }
                }
                PrimitiveBatch::Underlines(underlines) => {
                    for underline in underlines {
                        self.draw_underline(underline);
                    }
                }
                PrimitiveBatch::Surfaces(surfaces) => {
                    for surface in surfaces {
                        self.draw_surface(surface);
                    }
                }
            }
        }
    }

    fn draw_quad(&mut self, quad: &Quad) {
        let Some(rect) = self.pixel_rect(quad.bounds, &quad.content_mask) else {
            return;
        };
        let shape_rect = self.pixel_rect_for_bounds(quad.bounds).unwrap_or(rect);
        let sampler = BackgroundSampler::new(quad.background, shape_rect);
        let corner_radii = PixelCorners::from_scaled(quad.corner_radii, shape_rect);
        self.fill_rounded_rect(rect, shape_rect, corner_radii, |x, y| {
            sampler.color_at(x, y)
        });

        let border_color = PremulRgba::from_hsla(quad.border_color);
        if border_color.a <= 0.0 {
            return;
        }

        let border_widths = PixelEdges::from_scaled(quad.border_widths);
        self.fill_rounded_border(rect, shape_rect, corner_radii, border_widths, border_color);
    }

    fn draw_shadow(&mut self, shadow: &Shadow) {
        let clipped_bounds = shadow.bounds.intersect(&shadow.content_mask.bounds);
        let expanded_bounds = Bounds {
            origin: Point {
                x: ScaledPixels((clipped_bounds.origin.x.0 - shadow.blur_radius.0).max(0.0)),
                y: ScaledPixels((clipped_bounds.origin.y.0 - shadow.blur_radius.0).max(0.0)),
            },
            size: Size {
                width: ScaledPixels(clipped_bounds.size.width.0 + shadow.blur_radius.0 * 2.0),
                height: ScaledPixels(clipped_bounds.size.height.0 + shadow.blur_radius.0 * 2.0),
            },
        };
        let Some(rect) = self.pixel_rect(expanded_bounds, &shadow.content_mask) else {
            return;
        };

        let shadow_rect = self
            .pixel_rect(clipped_bounds, &shadow.content_mask)
            .unwrap_or(rect);
        let corner_radii = PixelCorners::from_scaled(shadow.corner_radii, shadow_rect);
        let color = PremulRgba::from_hsla(shadow.color);
        if color.a <= 0.0 {
            return;
        }

        let blur_radius = shadow.blur_radius.0.max(0.0);
        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let distance = rounded_rect_distance(px, py, shadow_rect, corner_radii);
                if distance > blur_radius {
                    continue;
                }

                // This is a low-cost software fallback approximation. GPU mode
                // keeps the exact shadow shader; CPU mode favors availability.
                let alpha = if blur_radius <= 0.0 {
                    (distance <= 0.0) as u8 as f32
                } else {
                    (1.0 - distance.max(0.0) / blur_radius).powf(SHADOW_SAMPLE_STEP)
                };
                self.blend_pixel(x, y, color.with_alpha_factor(alpha));
            }
        }
    }

    fn draw_path(&mut self, path: &Path<ScaledPixels>) {
        let clipped_bounds = path.bounds.intersect(&path.content_mask.bounds);
        if clipped_bounds.is_empty() {
            return;
        }

        let sampler = BackgroundSampler::new(path.color, pixel_rect_from_bounds(clipped_bounds));
        for triangle in path.vertices.chunks_exact(3) {
            let p0 = triangle[0].xy_position;
            let p1 = triangle[1].xy_position;
            let p2 = triangle[2].xy_position;
            let bounds = triangle_bounds(p0, p1, p2);
            let Some(rect) = self.pixel_rect(bounds, &path.content_mask) else {
                continue;
            };

            for y in rect.top..rect.bottom {
                for x in rect.left..rect.right {
                    let px = x as f32 + 0.5;
                    let py = y as f32 + 0.5;
                    if !triangle_contains(px, py, p0, p1, p2) {
                        continue;
                    }
                    self.blend_pixel(x, y, sampler.color_at(px, py));
                }
            }
        }
    }

    fn draw_underline(&mut self, underline: &Underline) {
        let Some(rect) = self.pixel_rect(underline.bounds, &underline.content_mask) else {
            return;
        };

        let color = PremulRgba::from_hsla(underline.color);
        if color.a <= 0.0 {
            return;
        }

        if underline.wavy == 0 {
            self.fill_rect(rect, color);
            return;
        }

        let height = underline.bounds.size.height.0.max(1.0);
        let thickness = underline.thickness.0.max(1.0);
        let half_thickness = thickness * 0.5;
        let frequency = (std::f32::consts::PI * WAVE_FREQUENCY * thickness) / height;
        let amplitude = (thickness * WAVE_HEIGHT_RATIO) / height;
        let origin_x = underline.bounds.origin.x.0;
        let origin_y = underline.bounds.origin.y.0;

        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let st_x = (px - origin_x) / height;
                let st_y = ((py - origin_y) / height) - 0.5;
                let sine = (st_x * frequency).sin() * amplitude;
                let slope = (st_x * frequency).cos() * amplitude * frequency;
                let distance = ((st_y - sine) / (1.0 + slope * slope).sqrt()) * height;
                let edge_distance = distance.abs() - half_thickness;
                let alpha = (0.5 - edge_distance).clamp(0.0, 1.0);
                self.blend_pixel(x, y, color.with_alpha_factor(alpha));
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn draw_surface(&mut self, surface: &PaintSurface) {
        let Some(rect) = self.pixel_rect(surface.bounds, &surface.content_mask) else {
            return;
        };

        let pixel_buffer = &surface.image_buffer;
        if pixel_buffer.lock_base_address(0) != kCVReturnSuccess {
            return;
        }

        let pixel_format = pixel_buffer.get_pixel_format();
        if pixel_format == kCVPixelFormatType_32BGRA {
            self.draw_bgra_surface(surface, rect);
        } else if is_supported_ycbcr_format(pixel_format) {
            self.draw_ycbcr_surface(surface, rect);
        }

        let _ = pixel_buffer.unlock_base_address(0);
    }

    #[cfg(not(target_os = "macos"))]
    fn draw_surface(&mut self, _surface: &PaintSurface) {}

    #[cfg(target_os = "macos")]
    fn draw_bgra_surface(&mut self, surface: &PaintSurface, rect: PixelRect) {
        let pixel_buffer = &surface.image_buffer;
        let width = pixel_buffer.get_width();
        let height = pixel_buffer.get_height();
        let stride = pixel_buffer.get_bytes_per_row();
        if width == 0 || height == 0 || stride == 0 {
            return;
        }

        let base_address = unsafe { pixel_buffer.get_base_address() as *const u8 };
        if base_address.is_null() {
            return;
        }

        let data = unsafe { slice::from_raw_parts(base_address, stride * height) };
        let dest_width = (rect.right - rect.left).max(1) as usize;
        let dest_height = (rect.bottom - rect.top).max(1) as usize;
        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                let source_x =
                    ((x - rect.left) as usize * width / dest_width).min(width.saturating_sub(1));
                let source_y =
                    ((y - rect.top) as usize * height / dest_height).min(height.saturating_sub(1));
                let index = source_y * stride + source_x * 4;
                if index + 3 >= data.len() {
                    continue;
                }
                self.blend_pixel(
                    x,
                    y,
                    PremulRgba::new(
                        data[index + 2] as f32 / 255.0,
                        data[index + 1] as f32 / 255.0,
                        data[index] as f32 / 255.0,
                        data[index + 3] as f32 / 255.0,
                    ),
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn draw_ycbcr_surface(&mut self, surface: &PaintSurface, rect: PixelRect) {
        let pixel_buffer = &surface.image_buffer;
        if pixel_buffer.get_plane_count() < 2 {
            return;
        }

        let y_width = pixel_buffer.get_width_of_plane(0);
        let y_height = pixel_buffer.get_height_of_plane(0);
        let y_stride = pixel_buffer.get_bytes_per_row_of_plane(0);
        let uv_width = pixel_buffer.get_width_of_plane(1);
        let uv_height = pixel_buffer.get_height_of_plane(1);
        let uv_stride = pixel_buffer.get_bytes_per_row_of_plane(1);
        if y_width == 0 || y_height == 0 || uv_width == 0 || uv_height == 0 {
            return;
        }

        let y_base = unsafe { pixel_buffer.get_base_address_of_plane(0) as *const u8 };
        let uv_base = unsafe { pixel_buffer.get_base_address_of_plane(1) as *const u8 };
        if y_base.is_null() || uv_base.is_null() {
            return;
        }

        let y_data = unsafe { slice::from_raw_parts(y_base, y_stride * y_height) };
        let uv_data = unsafe { slice::from_raw_parts(uv_base, uv_stride * uv_height) };
        let dest_width = (rect.right - rect.left).max(1) as usize;
        let dest_height = (rect.bottom - rect.top).max(1) as usize;
        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                let source_x = ((x - rect.left) as usize * y_width / dest_width)
                    .min(y_width.saturating_sub(1));
                let source_y = ((y - rect.top) as usize * y_height / dest_height)
                    .min(y_height.saturating_sub(1));
                let uv_x = (source_x * uv_width / y_width).min(uv_width.saturating_sub(1));
                let uv_y = (source_y * uv_height / y_height).min(uv_height.saturating_sub(1));

                let y_index = source_y * y_stride + source_x;
                let uv_index = uv_y * uv_stride + uv_x * 2;
                if y_index >= y_data.len() || uv_index + 1 >= uv_data.len() {
                    continue;
                }

                let luma = y_data[y_index] as f32 / 255.0;
                let cb = uv_data[uv_index] as f32 / 255.0;
                let cr = uv_data[uv_index + 1] as f32 / 255.0;
                self.blend_pixel(x, y, ycbcr_to_premul(luma, cb, cr));
            }
        }
    }

    fn draw_monochrome_sprite(&mut self, sprite: &MonochromeSprite) {
        let Some(rect) = self.pixel_rect(sprite.bounds, &sprite.content_mask) else {
            return;
        };
        let atlas = self.atlas.clone();
        let Some(()) = atlas.with_pixels_for_tile(&sprite.tile, |kind, tile_size, tile_bytes| {
            if kind != AtlasTextureKind::Monochrome {
                return;
            }

            self.draw_monochrome_sprite_pixels(sprite, rect, tile_size, tile_bytes);
        }) else {
            return;
        };
    }

    fn draw_monochrome_sprite_pixels(
        &mut self,
        sprite: &MonochromeSprite,
        rect: PixelRect,
        tile_size: Size<DevicePixels>,
        tile_bytes: &[u8],
    ) {
        let color = Rgba::from(sprite.color);
        let source_width = tile_size.width.0.max(1) as usize;
        let source_height = tile_size.height.0.max(1) as usize;
        let dest_width = (rect.right - rect.left).max(1) as usize;
        let dest_height = (rect.bottom - rect.top).max(1) as usize;

        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                let source_x = ((x - rect.left) as usize * source_width / dest_width)
                    .min(source_width.saturating_sub(1));
                let source_y = ((y - rect.top) as usize * source_height / dest_height)
                    .min(source_height.saturating_sub(1));
                let alpha = tile_bytes[source_y * source_width + source_x] as f32 / 255.0;
                let source = PremulRgba::new(color.r, color.g, color.b, color.a * alpha);
                self.blend_pixel(x, y, source);
            }
        }
    }

    fn draw_polychrome_sprite(&mut self, sprite: &PolychromeSprite) {
        let Some(rect) = self.pixel_rect(sprite.bounds, &sprite.content_mask) else {
            return;
        };
        let atlas = self.atlas.clone();
        let Some(()) = atlas.with_pixels_for_tile(&sprite.tile, |kind, tile_size, tile_bytes| {
            if kind != AtlasTextureKind::Polychrome {
                return;
            }

            self.draw_polychrome_sprite_pixels(sprite, rect, tile_size, tile_bytes);
        }) else {
            return;
        };
    }

    fn draw_polychrome_sprite_pixels(
        &mut self,
        sprite: &PolychromeSprite,
        rect: PixelRect,
        tile_size: Size<DevicePixels>,
        tile_bytes: &[u8],
    ) {
        let source_width = tile_size.width.0.max(1) as usize;
        let source_height = tile_size.height.0.max(1) as usize;
        let dest_width = (rect.right - rect.left).max(1) as usize;
        let dest_height = (rect.bottom - rect.top).max(1) as usize;
        let shape_rect = self.pixel_rect_for_bounds(sprite.bounds).unwrap_or(rect);
        let corner_radii = PixelCorners::from_scaled(sprite.corner_radii, shape_rect);

        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                if !corner_radii.is_zero()
                    && !rounded_rect_contains(
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                        shape_rect,
                        corner_radii,
                    )
                {
                    continue;
                }
                if let Some(source) = polychrome_source_color(
                    tile_bytes,
                    x,
                    y,
                    shape_rect,
                    source_width,
                    source_height,
                    dest_width,
                    dest_height,
                    sprite.grayscale,
                    sprite.opacity,
                ) {
                    self.blend_pixel(x, y, source);
                }
            }
        }
    }

    fn fill_rect(&mut self, rect: PixelRect, color: PremulRgba) {
        if color.a <= 0.0 {
            return;
        }
        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                self.blend_pixel(x, y, color);
            }
        }
    }

    fn fill_rounded_rect(
        &mut self,
        rect: PixelRect,
        shape_rect: PixelRect,
        corner_radii: PixelCorners,
        mut color_at: impl FnMut(f32, f32) -> PremulRgba,
    ) {
        if corner_radii.is_zero() {
            for y in rect.top..rect.bottom {
                for x in rect.left..rect.right {
                    let px = x as f32 + 0.5;
                    let py = y as f32 + 0.5;
                    self.blend_pixel(x, y, color_at(px, py));
                }
            }
            return;
        }

        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                if !rounded_rect_contains(x as f32 + 0.5, y as f32 + 0.5, shape_rect, corner_radii)
                {
                    continue;
                }
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                self.blend_pixel(x, y, color_at(px, py));
            }
        }
    }

    fn fill_rounded_border(
        &mut self,
        rect: PixelRect,
        shape_rect: PixelRect,
        corner_radii: PixelCorners,
        border_widths: PixelEdges,
        color: PremulRgba,
    ) {
        if color.a <= 0.0 || !border_widths.any() {
            return;
        }

        let inner_rect = shape_rect.inset(border_widths);
        let inner_corners = corner_radii.inset(border_widths);
        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let inside_outer = rounded_rect_contains(px, py, shape_rect, corner_radii);
                let inside_inner = inner_rect
                    .map(|inner_rect| rounded_rect_contains(px, py, inner_rect, inner_corners))
                    .unwrap_or(false);
                if inside_outer && !inside_inner {
                    self.blend_pixel(x, y, color);
                }
            }
        }
    }

    fn blend_pixel(&mut self, x: i32, y: i32, source: PremulRgba) {
        if source.a <= 0.0 || x < 0 || y < 0 {
            return;
        }
        let width = self.size.width.0.max(0) as usize;
        let height = self.size.height.0.max(0) as usize;
        let x = x as usize;
        let y = y as usize;
        if x >= width || y >= height {
            return;
        }
        let index = (y * width + x) * 4;
        let dest = PremulRgba {
            r: self.framebuffer[index] as f32 / 255.0,
            g: self.framebuffer[index + 1] as f32 / 255.0,
            b: self.framebuffer[index + 2] as f32 / 255.0,
            a: self.framebuffer[index + 3] as f32 / 255.0,
        };
        let out = source.over(dest);
        self.framebuffer[index] = to_u8(out.r);
        self.framebuffer[index + 1] = to_u8(out.g);
        self.framebuffer[index + 2] = to_u8(out.b);
        self.framebuffer[index + 3] = to_u8(out.a);
    }

    fn pixel_rect(
        &self,
        bounds: Bounds<ScaledPixels>,
        content_mask: &ContentMask<ScaledPixels>,
    ) -> Option<PixelRect> {
        let clipped = bounds.intersect(&content_mask.bounds);
        let left = clipped.origin.x.0.floor().max(0.0) as i32;
        let top = clipped.origin.y.0.floor().max(0.0) as i32;
        let right = (clipped.origin.x.0 + clipped.size.width.0)
            .ceil()
            .min(self.size.width.0 as f32) as i32;
        let bottom = (clipped.origin.y.0 + clipped.size.height.0)
            .ceil()
            .min(self.size.height.0 as f32) as i32;
        (right > left && bottom > top).then_some(PixelRect {
            left,
            top,
            right,
            bottom,
        })
    }

    fn pixel_rect_for_bounds(&self, bounds: Bounds<ScaledPixels>) -> Option<PixelRect> {
        let left = bounds.origin.x.0.floor().max(0.0) as i32;
        let top = bounds.origin.y.0.floor().max(0.0) as i32;
        let right = (bounds.origin.x.0 + bounds.size.width.0)
            .ceil()
            .min(self.size.width.0 as f32) as i32;
        let bottom = (bounds.origin.y.0 + bounds.size.height.0)
            .ceil()
            .min(self.size.height.0 as f32) as i32;
        (right > left && bottom > top).then_some(PixelRect {
            left,
            top,
            right,
            bottom,
        })
    }
}

impl PlatformRenderer for SoftwareRenderer {
    fn resize(&mut self, size: Size<DevicePixels>) -> Result<()> {
        if self.size != size {
            self.resize_framebuffer(size);
        }
        Ok(())
    }

    fn draw(&mut self, scene: &Scene) -> Result<()> {
        self.paint_scene(scene);
        Ok(())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    fn gpu_specs(&self) -> Result<Option<GpuSpecs>> {
        Ok(Some(GpuSpecs {
            is_software_emulated: true,
            device_name: "OxideTerm software renderer".into(),
            driver_name: "CPU framebuffer".into(),
            driver_info: "software".into(),
        }))
    }
}

impl PixelRect {
    fn width(self) -> f32 {
        (self.right - self.left).max(0) as f32
    }

    fn height(self) -> f32 {
        (self.bottom - self.top).max(0) as f32
    }

    fn inset(self, edges: PixelEdges) -> Option<Self> {
        let left = self.left + edges.left.ceil() as i32;
        let top = self.top + edges.top.ceil() as i32;
        let right = self.right - edges.right.ceil() as i32;
        let bottom = self.bottom - edges.bottom.ceil() as i32;
        (right > left && bottom > top).then_some(Self {
            left,
            top,
            right,
            bottom,
        })
    }
}

impl PixelCorners {
    fn from_scaled(corners: Corners<ScaledPixels>, rect: PixelRect) -> Self {
        let max_radius = rect.width().min(rect.height()) / 2.0;
        Self {
            top_left: corners.top_left.0.clamp(0.0, max_radius),
            top_right: corners.top_right.0.clamp(0.0, max_radius),
            bottom_right: corners.bottom_right.0.clamp(0.0, max_radius),
            bottom_left: corners.bottom_left.0.clamp(0.0, max_radius),
        }
    }

    fn inset(self, edges: PixelEdges) -> Self {
        Self {
            top_left: (self.top_left - edges.top.max(edges.left)).max(0.0),
            top_right: (self.top_right - edges.top.max(edges.right)).max(0.0),
            bottom_right: (self.bottom_right - edges.bottom.max(edges.right)).max(0.0),
            bottom_left: (self.bottom_left - edges.bottom.max(edges.left)).max(0.0),
        }
    }

    fn is_zero(self) -> bool {
        self.top_left <= 0.0
            && self.top_right <= 0.0
            && self.bottom_right <= 0.0
            && self.bottom_left <= 0.0
    }
}

impl PixelEdges {
    fn from_scaled(edges: Edges<ScaledPixels>) -> Self {
        Self {
            top: edges.top.0.max(0.0),
            right: edges.right.0.max(0.0),
            bottom: edges.bottom.0.max(0.0),
            left: edges.left.0.max(0.0),
        }
    }

    fn any(self) -> bool {
        self.top > 0.0 || self.right > 0.0 || self.bottom > 0.0 || self.left > 0.0
    }
}

impl PremulRgba {
    fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        let a = a.clamp(0.0, 1.0);
        Self {
            r: r.clamp(0.0, 1.0) * a,
            g: g.clamp(0.0, 1.0) * a,
            b: b.clamp(0.0, 1.0) * a,
            a,
        }
    }

    fn from_hsla(color: Hsla) -> Self {
        let color = Rgba::from(color);
        Self::new(color.r, color.g, color.b, color.a)
    }

    fn over(self, dest: Self) -> Self {
        let inverse_alpha = 1.0 - self.a;
        Self {
            r: self.r + dest.r * inverse_alpha,
            g: self.g + dest.g * inverse_alpha,
            b: self.b + dest.b * inverse_alpha,
            a: self.a + dest.a * inverse_alpha,
        }
    }

    fn with_alpha_factor(self, factor: f32) -> Self {
        let factor = factor.clamp(0.0, 1.0);
        Self {
            r: self.r * factor,
            g: self.g * factor,
            b: self.b * factor,
            a: self.a * factor,
        }
    }
}

impl BackgroundSampler {
    fn new(background: Background, rect: PixelRect) -> Self {
        if background.tag != BackgroundTag::LinearGradient {
            return Self::Solid(PremulRgba::from_hsla(background.solid));
        }

        let width = rect.width().max(1.0);
        let height = rect.height().max(1.0);
        let radians = ((background.gradient_angle_or_pattern_height % 360.0) - 90.0).to_radians();
        let mut direction_x = radians.cos();
        let mut direction_y = radians.sin();
        if width > height {
            direction_y *= height / width;
        } else {
            direction_x *= width / height;
        }

        let direction_length = (direction_x * direction_x + direction_y * direction_y).sqrt();
        if direction_length <= f32::EPSILON {
            return Self::Solid(PremulRgba::from_hsla(background.colors[0].color));
        }

        let half_width = width * 0.5;
        let half_height = height * 0.5;
        let (half_extent, inverse_extent) = if direction_x.abs() > direction_y.abs() {
            (half_width, 1.0 / width)
        } else {
            (half_height, 1.0 / height)
        };
        let start = background.colors[0].percentage;
        let end = background.colors[1].percentage;
        let inverse_stop_range =
            ((end - start).abs() > f32::EPSILON).then_some(1.0 / (end - start));

        Self::LinearGradient {
            from: Rgba::from(background.colors[0].color),
            to: Rgba::from(background.colors[1].color),
            center_x: rect.left as f32 + half_width,
            center_y: rect.top as f32 + half_height,
            direction_x,
            direction_y,
            inverse_direction_length: 1.0 / direction_length,
            half_extent,
            inverse_extent,
            start,
            inverse_stop_range,
        }
    }

    fn color_at(self, x: f32, y: f32) -> PremulRgba {
        match self {
            Self::Solid(color) => color,
            Self::LinearGradient {
                from,
                to,
                center_x,
                center_y,
                direction_x,
                direction_y,
                inverse_direction_length,
                half_extent,
                inverse_extent,
                start,
                inverse_stop_range,
            } => {
                let projection = ((x - center_x) * direction_x + (y - center_y) * direction_y)
                    * inverse_direction_length;
                let mut t = (projection + half_extent) * inverse_extent;
                if let Some(inverse_stop_range) = inverse_stop_range {
                    t = (t - start) * inverse_stop_range;
                }
                let t = t.clamp(0.0, 1.0);
                PremulRgba::new(
                    lerp(from.r, to.r, t),
                    lerp(from.g, to.g, t),
                    lerp(from.b, to.b, t),
                    lerp(from.a, to.a, t),
                )
            }
        }
    }
}

fn polychrome_source_color(
    tile_bytes: &[u8],
    x: i32,
    y: i32,
    rect: PixelRect,
    source_width: usize,
    source_height: usize,
    dest_width: usize,
    dest_height: usize,
    grayscale: bool,
    opacity: f32,
) -> Option<PremulRgba> {
    let source_x =
        ((x - rect.left) as usize * source_width / dest_width).min(source_width.saturating_sub(1));
    let source_y = ((y - rect.top) as usize * source_height / dest_height)
        .min(source_height.saturating_sub(1));
    let source_index = (source_y * source_width + source_x) * 4;
    if source_index + 3 >= tile_bytes.len() {
        return None;
    }

    let mut r = tile_bytes[source_index] as f32 / 255.0;
    let mut g = tile_bytes[source_index + 1] as f32 / 255.0;
    let mut b = tile_bytes[source_index + 2] as f32 / 255.0;
    if grayscale {
        let gray = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        r = gray;
        g = gray;
        b = gray;
    }
    let a = (tile_bytes[source_index + 3] as f32 / 255.0) * opacity;
    Some(PremulRgba::new(r, g, b, a))
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

fn rounded_rect_contains(x: f32, y: f32, rect: PixelRect, corners: PixelCorners) -> bool {
    if x < rect.left as f32
        || x >= rect.right as f32
        || y < rect.top as f32
        || y >= rect.bottom as f32
    {
        return false;
    }

    let left = rect.left as f32;
    let top = rect.top as f32;
    let right = rect.right as f32;
    let bottom = rect.bottom as f32;

    if corner_contains(
        x,
        y,
        left + corners.top_left,
        top + corners.top_left,
        corners.top_left,
        x < left + corners.top_left && y < top + corners.top_left,
    ) {
        return true;
    }
    if corner_contains(
        x,
        y,
        right - corners.top_right,
        top + corners.top_right,
        corners.top_right,
        x >= right - corners.top_right && y < top + corners.top_right,
    ) {
        return true;
    }
    if corner_contains(
        x,
        y,
        right - corners.bottom_right,
        bottom - corners.bottom_right,
        corners.bottom_right,
        x >= right - corners.bottom_right && y >= bottom - corners.bottom_right,
    ) {
        return true;
    }
    if corner_contains(
        x,
        y,
        left + corners.bottom_left,
        bottom - corners.bottom_left,
        corners.bottom_left,
        x < left + corners.bottom_left && y >= bottom - corners.bottom_left,
    ) {
        return true;
    }

    let in_corner_area = (x < left + corners.top_left && y < top + corners.top_left)
        || (x >= right - corners.top_right && y < top + corners.top_right)
        || (x >= right - corners.bottom_right && y >= bottom - corners.bottom_right)
        || (x < left + corners.bottom_left && y >= bottom - corners.bottom_left);
    !in_corner_area
}

fn rounded_rect_distance(x: f32, y: f32, rect: PixelRect, corners: PixelCorners) -> f32 {
    if rounded_rect_contains(x, y, rect, corners) {
        return 0.0;
    }

    let nearest_x = x.clamp(rect.left as f32, rect.right as f32);
    let nearest_y = y.clamp(rect.top as f32, rect.bottom as f32);
    let dx = x - nearest_x;
    let dy = y - nearest_y;
    (dx * dx + dy * dy).sqrt()
}

fn corner_contains(
    x: f32,
    y: f32,
    center_x: f32,
    center_y: f32,
    radius: f32,
    active: bool,
) -> bool {
    if !active {
        return false;
    }
    if radius <= 0.0 {
        return true;
    }

    let dx = x - center_x;
    let dy = y - center_y;
    dx * dx + dy * dy <= radius * radius
}

fn triangle_bounds(
    p0: Point<ScaledPixels>,
    p1: Point<ScaledPixels>,
    p2: Point<ScaledPixels>,
) -> Bounds<ScaledPixels> {
    let min_x = p0.x.0.min(p1.x.0).min(p2.x.0);
    let min_y = p0.y.0.min(p1.y.0).min(p2.y.0);
    let max_x = p0.x.0.max(p1.x.0).max(p2.x.0);
    let max_y = p0.y.0.max(p1.y.0).max(p2.y.0);
    Bounds {
        origin: Point {
            x: ScaledPixels(min_x),
            y: ScaledPixels(min_y),
        },
        size: Size {
            width: ScaledPixels((max_x - min_x).max(0.0)),
            height: ScaledPixels((max_y - min_y).max(0.0)),
        },
    }
}

fn triangle_contains(
    x: f32,
    y: f32,
    p0: Point<ScaledPixels>,
    p1: Point<ScaledPixels>,
    p2: Point<ScaledPixels>,
) -> bool {
    let area = edge_function(p0, p1, p2.x.0, p2.y.0);
    if area.abs() <= f32::EPSILON {
        return false;
    }

    let w0 = edge_function(p1, p2, x, y);
    let w1 = edge_function(p2, p0, x, y);
    let w2 = edge_function(p0, p1, x, y);
    if area > 0.0 {
        w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0
    } else {
        w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0
    }
}

fn edge_function(a: Point<ScaledPixels>, b: Point<ScaledPixels>, x: f32, y: f32) -> f32 {
    (x - a.x.0) * (b.y.0 - a.y.0) - (y - a.y.0) * (b.x.0 - a.x.0)
}

fn pixel_rect_from_bounds(bounds: Bounds<ScaledPixels>) -> PixelRect {
    PixelRect {
        left: bounds.origin.x.0.floor() as i32,
        top: bounds.origin.y.0.floor() as i32,
        right: (bounds.origin.x.0 + bounds.size.width.0).ceil() as i32,
        bottom: (bounds.origin.y.0 + bounds.size.height.0).ceil() as i32,
    }
}

#[cfg(target_os = "macos")]
fn ycbcr_to_premul(y: f32, cb: f32, cr: f32) -> PremulRgba {
    PremulRgba::new(
        y + 1.4020 * cr - 0.7010,
        y - 0.3441 * cb - 0.7141 * cr + 0.5291,
        y + 1.7720 * cb - 0.8860,
        1.0,
    )
}

#[cfg(target_os = "macos")]
fn is_supported_ycbcr_format(pixel_format: u32) -> bool {
    pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        || pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        || pixel_format == kCVPixelFormatType_422YpCbCr8BiPlanarVideoRange
        || pixel_format == kCVPixelFormatType_422YpCbCr8BiPlanarFullRange
        || pixel_format == kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange
        || pixel_format == kCVPixelFormatType_444YpCbCr8BiPlanarFullRange
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blue, bounds, linear_color_stop, linear_gradient, point, px, red, size, solid_background,
    };

    fn alpha_at(renderer: &SoftwareRenderer, x: usize, y: usize) -> u8 {
        let width = renderer.size.width.0.max(0) as usize;
        renderer.framebuffer[(y * width + x) * 4 + 3]
    }

    fn color_at(renderer: &SoftwareRenderer, x: usize, y: usize) -> [u8; 4] {
        let width = renderer.size.width.0.max(0) as usize;
        let index = (y * width + x) * 4;
        [
            renderer.framebuffer[index],
            renderer.framebuffer[index + 1],
            renderer.framebuffer[index + 2],
            renderer.framebuffer[index + 3],
        ]
    }

    fn mask(width: f32, height: f32) -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: Bounds {
                origin: Point {
                    x: ScaledPixels(0.0),
                    y: ScaledPixels(0.0),
                },
                size: Size {
                    width: ScaledPixels(width),
                    height: ScaledPixels(height),
                },
            },
        }
    }

    #[test]
    fn rounded_rect_clips_corner_pixels() {
        let mut renderer = SoftwareRenderer::new(size(DevicePixels(4), DevicePixels(4)));
        renderer.fill_rounded_rect(
            PixelRect {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            },
            PixelRect {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            },
            PixelCorners {
                top_left: 2.0,
                top_right: 2.0,
                bottom_right: 2.0,
                bottom_left: 2.0,
            },
            |_, _| PremulRgba::new(1.0, 0.0, 0.0, 1.0),
        );

        assert_eq!(alpha_at(&renderer, 0, 0), 0);
        assert_eq!(alpha_at(&renderer, 2, 2), 255);
    }

    #[test]
    fn border_does_not_fill_inner_rect() {
        let mut renderer = SoftwareRenderer::new(size(DevicePixels(5), DevicePixels(5)));
        renderer.fill_rounded_border(
            PixelRect {
                left: 0,
                top: 0,
                right: 5,
                bottom: 5,
            },
            PixelRect {
                left: 0,
                top: 0,
                right: 5,
                bottom: 5,
            },
            PixelCorners::default(),
            PixelEdges {
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
                left: 1.0,
            },
            PremulRgba::new(0.0, 1.0, 0.0, 1.0),
        );

        assert_eq!(alpha_at(&renderer, 0, 2), 255);
        assert_eq!(alpha_at(&renderer, 2, 2), 0);
    }

    #[test]
    fn clipped_border_uses_full_shape_corners() {
        let mut renderer = SoftwareRenderer::new(size(DevicePixels(20), DevicePixels(10)));
        renderer.fill_rounded_border(
            PixelRect {
                left: 0,
                top: 4,
                right: 2,
                bottom: 6,
            },
            PixelRect {
                left: 0,
                top: 0,
                right: 20,
                bottom: 10,
            },
            PixelCorners {
                top_left: 5.0,
                top_right: 5.0,
                bottom_right: 5.0,
                bottom_left: 5.0,
            },
            PixelEdges {
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
                left: 1.0,
            },
            PremulRgba::new(0.0, 1.0, 0.0, 1.0),
        );

        assert_eq!(alpha_at(&renderer, 0, 5), 255);
        assert_eq!(alpha_at(&renderer, 1, 5), 0);
    }

    #[test]
    fn linear_gradient_changes_color_across_quad() {
        let mut renderer = SoftwareRenderer::new(size(DevicePixels(4), DevicePixels(1)));
        renderer.draw_quad(&Quad {
            bounds: Bounds {
                origin: Point {
                    x: ScaledPixels(0.0),
                    y: ScaledPixels(0.0),
                },
                size: Size {
                    width: ScaledPixels(4.0),
                    height: ScaledPixels(1.0),
                },
            },
            content_mask: mask(4.0, 1.0),
            background: linear_gradient(
                90.0,
                linear_color_stop(red(), 0.0),
                linear_color_stop(blue(), 1.0),
            ),
            ..Quad::default()
        });

        assert_ne!(color_at(&renderer, 0, 0), color_at(&renderer, 3, 0));
    }

    #[test]
    fn underline_draws_solid_pixels() {
        let mut renderer = SoftwareRenderer::new(size(DevicePixels(4), DevicePixels(2)));
        renderer.draw_underline(&Underline {
            bounds: Bounds {
                origin: Point {
                    x: ScaledPixels(0.0),
                    y: ScaledPixels(0.0),
                },
                size: Size {
                    width: ScaledPixels(4.0),
                    height: ScaledPixels(1.0),
                },
            },
            content_mask: mask(4.0, 2.0),
            color: red(),
            thickness: ScaledPixels(1.0),
            wavy: 0,
            order: 0,
            pad: 0,
        });

        assert_eq!(alpha_at(&renderer, 1, 0), 255);
    }

    #[test]
    fn path_triangle_draws_pixels() {
        let mut path = Path::new(point(px(0.0), px(0.0)));
        path.content_mask = ContentMask {
            bounds: bounds(point(px(0.0), px(0.0)), size(px(5.0), px(5.0))),
        };
        path.color = solid_background(red());
        path.push_triangle(
            (
                point(px(0.0), px(0.0)),
                point(px(4.0), px(0.0)),
                point(px(0.0), px(4.0)),
            ),
            (point(0.0, 1.0), point(0.0, 1.0), point(0.0, 1.0)),
        );

        let mut renderer = SoftwareRenderer::new(size(DevicePixels(5), DevicePixels(5)));
        renderer.draw_path(&path.scale(1.0));

        assert_eq!(alpha_at(&renderer, 1, 1), 255);
    }

    #[test]
    fn shadow_draws_outside_source_bounds() {
        let mut renderer = SoftwareRenderer::new(size(DevicePixels(8), DevicePixels(8)));
        renderer.draw_shadow(&Shadow {
            bounds: Bounds {
                origin: Point {
                    x: ScaledPixels(3.0),
                    y: ScaledPixels(3.0),
                },
                size: Size {
                    width: ScaledPixels(2.0),
                    height: ScaledPixels(2.0),
                },
            },
            content_mask: mask(8.0, 8.0),
            corner_radii: Corners::default(),
            blur_radius: ScaledPixels(3.0),
            color: red(),
            order: 0,
        });

        assert!(alpha_at(&renderer, 2, 3) > 0);
    }
}
