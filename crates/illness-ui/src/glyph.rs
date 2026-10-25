//! Crisp terminal glyph atlas.
//!
//! Glyphs are rasterized with `swash` at physical pixel sizes, packed into a
//! single 2048×2048 RGBA texture with a shelf allocator, and painted as
//! pixel-snapped meshes. The atlas stores coverage for mask glyphs as
//! white + gamma-corrected alpha (`2c - c²`, tuned for light-on-dark UI) and
//! premultiplied RGBA for color glyphs.
//!
//! Windows Terminal's grayscale shader instead applies a color-dependent
//! gamma-1.8 correction per fragment (0.69 vs our 0.75 at 50% coverage), which
//! measured softer at 14 px: 305 -> 224 full-alpha pixels (-27%) and
//! 832 -> 913 partial pixels (+10%). The fixed dark-mode curve is kept so the
//! atlas stays color-agnostic.

use std::collections::HashMap;
use std::sync::Arc;

use egui::{
    Color32, ColorImage, Mesh, Pos2, Rect, TextureHandle, TextureId, TextureOptions, Vec2,
    epaint::Vertex, pos2, vec2,
};
use swash::{
    FontRef, GlyphId,
    scale::{
        Render, ScaleContext, Source, StrikeWith,
        image::{Content, Image},
    },
    zeno::{Angle, Format, Transform},
};

/// Side length of the square atlas texture in texels.
const ATLAS_SIZE: usize = 2048;
/// Transparent padding around each glyph, in texels.
const GLYPH_PADDING: usize = 1;
/// Texture name used for the atlas in egui's texture manager.
const ATLAS_NAME: &str = "illness-glyph-atlas";
/// Fallback families, in preference order, for characters missing from Cascadia.
const FALLBACK_FAMILIES: [&str; 7] = [
    "Segoe UI",
    "Segoe UI Emoji",
    "Segoe UI Symbol",
    "Microsoft YaHei UI",
    "MS Gothic",
    "Malgun Gothic",
    "Nirmala UI",
];
/// Maximum number of fallback faces kept in memory.
const MAX_FALLBACK_FACES: usize = 4;
/// Normal line height, matching the font's natural metrics.
const DEFAULT_LINE_HEIGHT: f32 = 1.2;
/// Cascadia's variable default (400) reads slightly light after the fixed-pixel
/// grid and alpha transfer; 450 restores stem density without looking semibold.
const REGULAR_WEIGHT: f32 = 450.0;
/// Weight used for bold cells.
const BOLD_WEIGHT: f32 = 700.0;
/// Glyph sources, in priority order.
const SOURCES: [Source; 3] = [
    Source::ColorOutline(0),
    Source::ColorBitmap(StrikeWith::BestFit),
    Source::Outline,
];

/// Cell geometry, in logical points and physical pixels.
///
/// `width`/`height`/`ascent`/`baseline` are logical points. `width_px` and
/// `height_px` are rounded physical pixels so callers can keep the grid aligned
/// to the pixel grid at fractional DPI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub ascent: f32,
    pub baseline: f32,
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            width: 8.4,
            height: 18.9,
            ascent: 14.7,
            baseline: 14.7,
            width_px: 8,
            height_px: 19,
        }
    }
}

/// Terminal text style. Bold and italic are synthesized unless the font
/// exposes the relevant variable axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GlyphStyle {
    #[default]
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl GlyphStyle {
    fn is_bold(self) -> bool {
        matches!(self, Self::Bold | Self::BoldItalic)
    }

    fn is_italic(self) -> bool {
        matches!(self, Self::Italic | Self::BoldItalic)
    }
}

/// A rasterized glyph's placement in the atlas and its draw geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    /// Sub-rectangle of the atlas texture containing the glyph pixels.
    pub uv: egui::Rect,
    /// Logical size of the glyph image.
    pub size: egui::Vec2,
    /// Logical offset from the cell's top-left to the image's top-left.
    pub offset: egui::Vec2,
    /// `true` when the glyph carries its own color (emoji).
    pub color: bool,
    /// `true` when the glyph's advance spans two cells (wide CJK).
    pub wide: bool,
}

/// A glyph rasterized into CPU memory for subpixel software compositing.
///
/// Mask glyphs store three bytes per pixel of linear RGB coverage, so each
/// channel can be blended independently (ClearType). Color glyphs (emoji)
/// store premultiplied RGBA and ignore the foreground color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftGlyph {
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    /// RGB coverage (3 bytes/pixel) or premultiplied RGBA (4 bytes/pixel).
    pub data: Vec<u8>,
    pub color: bool,
}

impl SoftGlyph {
    fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            left: 0,
            top: 0,
            data: Vec::new(),
            color: false,
        }
    }
}

/// Font data and face index of a system fallback face.
struct FallbackFace {
    data: Vec<u8>,
    index: usize,
}

/// Simple shelf allocator over the atlas texture.
#[derive(Debug, Default)]
struct ShelfAllocator {
    x: usize,
    y: usize,
    row_height: usize,
}

impl ShelfAllocator {
    fn reset(&mut self) {
        self.x = 0;
        self.y = 0;
        self.row_height = 0;
    }

    /// Allocates a `width × height` region, or returns `None` when full.
    fn allocate(&mut self, width: usize, height: usize) -> Option<(usize, usize)> {
        if width > ATLAS_SIZE || height > ATLAS_SIZE {
            return None;
        }

        if self.x + width > ATLAS_SIZE {
            self.y += self.row_height;
            self.x = 0;
            self.row_height = 0;
        }

        if self.y + height > ATLAS_SIZE {
            return None;
        }

        let origin = (self.x, self.y);
        self.x += width;
        self.row_height = self.row_height.max(height);
        Some(origin)
    }
}

