//! Crisp terminal glyph atlas.
//!
//! Glyphs are rasterized with `swash` at physical pixel sizes, packed into a
//! single 2048×2048 RGBA texture with a shelf allocator, and painted as
//! pixel-snapped meshes. The atlas stores coverage for mask glyphs as
//! white + gamma-corrected alpha (`2c - c²`, tuned for light-on-dark UI) and
//! premultiplied RGBA for color glyphs.
//!
//! Terminal grid glyphs (the [`SoftGlyph`] path used by both the software
//! compositor and the GPU mask pipeline) are deliberately *grayscale*. swash's
//! `Format::Subpixel` mask is a raw three-sample RGB mask with no LCD low-pass
//! filter, and its hinting snaps stems to the 1/3-pixel LCD grid. At terminal
//! sizes that leaves ±60% per-channel coverage on every stem edge, which reads
//! as rainbow fringing on light-on-dark text. Windows Terminal's default
//! grayscale mode has none of that error, so the grid uses one coverage byte
//! per pixel and ordinary alpha blending. Color glyphs (emoji) keep their own
//! colors.
//!
//! Windows Terminal's grayscale shader applies a color-dependent gamma-1.8
//! correction per fragment (0.69 vs our 0.75 at 50% coverage), which measured
//! softer at 14 px: 305 -> 224 full-alpha pixels (-27%) and 832 -> 913 partial
//! pixels (+10%). The fixed dark-mode curve is kept for the egui atlas so it
//! stays color-agnostic. The terminal mask path stores raw coverage.

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
/// Weight used for bold cells when the face has a `wght` axis. Families that
/// install real bold faces use those faces instead.
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

/// A glyph rasterized into CPU memory for software compositing.
///
/// Mask glyphs store one byte of grayscale coverage per pixel, so every color
/// channel blends by the same amount (no chroma fringing). Color glyphs (emoji)
/// store premultiplied RGBA and ignore the foreground color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftGlyph {
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    /// Grayscale coverage (1 byte/pixel) or premultiplied RGBA (4 bytes/pixel).
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

/// Font data and face index for one primary font style.
///
/// The `Arc` lets regular, bold, italic, and bold-italic share one font file
/// when a family is a single variable font.
#[derive(Clone)]
pub struct FontFace {
    pub data: Arc<Vec<u8>>,
    pub index: u32,
}

impl FontFace {
    pub fn new(data: Vec<u8>, index: u32) -> Self {
        Self {
            data: Arc::new(data),
            index,
        }
    }

    fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index as usize)
    }
}

/// Font data and face index of a system fallback face.
struct FallbackFace {
    data: Vec<u8>,
    index: usize,
}

/// Which primary face serves a glyph request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaceSlot {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Fallback(usize),
}

/// Styles that must be synthesized because the selected face is not the real
/// styled face.
#[derive(Debug, Clone, Copy, Default)]
struct Synthetic {
    bold: bool,
    italic: bool,
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
    /// Primary regular font face. Cell metrics always come from this face.
    regular: FontFace,
    /// Real bold face, when the family installs one.
    bold: Option<FontFace>,
    /// Real italic face, when the family installs one.
    italic: Option<FontFace>,
    /// Real bold-italic face, when the family installs one.
    bold_italic: Option<FontFace>,
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
        let regular = FontFace::new(mono_bytes.to_vec(), 0);
        let metrics = metrics_for(regular.font().as_ref(), physical_size, ppp);