/// A `swash`-backed glyph atlas uploaded as one egui texture.
///
/// The 2048² egui texture is created lazily on the first CPU glyph. A
/// GPU-only terminal never allocates it; the GPU pass has its own small atlas.
pub struct GlyphAtlas {
    texture: Option<TextureHandle>,
    options: TextureOptions,
    ctx: egui::Context,
    metrics: CellMetrics,
    cache: HashMap<(char, GlyphStyle), Glyph>,
    soft_cache: HashMap<(char, GlyphStyle), Arc<SoftGlyph>>,
    scale: ScaleContext,
    /// Primary font file bytes. Owned so the family can be swapped at runtime
    /// without self-referential font references.
    font_data: Arc<Vec<u8>>,
    font_index: usize,
    fallback_faces: Vec<FallbackFace>,
    font_db: Option<fontdb::Database>,
    next_fallback_family: usize,
    physical_size: f32,
    ppp: f32,
    baseline_px: f32,
    base_height_px: f32,
    line_height: f32,
    line_pad_px: f32,
    allocator: ShelfAllocator,
    #[cfg(test)]
    last_coverage: (usize, u8),
}

impl GlyphAtlas {
    /// Creates an atlas for `mono_bytes`, which must be a static font file.
    ///
    /// `size_px` is in logical points and `ppp` is the physical pixels per
    /// point. Rasterization happens at `size_px * ppp`; the reported metrics
    /// are divided back into points.
    pub fn new(ctx: &egui::Context, mono_bytes: &'static [u8], size_px: f32, ppp: f32) -> Self {
        let ppp = normalize_ppp(ppp);
        let size_px = normalize_size(size_px);
        let physical_size = size_px * ppp;
        let font_data = Arc::new(mono_bytes.to_vec());
        let font_index = 0usize;
        let metrics = metrics_for(
            FontRef::from_index(&font_data, font_index).as_ref(),
            physical_size,
            ppp,
        );

        Self {
            texture: None,
            options: TextureOptions::NEAREST,
            ctx: ctx.clone(),
            metrics,
            cache: HashMap::new(),
            soft_cache: HashMap::new(),
            scale: ScaleContext::new(),
            font_data,
            font_index,
            fallback_faces: Vec::new(),
            font_db: None,
            next_fallback_family: 0,
            physical_size,
            ppp,
            baseline_px: metrics.baseline * ppp,
            base_height_px: metrics.height_px as f32,
            line_height: DEFAULT_LINE_HEIGHT,
            line_pad_px: 0.0,
            allocator: ShelfAllocator::default(),
            #[cfg(test)]
            last_coverage: (0, 0),
        }
    }

    /// Changes the point size and device scale, dropping every cached glyph.
    pub fn set_size(&mut self, _ctx: &egui::Context, size_px: f32, ppp: f32) {
        self.ppp = normalize_ppp(ppp);
        self.physical_size = normalize_size(size_px) * self.ppp;
        self.rebuild_metrics();
    }

    /// Replaces the primary font face and rebuilds metrics and caches.
    ///
    /// `data` must contain at least the face at `index`; invalid data is ignored
    /// so a bad selection cannot break an otherwise working grid.
    pub fn set_font(&mut self, data: Vec<u8>, index: u32) {
        if FontRef::from_index(&data, index as usize).is_none() {
            log::warn!("ignoring unreadable terminal font face {index}");
            return;
        }
        self.font_data = Arc::new(data);
        self.font_index = index as usize;
        self.rebuild_metrics();
        self.ctx.request_repaint();
    }

    /// Rebuilds cell metrics for the current face, size, and line height, then
    /// clears every cached glyph and the atlas texture.
    fn rebuild_metrics(&mut self) {
        let font = FontRef::from_index(&self.font_data, self.font_index);
        self.metrics = metrics_for(font.as_ref(), self.physical_size, self.ppp);
        self.base_height_px = self.metrics.height_px as f32;
        self.baseline_px = self.metrics.baseline * self.ppp;
        self.apply_line_height();
        self.reset();
    }

    /// Changes the line height multiplier (clamped to 1.0..=2.0) and rebuilds
    /// cell metrics around the font's natural height.
    pub fn set_line_height(&mut self, line_height: f32) {
        self.line_height = if line_height.is_finite() {
            line_height.clamp(1.0, 2.0)
        } else {
            DEFAULT_LINE_HEIGHT
        };
        self.apply_line_height();
        self.reset();
    }

    fn apply_line_height(&mut self) {
        let base_height_px = self.base_height_px.max(1.0);
        let height_px = (base_height_px * self.line_height / DEFAULT_LINE_HEIGHT)
            .round()
            .max(1.0);
        let line_pad_px = (height_px - base_height_px) / 2.0;
        self.line_pad_px = line_pad_px;
        self.metrics.height_px = height_px as u32;
        self.metrics.height = height_px / self.ppp;
        self.metrics.baseline = (self.baseline_px + line_pad_px) / self.ppp;
    }

    /// Cell geometry derived from Cascadia Mono's `M` advance and line metrics.
    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Rasterizes and caches a glyph, returning `None` only if the atlas cannot
    /// make room even after a reset.
    pub fn glyph(&mut self, ch: char, style: GlyphStyle) -> Option<Glyph> {
        let key = (ch, style);
        if let Some(glyph) = self.cache.get(&key) {
            return Some(*glyph);
        }

        let mut glyph_id = {
            let primary = FontRef::from_index(&self.font_data, self.font_index)?;
            primary.charmap().map(ch)
        };
        let mut fallback_index = None;

        if glyph_id == 0
            && let Some(index) = self.find_fallback(ch)
            && let Some(font) = self
                .fallback_faces
                .get(index)
                .and_then(|face| FontRef::from_index(&face.data, face.index))
        {
            let id = font.charmap().map(ch);
            if id != 0 {
                glyph_id = id;
                fallback_index = Some(index);
            }
        }

        let physical_size = self.physical_size;
        let ppp = self.ppp;

        // Keep the borrow of the primary/fallback font data inside this block
        // so the rest of the method can mutate the atlas.
        let (image, advance_px) = {
            let primary = FontRef::from_index(&self.font_data, self.font_index);
            let source_font = fallback_index
                .and_then(|index| self.fallback_faces.get(index))
                .and_then(|face| FontRef::from_index(&face.data, face.index))
                .or(primary)?;

            let advance_px = source_font
                .glyph_metrics(&[])
                .scale(physical_size)
                .advance_width(glyph_id);
            let image = rasterize(
                &mut self.scale,
                source_font,
                glyph_id,
                style,
                physical_size,
                ppp,
            );
            (image, advance_px)
        };

        let wide = advance_px > 1.5 * self.metrics.width_px as f32;
        let empty = Glyph {
            uv: Rect::from_min_size(Pos2::ZERO, Vec2::ZERO),
            size: Vec2::ZERO,
            offset: Vec2::ZERO,
            color: false,
            wide,
        };

        let Some(image) = image else {
            self.cache.insert(key, empty);
            return Some(empty);
        };

        let width = image.placement.width as usize;
        let height = image.placement.height as usize;
        if width == 0 || height == 0 {
            self.cache.insert(key, empty);
            return Some(empty);
        }

        let tile_width = width + 2 * GLYPH_PADDING;
        let tile_height = height + 2 * GLYPH_PADDING;
        let origin = if let Some(origin) = self.allocator.allocate(tile_width, tile_height) {
            origin
        } else {
            self.reset();
            self.allocator.allocate(tile_width, tile_height)?
        };

        let (tile, _coverage) = build_tile(&image, tile_width, tile_height);
        let options = self.options;
        let texture = self.ensure_texture();
        texture.set_partial(
            [origin.0, origin.1],
            ColorImage::new([tile_width, tile_height], tile),
            options,
        );

        let atlas_size = ATLAS_SIZE as f32;
        let atlas_x = (origin.0 + GLYPH_PADDING) as f32;
        let atlas_y = (origin.1 + GLYPH_PADDING) as f32;
        let uv = Rect::from_min_max(
            pos2(atlas_x / atlas_size, atlas_y / atlas_size),
            pos2(
                (atlas_x + width as f32) / atlas_size,
                (atlas_y + height as f32) / atlas_size,
            ),
        );
        let offset = vec2(
            image.placement.left as f32 / ppp,
            (self.baseline_px - image.placement.top as f32) / ppp,
        );
        let glyph = Glyph {
            uv,
            size: vec2(width as f32 / ppp, height as f32 / ppp),
            offset,
            color: image.content == Content::Color,
            wide,
        };

        #[cfg(test)]
        {
            self.last_coverage = _coverage;
        }

        self.cache.insert(key, glyph);
        Some(glyph)
    }

    /// Rasterizes and caches a glyph for software subpixel compositing.
    ///
    /// Mask glyphs use swash's `Format::Subpixel` and therefore come back as
    /// `Content::SubpixelMask`. swash/zeno store that mask as four bytes per
    /// pixel (RGBX, alpha unused), so it is compacted to the three bytes per
    /// pixel exposed by [`SoftGlyph`].
    pub fn soft_glyph(&mut self, ch: char, style: GlyphStyle) -> Option<Arc<SoftGlyph>> {
        let key = (ch, style);
        if let Some(glyph) = self.soft_cache.get(&key) {
            return Some(Arc::clone(glyph));
        }

        let mut glyph_id = {
            let primary = FontRef::from_index(&self.font_data, self.font_index)?;
            primary.charmap().map(ch)
        };
        let mut fallback_index = None;

        if glyph_id == 0
            && let Some(index) = self.find_fallback(ch)
            && let Some(font) = self
                .fallback_faces
                .get(index)
                .and_then(|face| FontRef::from_index(&face.data, face.index))
        {
            let id = font.charmap().map(ch);
            if id != 0 {
                glyph_id = id;
                fallback_index = Some(index);
            }
        }

        let physical_size = self.physical_size;
        let ppp = self.ppp;
        let image = {
            let primary = FontRef::from_index(&self.font_data, self.font_index);
            let source_font = fallback_index
                .and_then(|index| self.fallback_faces.get(index))
                .and_then(|face| FontRef::from_index(&face.data, face.index))
                .or(primary)?;
            rasterize_subpixel(
                &mut self.scale,
                source_font,
                glyph_id,
                style,
                physical_size,
                ppp,
            )
        };

        let glyph = image
            .as_ref()
            .filter(|image| image.placement.width > 0 && image.placement.height > 0)
            .map(soft_from_image)
            .unwrap_or_else(SoftGlyph::empty);
        let glyph = Arc::new(glyph);
        self.soft_cache.insert(key, Arc::clone(&glyph));
        Some(glyph)
    }

    /// The atlas texture to sample from in custom meshes, once CPU glyphs
    /// have been requested. GPU-only text never creates it.
    pub fn texture(&self) -> Option<TextureId> {
        self.texture.as_ref().map(TextureHandle::id)
    }