        Self {
            texture: None,
            options: TextureOptions::NEAREST,
            ctx: ctx.clone(),
            metrics,
            cache: HashMap::new(),
            soft_cache: HashMap::new(),
            scale: ScaleContext::new(),
            regular,
            bold: None,
            italic: None,
            bold_italic: None,
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
    /// so a bad selection cannot break an otherwise working grid. Styled faces
    /// are cleared, so bold and italic fall back to synthesis.
    pub fn set_font(&mut self, data: Vec<u8>, index: u32) {
        self.set_font_faces(FontFace::new(data, index), None, None, None);
    }

    /// Replaces all primary faces and rebuilds metrics and caches.
    ///
    /// The regular face is required. Missing or unreadable styled faces are
    /// ignored; the matching style is then synthesized from the closest face.
    pub fn set_font_faces(
        &mut self,
        regular: FontFace,
        bold: Option<FontFace>,
        italic: Option<FontFace>,
        bold_italic: Option<FontFace>,
    ) {
        if regular.font().is_none() {
            log::warn!("ignoring unreadable terminal font face {}", regular.index);
            return;
        }
        self.regular = regular;
        self.bold = valid_face(bold, "bold");
        self.italic = valid_face(italic, "italic");
        self.bold_italic = valid_face(bold_italic, "bold italic");
        self.rebuild_metrics();
        self.ctx.request_repaint();
    }

    /// Rebuilds cell metrics for the current face, size, and line height, then
    /// clears every cached glyph and the atlas texture.
    fn rebuild_metrics(&mut self) {
        self.metrics = metrics_for(self.regular.font().as_ref(), self.physical_size, self.ppp);
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

    /// The primary face that serves `style`, preferring a real styled face.
    fn style_face_slot(&self, style: GlyphStyle) -> FaceSlot {
        match style {
            GlyphStyle::Regular => FaceSlot::Regular,
            GlyphStyle::Bold => {
                if self.bold.is_some() {
                    FaceSlot::Bold
                } else {
                    FaceSlot::Regular
                }
            }
            GlyphStyle::Italic => {
                if self.italic.is_some() {
                    FaceSlot::Italic
                } else {
                    FaceSlot::Regular
                }
            }
            GlyphStyle::BoldItalic => {
                if self.bold_italic.is_some() {
                    FaceSlot::BoldItalic
                } else if self.bold.is_some() {
                    FaceSlot::Bold
                } else if self.italic.is_some() {
                    FaceSlot::Italic
                } else {
                    FaceSlot::Regular
                }
            }
        }
    }

    fn face_for_slot(&self, slot: FaceSlot) -> Option<&FontFace> {
        match slot {
            FaceSlot::Regular => Some(&self.regular),
            FaceSlot::Bold => self.bold.as_ref(),
            FaceSlot::Italic => self.italic.as_ref(),
            FaceSlot::BoldItalic => self.bold_italic.as_ref(),
            FaceSlot::Fallback(_) => None,
        }
    }

    /// Resolves the face and glyph id for `ch`, preferring the real styled face.
    fn resolve_glyph(&mut self, ch: char, style: GlyphStyle) -> Option<(FaceSlot, GlyphId)> {
        let style_slot = self.style_face_slot(style);
        if let Some(font) = self.face_for_slot(style_slot).and_then(FontFace::font) {
            let id = font.charmap().map(ch);
            if id != 0 {
                return Some((style_slot, id));
            }
        }

        // A styled face can have a smaller character map than the regular face
        // (patched Nerd Fonts occasionally differ between weights).
        if style_slot != FaceSlot::Regular
            && let Some(font) = self.regular.font()
        {
            let id = font.charmap().map(ch);
            if id != 0 {
                return Some((FaceSlot::Regular, id));
            }
        }

        if let Some(index) = self.find_fallback(ch)
            && let Some(face) = self.fallback_faces.get(index)
            && let Some(font) = FontRef::from_index(&face.data, face.index)
        {
            let id = font.charmap().map(ch);
            if id != 0 {
                return Some((FaceSlot::Fallback(index), id));
            }
        }

        // Preserve the previous behavior for missing glyphs: rasterize the
        // primary face's .notdef rather than dropping the cell.
        let font = self.regular.font()?;
        Some((FaceSlot::Regular, font.charmap().map(ch)))
    }

    /// Rasterizes and caches a glyph, returning `None` only if the atlas cannot
    /// make room even after a reset.
    pub fn glyph(&mut self, ch: char, style: GlyphStyle) -> Option<Glyph> {
        let key = (ch, style);
        if let Some(glyph) = self.cache.get(&key) {
            return Some(*glyph);
        }

        let (slot, glyph_id) = self.resolve_glyph(ch, style)?;
        let synthetic = Synthetic {
            bold: style.is_bold() && !matches!(slot, FaceSlot::Bold | FaceSlot::BoldItalic),
            italic: style.is_italic() && !matches!(slot, FaceSlot::Italic | FaceSlot::BoldItalic),
        };

        let physical_size = self.physical_size;
        let ppp = self.ppp;

        // Keep the borrow of the selected font data inside this block so the
        // rest of the method can mutate the atlas.
        let (image, advance_px) = {
            let source_font = match slot {
                FaceSlot::Regular => self.regular.font(),
                FaceSlot::Bold => self.bold.as_ref().and_then(FontFace::font),
                FaceSlot::Italic => self.italic.as_ref().and_then(FontFace::font),
                FaceSlot::BoldItalic => self.bold_italic.as_ref().and_then(FontFace::font),
                FaceSlot::Fallback(index) => self
                    .fallback_faces
                    .get(index)
                    .and_then(|face| FontRef::from_index(&face.data, face.index)),
            }?;

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
                synthetic,
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

    /// Rasterizes and caches a glyph for software compositing.
    ///
    /// Mask glyphs use swash's `Format::Alpha` and therefore come back as
    /// `Content::Mask`, one coverage byte per pixel. Color glyphs keep
    /// premultiplied RGBA.
    pub fn soft_glyph(&mut self, ch: char, style: GlyphStyle) -> Option<Arc<SoftGlyph>> {
        let key = (ch, style);
        if let Some(glyph) = self.soft_cache.get(&key) {
            return Some(Arc::clone(glyph));
        }

        let (slot, glyph_id) = self.resolve_glyph(ch, style)?;
        let synthetic = Synthetic {
            bold: style.is_bold() && !matches!(slot, FaceSlot::Bold | FaceSlot::BoldItalic),
            italic: style.is_italic() && !matches!(slot, FaceSlot::Italic | FaceSlot::BoldItalic),
        };

        let physical_size = self.physical_size;
        let ppp = self.ppp;
        let image = {
            let source_font = match slot {
                FaceSlot::Regular => self.regular.font(),
                FaceSlot::Bold => self.bold.as_ref().and_then(FontFace::font),
                FaceSlot::Italic => self.italic.as_ref().and_then(FontFace::font),
                FaceSlot::BoldItalic => self.bold_italic.as_ref().and_then(FontFace::font),
                FaceSlot::Fallback(index) => self
                    .fallback_faces
                    .get(index)
                    .and_then(|face| FontRef::from_index(&face.data, face.index)),
            }?;
            rasterize(
                &mut self.scale,
                source_font,
                glyph_id,
                style,
                physical_size,
                ppp,
                synthetic,
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
    synthetic_bold: bool,
    synthetic_italic: bool,
}

/// Fraction of a pixel of extra coverage added to the right edge of a
/// synthetic bold glyph.
const SYNTHETIC_BOLD_STRENGTH: f32 = 0.5;

/// Rasterizes one glyph at `physical_size`, applying bold/italic synthesis.
fn rasterize(
    scale: &mut ScaleContext,
    font: FontRef<'_>,
    glyph_id: GlyphId,
    style: GlyphStyle,
    physical_size: f32,
    ppp: f32,
    synthetic: Synthetic,
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
        RasterOptions {
            hint: true,
            weight,
            synthetic_bold: synthetic.bold,
            synthetic_italic: synthetic.italic,
        },
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
    if options.synthetic_italic {
        render.transform(Some(Transform::skew(Angle::from_radians(0.2), Angle::ZERO)));
    }

    let mut image = render.render(&mut scaler, glyph_id)?;
    if style.is_bold() && options.synthetic_bold && !weight_axis {
        // swash's `Render::embolden` misplaces the two components of Caskaydia's
        // hinted `i` and leaves it *lighter* than regular at 18 px. Widening
        // every mask by a fraction of a pixel is uniform across glyphs and has
        // no interaction with hinting.
        let strength = (SYNTHETIC_BOLD_STRENGTH * ppp).min(1.0);
        synthesized_bold_mask(&mut image, strength);
    }
    Some(image)
}

/// Adds `strength` of the left neighbor's coverage to each mask pixel.
///
/// The mask grows one column to the right; the placement's left edge and the
/// baseline stay fixed. Color glyphs (emoji) are left untouched.
fn synthesized_bold_mask(image: &mut Image, strength: f32) {
    if image.content != Content::Mask || strength <= 0.0 {
        return;
    }
    let width = image.placement.width as usize;
    let height = image.placement.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = &image.data[..image.data.len().min(width * height)];
    let out_width = width + 1;
    let mut data = vec![0u8; out_width * height];
    for y in 0..height {
        for x in 0..out_width {
            let a = if x < width {
                source.get(y * width + x).copied().unwrap_or(0)
            } else {
                0
            };
            let b = if x == 0 {
                0
            } else {
                let previous =
                    f32::from(source.get(y * width + x - 1).copied().unwrap_or(0)) * strength;
                previous.round().clamp(0.0, 255.0) as u8
            };
            let union = u32::from(a) + u32::from(b) - (u32::from(a) * u32::from(b) + 127) / 255;
            data[y * out_width + x] = union.min(255) as u8;
        }
    }

    image.data = data;
    image.placement.width = out_width as u32;
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
    } else {
        // `Format::Alpha` masks are one coverage byte per pixel.
        data.extend_from_slice(&image.data[..image.data.len().min(count)]);
        data.resize(count, 0);
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

fn valid_face(face: Option<FontFace>, style: &str) -> Option<FontFace> {
    face.filter(|face| {
        if face.font().is_none() {
            log::warn!("ignoring unreadable terminal {style} face {}", face.index);
            false
        } else {
            true
        }
    })
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
    use crate::fonts::{self, CASCADIA_MONO};

    fn atlas(size_px: f32, ppp: f32) -> (egui::Context, GlyphAtlas) {
        let ctx = egui::Context::default();
        let atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, size_px, ppp);
        (ctx, atlas)
    }

    fn ink(glyph: &SoftGlyph) -> u64 {
        glyph.data.iter().map(|&alpha| u64::from(alpha)).sum()
    }

    fn row_ink(glyph: &SoftGlyph, y: usize) -> u64 {
        let width = glyph.width as usize;
        glyph.data[y * width..(y + 1) * width]
            .iter()
            .map(|&alpha| u64::from(alpha))
            .sum()
    }

    /// Ink in the detached dot and in the first full stem row.
    fn dot_and_stem_ink(glyph: &SoftGlyph) -> (u64, u64) {
        let height = glyph.height as usize;
        let gap = (1..height)
            .find(|&y| row_ink(glyph, y) == 0)
            .expect("lowercase i should have a blank row under the dot");
        let dot = (0..gap).map(|y| row_ink(glyph, y)).sum();
        let stem_row = gap + 2;
        assert!(stem_row < height, "i stem missing below the dot gap");
        (dot, row_ink(glyph, stem_row))
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
    fn test_soft_glyph_grayscale_mask() {
        let (_ctx, mut atlas) = atlas(18.0, 1.0);
        let glyph = atlas
            .soft_glyph('H', GlyphStyle::Regular)
            .expect("H soft glyph");
        assert!(!glyph.color, "outline glyph should be a mask");
        assert!(glyph.width > 0 && glyph.height > 0);
        assert_eq!(
            glyph.data.len(),
            glyph.width as usize * glyph.height as usize,
            "grayscale masks must store one coverage byte per pixel"
        );
        assert!(glyph.data.iter().any(|&value| value > 0));
        assert!(
            glyph.data.iter().any(|&value| value > 0 && value < 255),
            "expected antialiased edge coverage"
        );
    }

    /// The lowercase `i` must keep a detached but horizontally aligned dot.
    /// A broken placement or a clipped mask shows up as a missing gap, a
    /// one-pixel dot, or a dot that no longer overlaps the stem.
    #[test]
    fn test_i_dot_and_stem_geometry() {
        let (_ctx, mut atlas) = atlas(18.0, 1.0);
        let glyph = atlas
            .soft_glyph('i', GlyphStyle::Regular)
            .expect("i soft glyph");
        assert!(!glyph.color);
        let width = glyph.width as usize;
        let height = glyph.height as usize;
        let ink = |y: usize| -> Vec<usize> {
            (0..width)
                .filter(|&x| glyph.data[y * width + x] > 0)
                .collect()
        };

        let dot = ink(0);
        assert!(!dot.is_empty(), "i dot is clipped at the top");
        assert!(
            dot.iter().all(|&x| x > 0 && x + 1 < width),
            "i dot touches the mask edge: {dot:?}"
        );
        assert!(dot.len() >= 2, "i dot is only {} px wide", dot.len());
        assert!(
            ink(height - 1).len() >= 2,
            "i bottom is clipped at the last mask row"
        );

        let gap = (1..height)
            .find(|&y| ink(y).is_empty())
            .expect("i dot should be detached from the stem by a blank row");
        assert!(gap > 0 && gap < height / 2, "unexpected i gap at row {gap}");

        let stem_row = gap + 2;
        assert!(stem_row < height, "i stem missing below the dot gap");
        let stem = ink(stem_row);
        assert!(stem.len() >= 2, "i stem is only {} px wide", stem.len());
        assert!(
            dot.iter().any(|x| stem.contains(x)),
            "i dot {dot:?} does not overlap stem {stem:?}"
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
            synthetic_bold: false,
            synthetic_italic: false,
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
    fn test_bold_i_has_more_ink_than_regular() {
        for ppp in [1.0, 1.25, 1.5] {
            let (_ctx, mut atlas) = atlas(18.0, ppp);
            let regular = atlas
                .soft_glyph('i', GlyphStyle::Regular)
                .expect("regular i");
            let bold = atlas.soft_glyph('i', GlyphStyle::Bold).expect("bold i");

            assert_ne!(
                regular.data, bold.data,
                "bold i reused the regular mask at ppp {ppp}"
            );
            let (regular_dot, regular_stem) = dot_and_stem_ink(&regular);
            let (bold_dot, bold_stem) = dot_and_stem_ink(&bold);
            assert!(
                bold_dot > regular_dot,
                "bold i dot ink {bold_dot} <= regular {regular_dot} at ppp {ppp}"
            );
            assert!(
                bold_stem > regular_stem,
                "bold i stem ink {bold_stem} <= regular {regular_stem} at ppp {ppp}"
            );
            assert!(
                ink(&bold) > ink(&regular),
                "bold i total ink {} <= regular {} at ppp {ppp}",
                ink(&bold),
                ink(&regular)
            );
        }
    }

    #[test]
    fn test_bold_i_does_not_reuse_regular_atlas_glyph() {
        let (_ctx, mut atlas) = atlas(18.0, 1.0);
        let regular = atlas.glyph('i', GlyphStyle::Regular).expect("regular i");
        let bold = atlas.glyph('i', GlyphStyle::Bold).expect("bold i");
        assert_ne!(regular, bold, "bold i returned the regular atlas glyph");

        let regular_soft = atlas
            .soft_glyph('i', GlyphStyle::Regular)
            .expect("regular i");
        let bold_soft = atlas.soft_glyph('i', GlyphStyle::Bold).expect("bold i");
        assert!(!Arc::ptr_eq(&regular_soft, &bold_soft));
    }

    /// Synthetic bold must thicken every mask uniformly. A one-pixel stem plus
    /// a detached dot is the shape that swash's embolden used to collapse.
    #[test]
    fn test_synthetic_bold_widens_a_thin_stem_and_dot() {
        let mut image = Image {
            content: Content::Mask,
            ..Image::default()
        };
        image.placement.width = 4;
        image.placement.height = 7;
        image.data = vec![0; 4 * 7];
        for y in 0..2 {
            for x in 1..3 {
                image.data[y * 4 + x] = 255;
            }
        }
        for y in 3..7 {
            image.data[y * 4 + 1] = 255;
        }
        let before = ink_pixels(&image);

        synthesized_bold_mask(&mut image, 0.5);

        assert_eq!(image.placement.width, 5);
        assert!(ink_pixels(&image) > before);
        for y in 0..2 {
            assert!(image.data[y * 5 + 1] > 0 && image.data[y * 5 + 2] > 0);
            assert!(image.data[y * 5 + 3] > 0, "dot did not widen right");
        }
        for y in 3..7 {
            assert!(image.data[y * 5 + 1] > 0 && image.data[y * 5 + 2] > 0);
        }

        let mut color = Image {
            content: Content::Color,
            ..Image::default()
        };
        color.placement.width = 1;
        color.placement.height = 1;
        color.data = vec![1, 2, 3, 4];
        synthesized_bold_mask(&mut color, 1.0);
        assert_eq!(color.data, vec![1, 2, 3, 4]);
        assert_eq!(color.placement.width, 1);
    }

    fn ink_pixels(image: &Image) -> u64 {
        image.data.iter().map(|&alpha| u64::from(alpha)).sum()
    }

    /// The installed Nerd Font ships real static Bold/BoldItalic/Italic faces.
    /// Bold must use the real face with no extra synthetic widening.
    #[test]
    fn test_real_bold_face_is_used_when_installed() {
        let Some(font) = fonts::load_terminal_font_faces("CaskaydiaCove Nerd Font") else {
            eprintln!("skipping real bold-face assertion: Nerd Font is not installed");
            return;
        };
        let Some(bold) = font.bold.clone() else {
            eprintln!("skipping real bold-face assertion: Nerd Font has no Bold face");
            return;
        };
        assert!(
            font.italic.is_some(),
            "Nerd Font Italic face was not loaded"
        );
        assert!(
            font.bold_italic.is_some(),
            "Nerd Font BoldItalic face was not loaded"
        );

        let ctx = egui::Context::default();
        let mut styled = GlyphAtlas::new(&ctx, CASCADIA_MONO, 18.0, 1.0);
        styled.set_font_faces(
            font.regular.clone(),
            Some(bold.clone()),
            font.italic.clone(),
            font.bold_italic.clone(),
        );
        styled.set_size(&ctx, 18.0, 1.0);
        for ch in ['i', 'l', 'M', 'n'] {
            let regular = styled
                .soft_glyph(ch, GlyphStyle::Regular)
                .expect("regular glyph");
            let bold = styled.soft_glyph(ch, GlyphStyle::Bold).expect("bold glyph");
            assert!(
                ink(&bold) > ink(&regular),
                "{ch:?}: real bold ink {} <= regular {}",
                ink(&bold),
                ink(&regular)
            );
        }
        let bold_i = styled
            .soft_glyph('i', GlyphStyle::Bold)
            .expect("bold i from real face");

        let mut control = GlyphAtlas::new(&ctx, CASCADIA_MONO, 18.0, 1.0);
        control.set_font_faces(bold, None, None, None);
        control.set_size(&ctx, 18.0, 1.0);
        let control_i = control
            .soft_glyph('i', GlyphStyle::Regular)
            .expect("bold face as regular");

        assert_eq!(
            bold_i.data, control_i.data,
            "bold i did not use the real Bold face unchanged"
        );
        assert_eq!(bold_i.width, control_i.width);
        assert_eq!(bold_i.left, control_i.left);
        assert_eq!(bold_i.top, control_i.top);
    }

    /// Without a real bold face, all synthetic bold glyphs must thicken even
    /// when the regular face is a static font with a one-pixel `i` stem.
    #[test]
    fn test_synthetic_bold_strengthens_i_l_m_n() {
        let Some((data, index)) = fonts::load_terminal_font("CaskaydiaCove Nerd Font") else {
            eprintln!("skipping synthetic bold assertion: Nerd Font is not installed");
            return;
        };
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 18.0, 1.0);
        atlas.set_font(data, index);
        atlas.set_size(&ctx, 18.0, 1.0);

        for ch in ['i', 'l', 'M', 'n'] {
            let regular = atlas
                .soft_glyph(ch, GlyphStyle::Regular)
                .expect("regular glyph");
            let bold = atlas
                .soft_glyph(ch, GlyphStyle::Bold)
                .expect("synthetic bold glyph");
            assert!(
                ink(&bold) > ink(&regular),
                "{ch:?}: synthetic bold ink {} <= regular {}",
                ink(&bold),
                ink(&regular)
            );
            assert!(
                bold.width > regular.width,
                "{ch:?}: synthetic bold width {} <= regular {}",
                bold.width,
                regular.width
            );
        }

        let regular_i = atlas
            .soft_glyph('i', GlyphStyle::Regular)
            .expect("regular i");
        let bold_i = atlas.soft_glyph('i', GlyphStyle::Bold).expect("bold i");
        let gap = (1..regular_i.height as usize)
            .find(|&y| row_ink(&regular_i, y) == 0)
            .expect("regular i should have a blank row under the dot");
        let regular_dot: u64 = (0..gap).map(|y| row_ink(&regular_i, y)).sum();
        let bold_dot: u64 = (0..gap).map(|y| row_ink(&bold_i, y)).sum();
        let stem_row = gap + 2;
        assert!(stem_row < regular_i.height as usize);
        assert!(
            bold_dot > regular_dot,
            "synthetic bold dot ink {bold_dot} <= regular {regular_dot}"
        );
        assert!(
            row_ink(&bold_i, stem_row) > row_ink(&regular_i, stem_row),
            "synthetic bold stem did not thicken"
        );
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
                RasterOptions {
                    hint: true,
                    weight,
                    synthetic_bold: false,
                    synthetic_italic: false,
                },
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
                synthetic_bold: false,
                synthetic_italic: false,
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