    /// Creates the egui atlas on first CPU glyph use.
    fn ensure_texture(&mut self) -> &mut TextureHandle {
        self.texture.get_or_insert_with(|| {
            self.ctx.load_texture(
                ATLAS_NAME,
                ColorImage::filled([ATLAS_SIZE, ATLAS_SIZE], Color32::TRANSPARENT),
                self.options,
            )
        })
    }

    /// Sampling options used for the atlas (always nearest for crisp text).
    pub fn texture_options(&self) -> TextureOptions {
        self.options
    }

    /// Paints a glyph as a single pixel-snapped textured quad.
    pub fn paint_glyph(
        &self,
        painter: &egui::Painter,
        pos: egui::Pos2,
        glyph: &Glyph,
        color: egui::Color32,
    ) {
        if glyph.size.x <= 0.0 || glyph.size.y <= 0.0 {
            return;
        }
        let Some(texture_id) = self.texture.as_ref().map(TextureHandle::id) else {
            return;
        };

        let ppp = painter.ctx().pixels_per_point().max(f32::EPSILON);
        let snap = |value: f32| (value * ppp).round() / ppp;
        let pos = pos2(pos.x, pos.y + self.line_pad_px / self.ppp.max(f32::EPSILON));
        let min = pos + glyph.offset;
        let max = min + glyph.size;
        let rect = Rect::from_min_max(
            pos2(snap(min.x), snap(min.y)),
            pos2(snap(max.x), snap(max.y)),
        );
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }

        // Nearest sampling needs no inset: a pixel-snapped quad whose width is
        // the glyph's pixel width puts every fragment center at a texel center,
        // u(x) = (atlas_x + x + 0.5) / ATLAS_SIZE. Insetting here would only
        // shift the mapping for a stretched quad (e.g. a DPI mismatch) and can
        // drop the edge texel. The 1px atlas padding makes bleeding impossible
        // without any inset.
        let uv = glyph.uv;
        let vertex_color = if glyph.color { Color32::WHITE } else { color };

        let mut mesh = Mesh::with_texture(texture_id);
        mesh.vertices.push(Vertex {
            pos: rect.left_top(),
            uv: uv.left_top(),
            color: vertex_color,
        });
        mesh.vertices.push(Vertex {
            pos: rect.right_top(),
            uv: uv.right_top(),
            color: vertex_color,
        });
        mesh.vertices.push(Vertex {
            pos: rect.right_bottom(),
            uv: uv.right_bottom(),
            color: vertex_color,
        });
        mesh.vertices.push(Vertex {
            pos: rect.left_bottom(),
            uv: uv.left_bottom(),
            color: vertex_color,
        });
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
    }

    /// Drops cached glyphs and makes the atlas fully transparent again.
    pub fn reset(&mut self) {
        self.cache.clear();
        self.soft_cache.clear();
        self.allocator.reset();
        if let Some(texture) = self.texture.as_mut() {
            texture.set(
                ColorImage::filled([ATLAS_SIZE, ATLAS_SIZE], Color32::TRANSPARENT),
                self.options,
            );
        }
        #[cfg(test)]
        {
            self.last_coverage = (0, 0);
        }
    }

    /// Finds (loading if needed) a fallback face that contains `ch`.
    fn find_fallback(&mut self, ch: char) -> Option<usize> {
        for (index, face) in self.fallback_faces.iter().enumerate() {
            if let Some(font) = FontRef::from_index(&face.data, face.index)
                && font.charmap().map(ch) != 0
            {
                return Some(index);
            }
        }

        while self.fallback_faces.len() < MAX_FALLBACK_FACES
            && self.next_fallback_family < FALLBACK_FAMILIES.len()
        {
            let family = FALLBACK_FAMILIES[self.next_fallback_family];
            self.next_fallback_family += 1;
            let Some(face) = self.load_family(family) else {
                continue;
            };
            let has_char = FontRef::from_index(&face.data, face.index)
                .is_some_and(|font| font.charmap().map(ch) != 0);
            let index = self.fallback_faces.len();
            self.fallback_faces.push(face);
            if has_char {
                return Some(index);
            }
        }

        None
    }

    /// Loads one face by family name and copies its bytes for caching.
    fn load_family(&mut self, family: &str) -> Option<FallbackFace> {
        if self.font_db.is_none() {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            self.font_db = Some(db);
        }
        let db = self.font_db.as_ref()?;
        let families = [fontdb::Family::Name(family)];
        let query = fontdb::Query {
            families: &families,
            ..fontdb::Query::default()
        };
        let id = db.query(&query)?;
        db.with_face_data(id, |data, index| FallbackFace {
            data: data.to_vec(),
            index: index as usize,
        })
    }
}

/// Rasterization settings that tests can vary without touching production
/// call sites.
#[derive(Clone, Copy, Debug)]
struct RasterOptions {
    hint: bool,
    weight: f32,
}

/// Rasterizes one glyph at `physical_size`, applying bold/italic synthesis.
fn rasterize(
    scale: &mut ScaleContext,
    font: FontRef<'_>,
    glyph_id: GlyphId,
    style: GlyphStyle,
    physical_size: f32,
    ppp: f32,
) -> Option<Image> {
    let weight = if style.is_bold() {
        BOLD_WEIGHT
    } else {
        REGULAR_WEIGHT
    };
    rasterize_with(
        scale,
        font,
        glyph_id,
        style,
        physical_size,
        ppp,
        RasterOptions { hint: true, weight },
    )
}

fn rasterize_with(
    scale: &mut ScaleContext,
    font: FontRef<'_>,
    glyph_id: GlyphId,
    style: GlyphStyle,
    physical_size: f32,
    ppp: f32,
    options: RasterOptions,
) -> Option<Image> {
    rasterize_format(
        scale,
        font,
        glyph_id,
        style,
        physical_size,
        ppp,
        options,
        Format::Alpha,
    )
}

/// Rasterizes one glyph as three-channel subpixel coverage.
fn rasterize_subpixel(
    scale: &mut ScaleContext,
    font: FontRef<'_>,
    glyph_id: GlyphId,
    style: GlyphStyle,
    physical_size: f32,
    ppp: f32,
) -> Option<Image> {
    let weight = if style.is_bold() {
        BOLD_WEIGHT
    } else {
        REGULAR_WEIGHT
    };
    rasterize_format(
        scale,
        font,
        glyph_id,
        style,
        physical_size,
        ppp,
        RasterOptions { hint: true, weight },
        Format::Subpixel,
    )
}

#[allow(clippy::too_many_arguments)]
fn rasterize_format(
    scale: &mut ScaleContext,
    font: FontRef<'_>,
    glyph_id: GlyphId,
    style: GlyphStyle,
    physical_size: f32,
    ppp: f32,
    options: RasterOptions,
    format: Format,
) -> Option<Image> {
    let weight_axis = font.attributes().has_weight_variation();
    let mut builder = scale.builder(font).size(physical_size).hint(options.hint);
    if weight_axis {
        builder = builder.variations(&[("wght", options.weight)]);
    }
    let mut scaler = builder.build();

    let mut render = Render::new(&SOURCES);
    render.format(format);
    if style.is_italic() {
        render.transform(Some(Transform::skew(Angle::from_radians(0.2), Angle::ZERO)));
    }
    if style.is_bold() && !weight_axis {
        render.embolden(0.4 * ppp);
    }

    render.render(&mut scaler, glyph_id)
}

/// Converts one swash image into a padded, premultiplied RGBA tile.
///
/// Returns the tile pixels and the number of non-transparent, non-padding
/// pixels with the brightest alpha, which is handy for tests.
fn build_tile(image: &Image, tile_width: usize, tile_height: usize) -> (Vec<Color32>, (usize, u8)) {
    let mut pixels = vec![Color32::TRANSPARENT; tile_width * tile_height];
    let width = image.placement.width as usize;
    let height = image.placement.height as usize;
    let mut coverage = 0;
    let mut max_alpha = 0;

    if image.content == Content::Color {
        // COLR outlines are composited as premultiplied RGBA by swash, while
        // embedded PNG color bitmaps decode to straight RGBA.
        let straight_alpha = matches!(image.source, Source::ColorBitmap(_));
        for (index, source) in image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .take(width * height)
            .enumerate()
        {
            let alpha = source[3];
            if alpha == 0 {
                continue;
            }
            let (red, green, blue) = if straight_alpha {
                let src_alpha = u32::from(alpha);
                (
                    (u32::from(source[0]) * src_alpha / 255) as u8,
                    (u32::from(source[1]) * src_alpha / 255) as u8,
                    (u32::from(source[2]) * src_alpha / 255) as u8,
                )
            } else {
                (source[0], source[1], source[2])
            };
            let x = index % width;
            let y = index / width;
            pixels[(y + GLYPH_PADDING) * tile_width + x + GLYPH_PADDING] =
                Color32::from_rgba_premultiplied(red, green, blue, alpha);
            coverage += 1;
            max_alpha = max_alpha.max(alpha);
        }
    } else {
        for (index, &alpha) in image.data.iter().take(width * height).enumerate() {
            if alpha == 0 {
                continue;
            }
            let corrected = gamma_correct(alpha);
            let x = index % width;
            let y = index / width;
            pixels[(y + GLYPH_PADDING) * tile_width + x + GLYPH_PADDING] =
                Color32::from_rgba_premultiplied(corrected, corrected, corrected, corrected);
            coverage += 1;
            max_alpha = max_alpha.max(corrected);
        }
    }

    (pixels, (coverage, max_alpha))
}

/// Converts a swash image into the CPU glyph representation.
fn soft_from_image(image: &Image) -> SoftGlyph {
    let width = image.placement.width;
    let height = image.placement.height;
    let count = width as usize * height as usize;
    let color = image.content == Content::Color;
    let mut data = Vec::new();

    if color {
        data.reserve(count * 4);
        let straight_alpha = matches!(image.source, Source::ColorBitmap(_));
        let mut source = image.data.as_chunks::<4>().0.iter();
        for _ in 0..count {
            let mut pixel = [0u8; 4];
            if let Some(chunk) = source.next() {
                pixel.copy_from_slice(chunk);
            }
            if straight_alpha {
                data.push(premultiply(pixel[0], pixel[3]));
                data.push(premultiply(pixel[1], pixel[3]));
                data.push(premultiply(pixel[2], pixel[3]));
                data.push(pixel[3]);
            } else {
                data.extend_from_slice(&pixel);
            }
        }
    } else if image.data.len() >= count * 4 {
        // swash's SubpixelMask is RGBX: four bytes per pixel, alpha unused.
        data.reserve(count * 3);
        for pixel in image.data.as_chunks::<4>().0.iter().take(count) {
            data.extend_from_slice(&pixel[..3]);
        }
    } else if image.data.len() >= count * 3 {
        data.extend_from_slice(&image.data[..count * 3]);
    } else {
        // Grayscale fallback: replicate one coverage byte across all channels.
        data.reserve(count * 3);
        for index in 0..count {
            let coverage = image.data.get(index).copied().unwrap_or(0);
            data.extend_from_slice(&[coverage, coverage, coverage]);
        }
    }

    SoftGlyph {
        width,
        height,
        left: image.placement.left,
        top: image.placement.top,
        data,
        color,
    }
}

fn premultiply(value: u8, alpha: u8) -> u8 {
    ((u16::from(value) * u16::from(alpha) + 127) / 255) as u8
}

/// Gamma-corrects dark-mode coverage: `2c - c²` in 0..=255 integer space.
///
/// This is the same transfer epaint uses for dark-mode UI text and is sharper
/// than DirectWrite's grayscale correction at the same coverage (see module
/// docs). WT applies its correction per foreground color in the shader; a
/// baked atlas can only approximate that, and the approximation here is the
/// crisper one.
fn gamma_correct(alpha: u8) -> u8 {
    let value = u32::from(alpha);
    let corrected = 2 * value - (value * value) / 255;
    corrected.min(255) as u8
}

/// Builds cell metrics from the font at a physical pixel size.
fn metrics_for(font: Option<&FontRef<'_>>, physical_size: f32, ppp: f32) -> CellMetrics {
    let Some(font) = font else {
        return CellMetrics::default();
    };

    let metrics = font.metrics(&[]).scale(physical_size);
    let ascent = metrics.ascent.max(0.0);
    let descent = metrics.descent.max(0.0);
    let leading = metrics.leading.max(0.0);
    let height_px = (ascent + descent + leading).round().max(1.0) as u32;
    let baseline_px = ascent.round();

    let glyph_metrics = font.glyph_metrics(&[]).scale(physical_size);
    let advance = {
        let m = font.charmap().map('M');
        if m != 0 {
            glyph_metrics.advance_width(m)
        } else {
            let space = font.charmap().map(' ');
            if space != 0 {
                glyph_metrics.advance_width(space)
            } else {
                physical_size * 0.6
            }
        }
    };
    let width_px = advance.round().max(1.0) as u32;

    CellMetrics {
        width: width_px as f32 / ppp,
        height: height_px as f32 / ppp,
        ascent: baseline_px / ppp,
        baseline: baseline_px / ppp,
        width_px,
        height_px,
    }
}

fn normalize_ppp(ppp: f32) -> f32 {
    if ppp.is_finite() && ppp > 0.0 {
        ppp.clamp(0.5, 8.0)
    } else {
        1.0
    }
}

fn normalize_size(size_px: f32) -> f32 {
    if size_px.is_finite() {
        size_px.clamp(1.0, 400.0)
    } else {
        14.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::CASCADIA_MONO;

    fn atlas(size_px: f32, ppp: f32) -> (egui::Context, GlyphAtlas) {
        let ctx = egui::Context::default();
        let atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, size_px, ppp);
        (ctx, atlas)
    }

    #[test]
    fn test_alpha_nonzero() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let glyph = atlas.glyph('M', GlyphStyle::Regular).expect("M glyph");
        assert!(glyph.size.x > 0.0);
        assert!(glyph.size.y > 0.0);
        let (covered, max_alpha) = atlas.last_coverage;
        assert!(covered >= 20, "'M' covered only {covered} pixels");
        assert!(max_alpha > 128, "'M' peak alpha was only {max_alpha}");
    }

    #[test]
    fn test_metrics_positive() {
        let (_ctx, atlas) = atlas(14.0, 1.5);
        let metrics = atlas.metrics();
        assert!(metrics.width > 0.0);
        assert!(metrics.height > 0.0);
        assert!(metrics.ascent > 0.0);
        assert!(metrics.baseline > 0.0);
        assert!(metrics.width_px >= 1);
        assert!(metrics.height_px >= 1);
        assert!((metrics.width * 1.5 - metrics.width_px as f32).abs() < 0.001);
        assert!((metrics.height * 1.5 - metrics.height_px as f32).abs() < 0.001);
    }

    #[test]
    fn test_glyph_cache() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let first = atlas.glyph('M', GlyphStyle::Regular).expect("M glyph");
        let second = atlas.glyph('M', GlyphStyle::Regular).expect("M glyph");
        assert_eq!(first, second);
        assert_eq!(first.uv, second.uv);
    }

    #[test]
    fn test_set_font_bundled_renders_m() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        atlas.set_font(CASCADIA_MONO.to_vec(), 0);

        let glyph = atlas.glyph('M', GlyphStyle::Regular).expect("M glyph");
        assert!(glyph.size.x > 0.0, "set_font produced an empty 'M'");
        assert!(glyph.size.y > 0.0, "set_font produced an empty 'M'");
        let (covered, max_alpha) = atlas.last_coverage;
        assert!(
            covered >= 20,
            "'M' covered only {covered} pixels after set_font"
        );
        assert!(
            max_alpha > 128,
            "'M' peak alpha was only {max_alpha} after set_font"
        );
    }

    #[test]
    fn test_set_font_rejects_invalid_data() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let before = atlas.metrics();
        atlas.set_font(vec![0, 1, 2, 3], 0);
        assert_eq!(atlas.metrics(), before, "invalid font changed metrics");
        let glyph = atlas.glyph('M', GlyphStyle::Regular).expect("M glyph");
        assert!(glyph.size.x > 0.0);
    }

    #[test]
    fn test_soft_glyph_subpixel_fringes() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let glyph = atlas
            .soft_glyph('H', GlyphStyle::Regular)
            .expect("H soft glyph");
        assert!(!glyph.color, "outline glyph should be a mask");
        assert!(glyph.width > 0 && glyph.height > 0);
        assert_eq!(
            glyph.data.len(),
            glyph.width as usize * glyph.height as usize * 3
        );
        assert!(glyph.data.iter().any(|&value| value > 0));
        assert!(
            glyph
                .data
                .as_chunks::<3>()
                .0
                .iter()
                .any(|pixel| pixel[0] != pixel[1] || pixel[1] != pixel[2]),
            "expected per-channel ClearType fringes"
        );
    }

    #[test]
    fn test_soft_glyph_cache_and_reset() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let first = atlas
            .soft_glyph('H', GlyphStyle::Regular)
            .expect("H soft glyph");
        let second = atlas
            .soft_glyph('H', GlyphStyle::Regular)
            .expect("H soft glyph");
        assert!(Arc::ptr_eq(&first, &second));

        atlas.reset();
        let third = atlas
            .soft_glyph('H', GlyphStyle::Regular)
            .expect("H soft glyph");
        assert!(!Arc::ptr_eq(&first, &third));
    }

    #[test]
    fn test_soft_color_emoji_rgba() {
        if !emoji_font_present() {
            eprintln!("skipping soft color-glyph assertion: no emoji system font");
            return;
        }
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let glyph = atlas
            .soft_glyph('\u{1f600}', GlyphStyle::Regular)
            .expect("emoji");
        assert!(glyph.color);
        assert_eq!(
            glyph.data.len(),
            glyph.width as usize * glyph.height as usize * 4
        );
        assert!(glyph.data.iter().any(|&value| value > 0));
    }

    #[test]
    fn test_baseline_alignment() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let baseline = atlas.metrics().baseline;
        let glyph = atlas.glyph('M', GlyphStyle::Regular).expect("M glyph");
        let bottom = glyph.offset.y + glyph.size.y;
        assert!(glyph.offset.y >= 0.0, "'M' starts above its cell");
        assert!(
            (bottom - baseline).abs() <= 1.0,
            "'M' bottom {bottom} should sit on baseline {baseline}"
        );
    }

    #[test]
    fn line_height_expands_cell_and_pads_glyphs() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let base = atlas.metrics();
        atlas.set_line_height(1.6);
        let tall = atlas.metrics();
        assert!(tall.height > base.height);
        assert!(tall.height_px > base.height_px);
        assert!(atlas.line_pad_px > 0.0);
        assert!(tall.baseline > base.baseline);

        atlas.set_line_height(1.2);
        assert!((atlas.metrics().height - base.height).abs() < 0.001);
        assert!(atlas.line_pad_px.abs() < 0.001);
    }

    #[test]
    fn test_color_emoji() {
        if !emoji_font_present() {
            eprintln!("skipping color-glyph assertion: no emoji system font");
            return;
        }
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let glyph = atlas
            .glyph('\u{1f600}', GlyphStyle::Regular)
            .expect("emoji");
        assert!(glyph.size.x > 0.0);
        assert!(glyph.color, "emoji should keep its color glyph");
        assert!(
            atlas.last_coverage.0 > 0,
            "emoji should write non-transparent pixels"
        );
    }

    #[test]
    fn test_wide() {
        if !cjk_font_present() {
            eprintln!("skipping wide-glyph assertion: no CJK system font");
            return;
        }
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let glyph = atlas
            .glyph('\u{4e2d}', GlyphStyle::Regular)
            .expect("CJK glyph");
        assert!(glyph.size.x > 0.0);
        assert!(glyph.wide, "CJK glyph should span two cells: {glyph:?}");
    }

    #[test]
    fn test_atlas_overflow() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        for index in 0..3000u32 {
            if let Some(ch) = char::from_u32(0x20 + index) {
                let _ = atlas.glyph(ch, GlyphStyle::Regular);
            }
        }

        // Simulate a completely full atlas: the next glyph must trigger the
        // overflow reset path and still be inserted afterwards.
        atlas.allocator.y = ATLAS_SIZE;
        let replacement = atlas
            .glyph('\u{fffd}', GlyphStyle::Regular)
            .expect("glyph after overflow reset");
        assert!(replacement.size.x > 0.0);

        atlas.reset();
        assert!(atlas.glyph('\u{fffd}', GlyphStyle::Regular).is_some());
    }

    #[test]
    fn test_nearest() {
        let (_ctx, atlas) = atlas(14.0, 1.0);
        assert_eq!(atlas.texture_options(), TextureOptions::NEAREST);
    }

    /// GPU-only text must not allocate the 16 MB egui atlas.
    #[test]
    fn egui_atlas_is_lazy() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        assert!(atlas.texture().is_none());

        let glyph = atlas
            .soft_glyph('H', GlyphStyle::Regular)
            .expect("soft glyph");
        assert!(glyph.width > 0);
        assert!(
            atlas.texture().is_none(),
            "the software glyph cache alone must not create the egui atlas"
        );

        let glyph = atlas.glyph('H', GlyphStyle::Regular).expect("mesh glyph");
        assert!(glyph.size.x > 0.0);
        assert!(atlas.texture().is_some(), "mesh glyphs need the egui atlas");
    }

    #[test]
    fn test_glyph_uv_is_uninset() {
        for ppp in [1.0, 1.25, 1.5] {
            let (_ctx, mut atlas) = atlas(14.0, ppp);
            let glyph = atlas.glyph('H', GlyphStyle::Regular).expect("H glyph");
            let uv_px = glyph.uv.width() * ATLAS_SIZE as f32;
            let pixel_width = glyph.size.x * ppp;
            assert!(
                (uv_px - pixel_width).abs() < 0.001,
                "uv spans {uv_px}px, glyph is {pixel_width}px at ppp {ppp}"
            );
            for edge in [
                glyph.uv.min.x,
                glyph.uv.max.x,
                glyph.uv.min.y,
                glyph.uv.max.y,
            ] {
                let texel = edge * ATLAS_SIZE as f32;
                assert!(
                    (texel - texel.round()).abs() < 0.001,
                    "uv edge {texel} is not on a texel boundary"
                );
            }
        }
    }

    /// With nearest sampling, a snapped quad of `w` physical pixels maps
    /// fragment `i` to texel `atlas_x + i` when the full glyph UV rect is used.
    #[test]
    fn test_nearest_full_uv_maps_each_fragment_to_its_texel() {
        let size = ATLAS_SIZE as f32;
        for width in 1..=32 {
            let atlas_x = 37usize;
            let quad_x = 11.0f32; // already pixel-snapped
            let (u0, u1) = (atlas_x as f32 / size, (atlas_x + width) as f32 / size);
            for i in 0..width {
                let fragment_center = quad_x + i as f32 + 0.5;
                let t = (fragment_center - quad_x) / width as f32;
                let u = u0 + t * (u1 - u0);
                assert!((u * size - ((atlas_x + i) as f32 + 0.5)).abs() < 1e-3);
                assert_eq!((u * size).floor() as usize, atlas_x + i);
            }
        }
    }

    #[test]
    fn test_hinting_is_applied() {
        let font = FontRef::from_index(CASCADIA_MONO, 0).expect("Cascadia Mono");
        let mut scale = ScaleContext::new();
        let glyph_id = font.charmap().map('H');
        let options = |hint| RasterOptions {
            hint,
            weight: 400.0,
        };
        let hinted = rasterize_with(
            &mut scale,
            font,
            glyph_id,
            GlyphStyle::Regular,
            14.0,
            1.0,
            options(true),
        )
        .expect("hinted H");
        let unhinted = rasterize_with(
            &mut scale,
            font,
            glyph_id,
            GlyphStyle::Regular,
            14.0,
            1.0,
            options(false),
        )
        .expect("unhinted H");

        let partial = |image: &Image| image.data.iter().filter(|&&a| a > 0 && a < 255).count();
        let solid = |image: &Image| image.data.iter().filter(|&&a| a == 255).count();
        assert_ne!(hinted.data, unhinted.data, "hinting changed no pixels");
        assert!(
            partial(&hinted) < partial(&unhinted),
            "hinted {} vs unhinted {} partial pixels",
            partial(&hinted),
            partial(&unhinted)
        );
        assert!(solid(&hinted) > solid(&unhinted));
    }

    #[test]
    fn test_regular_weight_450_adds_core() {
        let font = FontRef::from_index(CASCADIA_MONO, 0).expect("Cascadia Mono");
        let mut scale = ScaleContext::new();
        let glyph_id = font.charmap().map('H');
        let mut render = |weight| {
            rasterize_with(
                &mut scale,
                font,
                glyph_id,
                GlyphStyle::Regular,
                14.0,
                1.0,
                RasterOptions { hint: true, weight },
            )
            .expect("H glyph")
        };
        let light = render(400.0);
        let regular = render(450.0);
        let ink = |image: &Image| image.data.iter().map(|&a| u64::from(a)).sum::<u64>();
        assert!(ink(&regular) > ink(&light));
        assert_eq!(regular.placement.width, light.placement.width);
    }

    /// WT's grayscale alpha correction (gamma 1.8, white foreground) is softer
    /// than epaint's dark-mode transfer, so it was checked and left out.
    #[test]
    fn test_wt_grayscale_transfer_is_softer() {
        fn wt(alpha: u8) -> f32 {
            let c = f32::from(alpha) / 255.0;
            let intensity = 1.0;
            c + c
                * (1.0 - c)
                * ((0.148_054_42 * intensity - 0.894_594_55) * c
                    + (1.475_908 * intensity - 0.324_668_26))
        }
        assert!((wt(128) * 255.0 - 177.0).abs() < 1.0);
        assert!((f32::from(gamma_correct(128)) - 192.0).abs() < 1.0);
        assert!(wt(128) < f32::from(gamma_correct(128)) / 255.0);

        let font = FontRef::from_index(CASCADIA_MONO, 0).expect("Cascadia Mono");
        let mut scale = ScaleContext::new();
        let image = rasterize_with(
            &mut scale,
            font,
            font.charmap().map('H'),
            GlyphStyle::Regular,
            14.0,
            1.0,
            RasterOptions {
                hint: true,
                weight: 450.0,
            },
        )
        .expect("H glyph");
        let wt_u8 = |a| (wt(a) * 255.0).round().clamp(0.0, 255.0) as u8;
        let ink =
            |map: &dyn Fn(u8) -> u8| image.data.iter().map(|&a| u64::from(map(a))).sum::<u64>();
        assert!(
            ink(&wt_u8) < ink(&gamma_correct),
            "WT ink {}, epaint ink {}",
            ink(&wt_u8),
            ink(&gamma_correct)
        );
    }

    #[test]
    fn test_missing_glyph_no_panic() {
        let (_ctx, mut atlas) = atlas(14.0, 1.0);
        let _ = atlas.glyph('\u{10ffff}', GlyphStyle::BoldItalic);
        let _ = atlas.glyph('\u{0}', GlyphStyle::Italic);
    }

    /// True when a system font family in our fallback list covers CJK.
    fn cjk_font_present() -> bool {
        const CJK_FAMILIES: [&str; 3] = ["Microsoft YaHei UI", "MS Gothic", "Malgun Gothic"];
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        CJK_FAMILIES.iter().any(|family| {
            let families = [fontdb::Family::Name(family)];
            let query = fontdb::Query {
                families: &families,
                ..fontdb::Query::default()
            };
            db.query(&query).is_some()
        })
    }

    /// True when Windows' color emoji font is installed.
    fn emoji_font_present() -> bool {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let families = [fontdb::Family::Name("Segoe UI Emoji")];
        let query = fontdb::Query {
            families: &families,
            ..fontdb::Query::default()
        };
        db.query(&query).is_some()
    }
}
