//! GPU glyph and fill compositor for the terminal grid.
//!
//! The CPU [`crate::soft`] pass keeps base/cell backgrounds and
//! underline/strike decorations. Selection, search, and cursor fills are solid
//! quads drawn here, before glyph quads, so they never invalidate the CPU
//! surface. Glyph quads sample a dedicated atlas of grayscale coverage and
//! color (premultiplied RGBA) glyphs.
//!
//! Mask glyphs are one coverage byte per pixel and use ordinary premultiplied
//! alpha blending: `fg * coverage` over the destination, with coverage folded
//! into alpha. That has no per-channel blending and therefore no ClearType
//! fringing, and it composites correctly over translucent Mica/Acrylic
//! surfaces. Color glyphs (emoji) use premultiplied `One`/`OneMinusSrcAlpha`.
//! Both use nearest sampling and physical-pixel quad positions, so atlas
//! texels map 1:1 to screen pixels.
//!
//! Vertex data is cached by a content fingerprint and uploaded into a
//! three-slot grow-only buffer ring; unchanged frames rebuild and upload
//! nothing. wgpu resources are created lazily in `prepare` on the render
//! thread. If the atlas overflows or shared state is poisoned,
//! [`GpuText::build_frame`] returns `None` and the caller keeps CPU text for
//! the whole frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{Color32, Rect};
use egui_wgpu::wgpu;
use egui_wgpu::{CallbackTrait, ScreenDescriptor};
use illness_core::CursorKind;
use illness_term::Snapshot;
use illness_term::snapshot::CellFlags;

use crate::design;
use crate::glyph::{CellMetrics, GlyphAtlas, GlyphStyle};
use crate::soft::{self, SurfaceSpec};
use crate::terminal::{cell_colors, dim, find_matches, glyph_style, row_text, selection_ranges};

/// Side length of the GPU atlas in texels.
const ATLAS_SIZE: u32 = 2048;
/// Bytes per vertex: position (2), UV (2), color (4) as `f32`.
const VERTEX_SIZE: usize = 8 * 4;
/// Smallest vertex buffer allocation, so short frames do not churn buffers.
const MIN_VERTEX_BYTES: u64 = 4096;
/// Number of vertex buffers; one is uploaded while earlier ones may still be
/// read by submitted frames.
const SLOT_COUNT: usize = 3;
/// Solid quads (selection, search, block cursor) are drawn before glyphs.
const LAYER_FILLS: usize = 0;
/// Grayscale mask glyphs.
const LAYER_MASK: usize = 1;
/// Color glyphs (emoji).
const LAYER_COLOR: usize = 2;
/// Beam and underline cursor fills are drawn over glyphs.
const LAYER_CURSOR: usize = 3;
const LAYER_COUNT: usize = 4;

const SHADER: &str = r#"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@group(0) @binding(0) var atlas: texture_2d<f32>;
@group(0) @binding(1) var atlas_sampler: sampler;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    return out;
}

@fragment
fn fs_mask(in: VertexOutput) -> @location(0) vec4<f32> {
    // Mask glyphs store one coverage byte replicated across RGB with an opaque
    // alpha, so premultiplied alpha blending applies the same coverage to
    // every channel.
    let coverage = textureSample(atlas, atlas_sampler, in.uv).r;
    let alpha = in.color.a * coverage;
    return vec4<f32>(in.color.rgb * alpha, alpha);
}

@fragment
fn fs_color(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(atlas, atlas_sampler, in.uv) * in.color.a;
}

@fragment
fn fs_solid(in: VertexOutput) -> @location(0) vec4<f32> {
    // Vertex colors are premultiplied by material and fill alpha.
    return in.color;
}
"#;

/// Cell metrics and DPI that a packed glyph's pixels were rasterized for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct MetricsKey {
    width_px: u32,
    height_px: u32,
    baseline: u32,
    ppp: u32,
}

impl MetricsKey {
    fn new(metrics: CellMetrics, ppp: f32) -> Self {
        Self {
            width_px: metrics.width_px,
            height_px: metrics.height_px,
            baseline: metrics.baseline.to_bits(),
            ppp: ppp.to_bits(),
        }
    }
}

/// One glyph's placement inside the GPU atlas, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GlyphSlot {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// Offset from the cell's left edge to the glyph bitmap.
    left: i32,
    /// Offset from the cell's baseline up to the glyph bitmap's top.
    top: i32,
    color: bool,
}

/// A packed tile waiting to be copied into the atlas texture.
struct Upload {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

#[derive(Default)]
struct Shelf {
    x: u32,
    y: u32,
    row_height: u32,
}

impl Shelf {
    /// Reserves `width × height` plus one texel of trailing padding.
    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        let width = width.checked_add(1)?;
        let height = height.checked_add(1)?;
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

/// CPU-side packing state for the GPU atlas.
struct GlyphCache {
    slots: HashMap<(char, GlyphStyle), Option<GlyphSlot>>,
    shelf: Shelf,
    uploads: Vec<Upload>,
    metrics: Option<MetricsKey>,
    /// Bumped whenever existing UVs stop being valid.
    generation: u64,
    /// Sticky until the font metrics change; a partially packed frame must
    /// fall back to CPU text instead of silently dropping glyphs.
    overflowed: bool,
}

impl GlyphCache {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            shelf: Shelf::default(),
            uploads: Vec::new(),
            metrics: None,
            generation: 1,
            overflowed: false,
        }
    }

    fn reset(&mut self) {
        self.slots.clear();
        self.shelf = Shelf::default();
        self.uploads.clear();
        self.overflowed = false;
        self.metrics = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Drops cached glyphs only when the rasterization geometry changes.
    /// Overflow is sticky until then so a broken frame does not rebuild the
    /// whole atlas on every repaint.
    fn begin_frame(&mut self, metrics: CellMetrics, ppp: f32) {
        let key = MetricsKey::new(metrics, ppp);
        if self.metrics != Some(key) {
            self.reset();
            self.metrics = Some(key);
        }
    }

    fn slot(&mut self, atlas: &mut GlyphAtlas, ch: char, style: GlyphStyle) -> Option<GlyphSlot> {
        let key = (ch, style);
        if let Some(cached) = self.slots.get(&key) {
            return *cached;
        }
        if self.overflowed {
            return None;
        }

        let glyph = atlas.soft_glyph(ch, style)?;
        if glyph.width == 0 || glyph.height == 0 {
            self.slots.insert(key, None);
            return None;
        }
        let Some((x, y)) = self.shelf.allocate(glyph.width, glyph.height) else {
            self.overflowed = true;
            log::warn!("GPU glyph atlas is full; using CPU text until the font or metrics change");
            return None;
        };

        let pixel_count = glyph.width as usize * glyph.height as usize;
        let mut data = Vec::with_capacity(pixel_count * 4);
        if glyph.color {
            let bytes = &glyph.data[..glyph.data.len().min(pixel_count * 4)];
            data.extend_from_slice(bytes);
            data.resize(pixel_count * 4, 0);
        } else {
            // Mask glyphs are one coverage byte per pixel. Replicate it into
            // RGB so the shader's `.r` sample and the debug dumps both show
            // neutral (equal-channel) coverage.
            for &coverage in glyph.data.iter().take(pixel_count) {
                data.extend_from_slice(&[coverage, coverage, coverage, 255]);
            }
            data.resize(pixel_count * 4, 0);
        }

        self.uploads.push(Upload {
            x,
            y,
            width: glyph.width,
            height: glyph.height,
            data,
        });
        let slot = GlyphSlot {
            x,
            y,
            width: glyph.width,
            height: glyph.height,
            left: glyph.left,
            top: glyph.top,
            color: glyph.color,
        };
        self.slots.insert(key, Some(slot));
        Some(slot)
    }
}

/// Reusable vertex scratch layers plus the assembled stream for one frame.
///
/// `layers` keeps its capacity across rebuilds and the assembled `vertices`
/// grows to the high-water mark, so a changed frame does not allocate a new
/// buffer.
struct VertexStream {
    fingerprint: Option<u64>,
    generation: u64,
    layers: [Vec<u8>; LAYER_COUNT],
    vertices: Vec<u8>,
    counts: [u32; LAYER_COUNT],
    slot: usize,
}

impl Default for VertexStream {
    fn default() -> Self {
        Self {
            fingerprint: None,
            generation: 0,
            layers: std::array::from_fn(|_| Vec::new()),
            vertices: Vec::new(),
            counts: [0; LAYER_COUNT],
            slot: 0,
        }
    }
}

impl VertexStream {
    /// Rebuilds only when the content fingerprint or atlas generation changed.
    /// Returns `true` when new vertices must be uploaded.
    fn update(
        &mut self,
        fingerprint: u64,
        generation: u64,
        surface_size: [u32; 2],
        material_alpha: u8,
        build: impl FnOnce(&mut VertexBuilder),
    ) -> bool {
        if self.fingerprint == Some(fingerprint) && self.generation == generation {
            return false;
        }

        let layers = std::array::from_fn(|index| std::mem::take(&mut self.layers[index]));
        let mut builder = VertexBuilder::new(surface_size, material_alpha, layers);
        build(&mut builder);
        let (counts, layers) = builder.finish();

        self.vertices.clear();
        for layer in &layers {
            self.vertices.extend_from_slice(layer);
        }
        self.layers = layers;
        self.counts = counts;
        self.fingerprint = Some(fingerprint);
        self.generation = generation;
        self.slot = (self.slot + 1) % SLOT_COUNT;
        true
    }

    /// Forgets the cached stream without releasing its capacity.
    fn invalidate(&mut self) {
        self.fingerprint = None;
        self.vertices.clear();
        self.counts = [0; LAYER_COUNT];
    }
}

/// Builds the layered vertex stream for one terminal frame.
struct VertexBuilder {
    surface_size: [u32; 2],
    /// Material alpha, baked into every vertex so translucent windows keep
    /// the same weight as the CPU surface.
    alpha: f32,
    layers: [Vec<u8>; LAYER_COUNT],
}

impl VertexBuilder {
    fn new(surface_size: [u32; 2], material_alpha: u8, mut layers: [Vec<u8>; LAYER_COUNT]) -> Self {
        for layer in &mut layers {
            layer.clear();
        }
        Self {
            surface_size,
            alpha: f32::from(material_alpha) / 255.0,
            layers,
        }
    }

    /// Emits two triangles for one glyph bitmap at physical pixel `x`,`y`.
    fn push_quad(&mut self, slot: GlyphSlot, x: i32, y: i32, fg: Color32) {
        let width = self.surface_size[0] as i32;
        let height = self.surface_size[1] as i32;
        if width <= 0 || height <= 0 || slot.width == 0 || slot.height == 0 {
            return;
        }
        let x1 = x + slot.width as i32;
        let y1 = y + slot.height as i32;
        if x1 <= 0 || y1 <= 0 || x >= width || y >= height {
            return;
        }

        let u0 = slot.x as f32 / ATLAS_SIZE as f32;
        let v0 = slot.y as f32 / ATLAS_SIZE as f32;
        let u1 = (slot.x + slot.width) as f32 / ATLAS_SIZE as f32;
        let v1 = (slot.y + slot.height) as f32 / ATLAS_SIZE as f32;

        let ndc_x0 = x as f32 / width as f32 * 2.0 - 1.0;
        let ndc_x1 = x1 as f32 / width as f32 * 2.0 - 1.0;
        let ndc_y0 = 1.0 - y as f32 / height as f32 * 2.0;
        let ndc_y1 = 1.0 - y1 as f32 / height as f32 * 2.0;

        let color = if slot.color {
            [1.0, 1.0, 1.0, self.alpha]
        } else {
            [
                f32::from(fg.r()) / 255.0,
                f32::from(fg.g()) / 255.0,
                f32::from(fg.b()) / 255.0,
                self.alpha,
            ]
        };
        let layer = if slot.color { LAYER_COLOR } else { LAYER_MASK };
        let target = &mut self.layers[layer];
        let corners = [
            ([ndc_x0, ndc_y0], [u0, v0]),
            ([ndc_x1, ndc_y0], [u1, v0]),
            ([ndc_x1, ndc_y1], [u1, v1]),
            ([ndc_x0, ndc_y0], [u0, v0]),
            ([ndc_x1, ndc_y1], [u1, v1]),
            ([ndc_x0, ndc_y1], [u0, v1]),
        ];
        for (position, uv) in corners {
            push_vertex(target, position, uv, color);
        }
    }

    /// Emits an axis-aligned solid rectangle in physical pixels.
    fn push_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color32, layer: usize) {
        let width = self.surface_size[0] as i32;
        let height = self.surface_size[1] as i32;
        let x0 = x0.clamp(0, width);
        let x1 = x1.clamp(0, width);
        let y0 = y0.clamp(0, height);
        let y1 = y1.clamp(0, height);
        if width <= 0 || height <= 0 || x1 <= x0 || y1 <= y0 {
            return;
        }

        let ndc_x0 = x0 as f32 / width as f32 * 2.0 - 1.0;
        let ndc_x1 = x1 as f32 / width as f32 * 2.0 - 1.0;
        let ndc_y0 = 1.0 - y0 as f32 / height as f32 * 2.0;
        let ndc_y1 = 1.0 - y1 as f32 / height as f32 * 2.0;

        let fill_alpha = f32::from(color.a()) / 255.0;
        let color = [
            f32::from(color.r()) / 255.0 * self.alpha * fill_alpha,
            f32::from(color.g()) / 255.0 * self.alpha * fill_alpha,
            f32::from(color.b()) / 255.0 * self.alpha * fill_alpha,
            fill_alpha,
        ];
        let target = &mut self.layers[layer];
        let corners = [
            ([ndc_x0, ndc_y0], [0.0, 0.0]),
            ([ndc_x1, ndc_y0], [0.0, 0.0]),
            ([ndc_x1, ndc_y1], [0.0, 0.0]),
            ([ndc_x0, ndc_y0], [0.0, 0.0]),
            ([ndc_x1, ndc_y1], [0.0, 0.0]),
            ([ndc_x0, ndc_y1], [0.0, 0.0]),
        ];
        for (position, uv) in corners {
            push_vertex(target, position, uv, color);
        }
    }

    fn finish(self) -> ([u32; LAYER_COUNT], [Vec<u8>; LAYER_COUNT]) {
        let counts = std::array::from_fn(|index| (self.layers[index].len() / VERTEX_SIZE) as u32);
        (counts, self.layers)
    }
}

fn push_vertex(target: &mut Vec<u8>, position: [f32; 2], uv: [f32; 2], color: [f32; 4]) {
    for value in position.into_iter().chain(uv).chain(color) {
        target.extend_from_slice(&value.to_le_bytes());
    }
}

#[allow(clippy::too_many_arguments)]
fn push_glyph(
    builder: &mut VertexBuilder,
    cache: &mut GlyphCache,
    atlas: &mut GlyphAtlas,
    ch: char,
    style: GlyphStyle,
    cell_x: i32,
    cell_y: i32,
    baseline: i32,
    fg: Color32,
) {
    let Some(slot) = cache.slot(atlas, ch, style) else {
        return;
    };
    builder.push_quad(slot, cell_x + slot.left, cell_y + baseline - slot.top, fg);
}

/// Draws selection, search, and cursor fills as solid quads.
fn build_fills(
    builder: &mut VertexBuilder,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    metrics: CellMetrics,
) {
    let cell_width = metrics.width_px as i32;
    let cell_height = metrics.height_px as i32;
    if cell_width <= 0 || cell_height <= 0 || spec.cols == 0 || spec.rows == 0 {
        return;
    }
    let origin = (spec.padding.max(0.0) * spec.ppp).round() as i32;

    for (row, start, end) in selection_ranges(snapshot, spec.cols, spec.rows) {
        if start >= end {
            continue;
        }
        builder.push_rect(
            origin + start as i32 * cell_width,
            origin + row as i32 * cell_height,
            origin + end as i32 * cell_width,
            origin + (row as i32 + 1) * cell_height,
            spec.tokens.selection,
            LAYER_FILLS,
        );
    }

    if let Some(search) = spec.search
        && !search.query.is_empty()
    {
        let highlight = Color32::from_rgba_unmultiplied(
            spec.tokens.accent.r(),
            spec.tokens.accent.g(),
            spec.tokens.accent.b(),
            96,
        );
        for row in 0..spec.rows {
            let text = row_text(snapshot, row, spec.cols);
            for (start, end) in find_matches(&text, &search.query, search.case_sensitive) {
                if start >= end {
                    continue;
                }
                builder.push_rect(
                    origin + start as i32 * cell_width,
                    origin + row as i32 * cell_height,
                    origin + end as i32 * cell_width,
                    origin + (row as i32 + 1) * cell_height,
                    highlight,
                    LAYER_FILLS,
                );
            }
        }
    }

    if !spec.show_cursor || spec.cols == 0 || spec.rows == 0 {
        return;
    }
    let mut col = snapshot.cursor.col.min(spec.cols - 1);
    let row = snapshot.cursor.row.min(spec.rows - 1);
    if col > 0
        && snapshot
            .cell(col, row)
            .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
    {
        col -= 1;
    }
    let wide = snapshot
        .cell(col, row)
        .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR));
    let x0 = origin + col as i32 * cell_width;
    let y0 = origin + row as i32 * cell_height;
    let x1 = x0 + if wide { cell_width * 2 } else { cell_width };
    let y1 = y0 + cell_height;
    let color = design::color(spec.theme.cursor);
    match snapshot.cursor.kind {
        CursorKind::Block => {
            builder.push_rect(x0, y0, x1, y1, color, LAYER_FILLS);
        }
        CursorKind::Beam => {
            builder.push_rect(x0, y0, x0 + 2, y1, color, LAYER_CURSOR);
        }
        CursorKind::Underline => {
            builder.push_rect(x0, y1 - 2, x1, y1, color, LAYER_CURSOR);
        }
    }
}

/// Builds every glyph quad for the visible grid.
fn build_vertices(
    builder: &mut VertexBuilder,
    cache: &mut GlyphCache,
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
) {
    let metrics = atlas.metrics();
    let cell_width = metrics.width_px as i32;
    let cell_height = metrics.height_px as i32;
    if cell_width <= 0 || cell_height <= 0 {
        return;
    }
    let origin = (spec.padding.max(0.0) * spec.ppp).round() as i32;
    let baseline = (metrics.baseline * spec.ppp).round() as i32;

    for row in 0..spec.rows {
        for col in 0..spec.cols {
            let Some(cell) = snapshot.cell(col, row) else {
                continue;
            };
            let flags = cell.flags;
            if flags.intersects(
                CellFlags::HIDDEN
                    | CellFlags::WIDE_CHAR_SPACER
                    | CellFlags::LEADING_WIDE_CHAR_SPACER,
            ) {
                continue;
            }
            let has_decoration = flags.intersects(CellFlags::ALL_UNDERLINES | CellFlags::STRIKEOUT);
            if cell.ch == ' ' && !has_decoration {
                continue;
            }
            let (fg, _) = cell_colors(cell, spec.theme);
            let fg = if flags.contains(CellFlags::DIM) {
                dim(fg)
            } else {
                fg
            };
            let style = glyph_style(flags);
            let cell_x = origin + col as i32 * cell_width;
            let cell_y = origin + row as i32 * cell_height;

            if cell.ch != ' ' {
                push_glyph(
                    builder, cache, atlas, cell.ch, style, cell_x, cell_y, baseline, fg,
                );
            }
            for &mark in snapshot.zero_width(cell) {
                push_glyph(
                    builder, cache, atlas, mark, style, cell_x, cell_y, baseline, fg,
                );
            }
        }
    }

    // A block cursor inverts its glyph; the CPU/GPU fill pass draws the block.
    if spec.show_cursor
        && snapshot.cursor.kind == CursorKind::Block
        && spec.cols > 0
        && spec.rows > 0
    {
        let mut col = snapshot.cursor.col.min(spec.cols - 1);
        let row = snapshot.cursor.row.min(spec.rows - 1);
        if col > 0
            && snapshot
                .cell(col, row)
                .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
        {
            col -= 1;
        }
        if let Some(cell) = snapshot.cell(col, row)
            && !cell.flags.intersects(
                CellFlags::HIDDEN
                    | CellFlags::WIDE_CHAR_SPACER
                    | CellFlags::LEADING_WIDE_CHAR_SPACER,
            )
            && cell.ch != ' '
        {
            let fg = design::color(spec.theme.bg);
            let style = glyph_style(cell.flags);
            let cell_x = origin + col as i32 * cell_width;
            let cell_y = origin + row as i32 * cell_height;
            push_glyph(
                builder, cache, atlas, cell.ch, style, cell_x, cell_y, baseline, fg,
            );
            for &mark in snapshot.zero_width(cell) {
                push_glyph(
                    builder, cache, atlas, mark, style, cell_x, cell_y, baseline, fg,
                );
            }
        }
    }
}

/// Shared GPU state and the CPU atlas that feeds it.
pub struct GpuText {
    target_format: wgpu::TextureFormat,
    state: Mutex<State>,
    failed: AtomicBool,
}

struct State {
    cache: GlyphCache,
    stream: VertexStream,
    buffers: Vec<Option<VertexBuffer>>,
    /// Slot, fingerprint, and generation currently present in that slot's
    /// buffer. Frames that egui never rendered must not look uploaded.
    uploaded: Option<(usize, u64, u64)>,
    resources: Option<Resources>,
}

impl GpuText {
    /// Wraps the device that eframe created for the window.
    pub fn new(render_state: &egui_wgpu::RenderState) -> Self {
        Self {
            target_format: render_state.target_format,
            state: Mutex::new(State {
                cache: GlyphCache::new(),
                stream: VertexStream::default(),
                buffers: Vec::new(),
                uploaded: None,
                resources: None,
            }),
            failed: AtomicBool::new(false),
        }
    }

    /// Invalidates every packed glyph. Call after swapping the font face.
    pub fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.cache.reset();
            state.stream.invalidate();
            state.uploaded = None;
        }
    }

    /// Whether the compositor has permanently disabled itself.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Packs missing glyphs and produces this frame's callback. `None` means
    /// the caller must keep CPU glyphs instead.
    pub fn build_frame(
        self: &Arc<Self>,
        atlas: &mut GlyphAtlas,
        snapshot: &Snapshot,
        spec: &SurfaceSpec<'_>,
    ) -> Option<GpuFrame> {
        if self.failed() {
            return None;
        }
        let surface_size = soft::surface_size(spec.rect, spec.ppp)?;
        let surface_size = [surface_size[0] as u32, surface_size[1] as u32];
        let metrics = atlas.metrics();
        let Ok(mut state) = self.state.lock() else {
            self.failed.store(true, Ordering::Relaxed);
            log::warn!("GPU text state is poisoned; falling back to the CPU compositor");
            return None;
        };

        state.cache.begin_frame(metrics, spec.ppp);
        // Debug-only fault injection used by the launch checks to prove the
        // whole-frame CPU fallback and one-time overflow log.
        #[cfg(debug_assertions)]
        if std::env::var_os("ILLNESS_FORCE_GPU_ATLAS_OVERFLOW").is_some() {
            state.cache.shelf.y = ATLAS_SIZE;
        }
        if state.cache.overflowed {
            return None;
        }

        let mut gpu_spec = *spec;
        gpu_spec.text = false;
        gpu_spec.fills = true;
        let fingerprint = soft::fingerprint(snapshot, &gpu_spec);
        let generation = state.cache.generation;
        let material_alpha = design::material_alpha(spec.material);

        let started = Instant::now();
        let rebuilt = {
            let State { cache, stream, .. } = &mut *state;
            stream.update(
                fingerprint,
                generation,
                surface_size,
                material_alpha,
                |builder| {
                    build_fills(builder, snapshot, spec, metrics);
                    build_vertices(builder, cache, atlas, snapshot, spec);
                },
            )
        };
        if rebuilt && log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "gpu frame rebuild {} vertices, {} bytes in {:?}",
                state.stream.vertices.len() / VERTEX_SIZE,
                state.stream.vertices.len(),
                started.elapsed()
            );
        }

        // The atlas filled up while packing. Discard the partial stream and
        // let the caller draw this whole frame with CPU glyphs.
        if state.cache.overflowed {
            state.stream.invalidate();
            return None;
        }

        Some(GpuFrame {
            shared: Arc::clone(self),
            slot: state.stream.slot,
            fingerprint,
            generation,
            counts: state.stream.counts,
            bytes: state.stream.vertices.len() as u64,
        })
    }
}

struct VertexBuffer {
    buffer: wgpu::Buffer,
    size: u64,
}

/// One frame's quads. The paint callback owns it until egui renders.
pub struct GpuFrame {
    shared: Arc<GpuText>,
    slot: usize,
    fingerprint: u64,
    generation: u64,
    counts: [u32; LAYER_COUNT],
    bytes: u64,
}

impl GpuFrame {
    /// True when there is nothing to draw (blank grid, or every glyph failed
    /// to pack).
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|&count| count == 0)
    }

    /// Converts the frame into an egui paint callback over `rect`.
    pub fn into_shape(self, rect: Rect) -> egui::Shape {
        egui::Shape::Callback(egui_wgpu::Callback::new_paint_callback(rect, self))
    }
}

impl CallbackTrait for GpuFrame {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if self.is_empty() || self.shared.failed() {
            return Vec::new();
        }
        let Ok(mut state) = self.shared.state.lock() else {
            self.shared.failed.store(true, Ordering::Relaxed);
            return Vec::new();
        };
        if state.cache.generation != self.generation {
            return Vec::new();
        }

        if state.resources.is_none() {
            state.resources = Some(Resources::new(
                device,
                self.shared.target_format,
                self.generation,
            ));
            log::info!("GPU text atlas and pipelines created");
        }
        let uploads = std::mem::take(&mut state.cache.uploads);
        if let Some(resources) = state.resources.as_mut() {
            if resources.generation != self.generation {
                resources.rebuild_atlas(device, self.generation);
            }
            for upload in &uploads {
                resources.upload(queue, upload);
            }
        }

        let key = (self.slot, self.fingerprint, self.generation);
        if state.uploaded != Some(key) {
            let matches = {
                let State {
                    stream, buffers, ..
                } = &mut *state;
                let matches = stream.fingerprint == Some(self.fingerprint)
                    && stream.vertices.len() as u64 == self.bytes;
                if matches {
                    upload_vertices(device, queue, buffers, self.slot, &stream.vertices);
                }
                matches
            };
            if matches {
                state.uploaded = Some(key);
            }
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        _callback_resources: &egui_wgpu::CallbackResources,
    ) {
        if self.is_empty() || self.shared.failed() {
            return;
        }
        let Ok(state) = self.shared.state.lock() else {
            return;
        };
        let Some(resources) = state.resources.as_ref() else {
            return;
        };
        if state.cache.generation != self.generation || resources.generation != self.generation {
            return;
        }
        let Some(slot) = state.buffers.get(self.slot).and_then(Option::as_ref) else {
            return;
        };

        // egui already set the viewport to the callback rect. Restrict the
        // draw to the widget's clip rect; the next egui primitive resets both.
        let clip = info.clip_rect_in_pixels();
        let screen = info.screen_size_px;
        let left = clip.left_px.clamp(0, screen[0] as i32) as u32;
        let top = clip.top_px.clamp(0, screen[1] as i32) as u32;
        let right = (clip.left_px + clip.width_px).clamp(0, screen[0] as i32) as u32;
        let bottom = (clip.top_px + clip.height_px).clamp(0, screen[1] as i32) as u32;
        if right <= left || bottom <= top {
            return;
        }
        render_pass.set_scissor_rect(left, top, right - left, bottom - top);
        render_pass.set_bind_group(0, &resources.bind_group, &[]);
        render_pass.set_vertex_buffer(0, slot.buffer.slice(..));

        let mut first = 0;
        let [fills, mask, color, cursor] = self.counts;
        if fills > 0 {
            render_pass.set_pipeline(&resources.solid_pipeline);
            render_pass.draw(first..first + fills, 0..1);
            first += fills;
        }
        if mask > 0 {
            render_pass.set_pipeline(&resources.mask_pipeline);
            render_pass.draw(first..first + mask, 0..1);
            first += mask;
        }
        if color > 0 {
            render_pass.set_pipeline(&resources.color_pipeline);
            render_pass.draw(first..first + color, 0..1);
            first += color;
        }
        if cursor > 0 {
            render_pass.set_pipeline(&resources.solid_pipeline);
            render_pass.draw(first..first + cursor, 0..1);
        }
    }
}

/// Creates or grows one ring slot and uploads `data` into it.
fn upload_vertices(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    buffers: &mut Vec<Option<VertexBuffer>>,
    slot: usize,
    data: &[u8],
) {
    if buffers.len() <= slot {
        buffers.resize_with(slot + 1, || None);
    }
    let needed = data.len() as u64;
    let needs_buffer = buffers[slot]
        .as_ref()
        .is_none_or(|buffer| buffer.size < needed);
    if needs_buffer {
        let size = needed.max(MIN_VERTEX_BYTES).next_power_of_two();
        buffers[slot] = Some(VertexBuffer {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("illness-gpu-text-vertices"),
                size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            size,
        });
    }
    if let Some(Some(buffer)) = buffers.get(slot)
        && !data.is_empty()
    {
        queue.write_buffer(&buffer.buffer, 0, data);
    }
}

/// Mask blending: premultiplied `fg * coverage` over the destination.
fn mask_blend_state() -> wgpu::BlendState {
    wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
}

struct Resources {
    bind_group: wgpu::BindGroup,
    bind_group_layout: wgpu::BindGroupLayout,
    mask_pipeline: wgpu::RenderPipeline,
    solid_pipeline: wgpu::RenderPipeline,
    color_pipeline: wgpu::RenderPipeline,
    texture: wgpu::Texture,
    generation: u64,
}

impl Resources {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, generation: u64) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("illness-gpu-text-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("illness-gpu-text-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("illness-gpu-text-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let vertex_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: VERTEX_SIZE as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 8,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 2,
                },
            ],
        })];

        let mask_pipeline = create_pipeline(
            device,
            format,
            &pipeline_layout,
            &shader,
            &vertex_buffers,
            "fs_mask",
            mask_blend_state(),
            "illness-gpu-text-mask-pipeline",
        );
        let color_pipeline = create_pipeline(
            device,
            format,
            &pipeline_layout,
            &shader,
            &vertex_buffers,
            "fs_color",
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            "illness-gpu-text-color-pipeline",
        );
        let solid_pipeline = create_pipeline(
            device,
            format,
            &pipeline_layout,
            &shader,
            &vertex_buffers,
            "fs_solid",
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            "illness-gpu-text-solid-pipeline",
        );

        let texture = create_atlas_texture(device);
        let sampler = create_sampler(device);
        let bind_group = create_bind_group(device, &bind_group_layout, &texture, &sampler);

        Self {
            bind_group,
            bind_group_layout,
            mask_pipeline,
            solid_pipeline,
            color_pipeline,
            texture,
            generation,
        }
    }

    /// Replaces the atlas with a zeroed texture after the cache was reset.
    fn rebuild_atlas(&mut self, device: &wgpu::Device, generation: u64) {
        let texture = create_atlas_texture(device);
        let sampler = create_sampler(device);
        self.bind_group = create_bind_group(device, &self.bind_group_layout, &texture, &sampler);
        self.texture = texture;
        self.generation = generation;
    }

    fn upload(&self, queue: &wgpu::Queue, upload: &Upload) {
        if upload.width == 0 || upload.height == 0 || upload.data.is_empty() {
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: upload.x,
                    y: upload.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &upload.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(upload.width * 4),
                rows_per_image: Some(upload.height),
            },
            wgpu::Extent3d {
                width: upload.width,
                height: upload.height,
                depth_or_array_layers: 1,
            },
        );
    }
}

fn create_atlas_texture(device: &wgpu::Device) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("illness-gpu-text-atlas"),
        size: wgpu::Extent3d {
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn create_sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("illness-gpu-text-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    })
}

fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    texture: &wgpu::Texture,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("illness-gpu-text-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

#[allow(clippy::too_many_arguments)]
fn create_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    vertex_buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
    fragment_entry: &str,
    blend: wgpu::BlendState,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: vertex_buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Rasterized quads for a small snapshot, without a wgpu device. Used by the
/// vertex/atlas tests.
#[cfg(test)]
pub(crate) fn test_vertices(
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
) -> (Vec<u8>, u32, u32) {
    let (vertices, counts) = test_layers(atlas, snapshot, spec, false);
    (vertices, counts[LAYER_MASK], counts[LAYER_COLOR])
}

#[cfg(test)]
fn test_layers(
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    fills: bool,
) -> (Vec<u8>, [u32; LAYER_COUNT]) {
    let surface_size = soft::surface_size(spec.rect, spec.ppp).expect("surface size");
    let surface_size = [surface_size[0] as u32, surface_size[1] as u32];
    let mut cache = GlyphCache::new();
    cache.begin_frame(atlas.metrics(), spec.ppp);
    let metrics = atlas.metrics();
    let mut builder = VertexBuilder::new(surface_size, 255, std::array::from_fn(|_| Vec::new()));
    if fills {
        build_fills(&mut builder, snapshot, spec, metrics);
    }
    build_vertices(&mut builder, &mut cache, atlas, snapshot, spec);
    let (counts, layers) = builder.finish();
    let mut vertices = Vec::new();
    for layer in &layers {
        vertices.extend_from_slice(layer);
    }
    (vertices, counts)
}

#[cfg(test)]
mod tests {
    use egui::{pos2, vec2};
    use illness_core::Theme;
    use illness_term::Snapshot;

    use super::*;
    use crate::design::Tokens;
    use crate::fonts::CASCADIA_MONO;

    fn spec<'a>(rect: Rect, theme: &'a Theme, tokens: &'a Tokens) -> SurfaceSpec<'a> {
        SurfaceSpec {
            cols: 4,
            rows: 2,
            rect,
            ppp: 1.0,
            padding: 0.0,
            material: illness_core::Material::Solid,
            show_cursor: false,
            search: None,
            text: true,
            fills: true,
            theme,
            tokens,
            font_size: 14.0,
            line_height: 1.2,
        }
    }

    fn quad_positions(vertices: &[u8]) -> Vec<[f32; 2]> {
        vertices
            .as_chunks::<VERTEX_SIZE>()
            .0
            .iter()
            .map(|vertex| {
                [
                    f32::from_le_bytes(vertex[0..4].try_into().expect("x")),
                    f32::from_le_bytes(vertex[4..8].try_into().expect("y")),
                ]
            })
            .collect()
    }

    #[test]
    fn mask_pipeline_uses_grayscale_premultiplied_alpha() {
        let blend = mask_blend_state();
        assert_eq!(blend, wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        assert_eq!(blend.color.src_factor, wgpu::BlendFactor::One);
        assert_eq!(blend.color.dst_factor, wgpu::BlendFactor::OneMinusSrcAlpha);
        assert!(SHADER.contains("textureSample(atlas, atlas_sampler, in.uv).r"));
        assert!(!SHADER.contains("dual_source_blending"));
        assert!(!SHADER.contains("@blend_src"));
        assert!(!SHADER.contains("Src1"));
    }

    /// Mask tiles must hit the atlas as neutral coverage with an opaque alpha:
    /// equal RGB channels for every texel, so the shader's `.r` sample cannot
    /// reintroduce chroma fringing.
    #[test]
    fn mask_uploads_store_equal_channels() {
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 18.0, 1.0);
        let mut cache = GlyphCache::new();
        cache.begin_frame(atlas.metrics(), 1.0);
        let slot = cache
            .slot(&mut atlas, 'i', GlyphStyle::Regular)
            .expect("packed i");
        assert!(!slot.color);
        let upload = cache.uploads.last().expect("mask upload");
        assert_eq!(
            upload.data.len(),
            slot.width as usize * slot.height as usize * 4
        );
        for pixel in upload.data.as_chunks::<4>().0 {
            assert_eq!(pixel[1], pixel[0], "mask green differs from red");
            assert_eq!(pixel[2], pixel[0], "mask blue differs from red");
            assert_eq!(pixel[3], 255, "mask texel alpha must be opaque");
        }
    }

    /// The 1-texel gutter between shelf tiles keeps nearest sampling from
    /// reading a neighbor when a quad is drawn at a rounded edge.
    #[test]
    fn shelf_leaves_a_gutter_between_tiles() {
        let mut shelf = Shelf::default();
        let (ax, ay) = shelf.allocate(9, 12).expect("first tile");
        let (bx, by) = shelf.allocate(7, 12).expect("second tile");
        assert_eq!(ay, 0);
        assert_eq!(by, 0);
        assert!(bx > ax + 9, "tiles at {ax} and {bx} touch");

        let mut rows = Shelf::default();
        let (_, first_y) = rows.allocate(ATLAS_SIZE - 2, 12).expect("wide tile");
        let (_, second_y) = rows.allocate(8, 9).expect("wrapped tile");
        assert!(
            second_y > first_y + 12,
            "rows at {first_y} and {second_y} touch"
        );
    }

    #[test]
    fn vertex_stream_reuses_unchanged_frames() {
        let mut stream = VertexStream::default();
        let mut builds = 0;
        let rebuilt = stream.update(7, 1, [64, 64], 255, |builder| {
            builds += 1;
            builder.push_rect(0, 0, 8, 8, Color32::WHITE, LAYER_FILLS);
        });
        assert!(rebuilt);
        assert_eq!(builds, 1);
        let first_len = stream.vertices.len();
        let first_slot = stream.slot;

        let rebuilt = stream.update(7, 1, [64, 64], 255, |_| builds += 1);
        assert!(!rebuilt, "same fingerprint must reuse the vertex stream");
        assert_eq!(builds, 1);
        assert_eq!(stream.vertices.len(), first_len);
        assert_eq!(stream.slot, first_slot);

        let rebuilt = stream.update(8, 1, [64, 64], 255, |_| builds += 1);
        assert!(rebuilt);
        assert_eq!(builds, 2);
        assert_ne!(stream.slot, first_slot, "rebuilds rotate the buffer ring");

        // An atlas generation change invalidates the stream even when the
        // snapshot is identical.
        assert!(stream.update(8, 2, [64, 64], 255, |_| builds += 1));
        assert_eq!(builds, 3);
    }

    #[test]
    fn atlas_overflow_falls_back_until_metrics_change() {
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 14.0, 1.0);
        let mut cache = GlyphCache::new();
        cache.begin_frame(atlas.metrics(), 1.0);
        cache.shelf.y = ATLAS_SIZE;

        assert!(cache.slot(&mut atlas, 'A', GlyphStyle::Regular).is_none());
        assert!(cache.overflowed);
        assert!(
            cache.slot(&mut atlas, 'B', GlyphStyle::Regular).is_none(),
            "an overflowed cache must not pretend later glyphs fit"
        );

        // Same metrics: stay overflowed instead of resetting/logging per frame.
        cache.begin_frame(atlas.metrics(), 1.0);
        assert!(cache.overflowed);
        assert!(cache.slot(&mut atlas, 'C', GlyphStyle::Regular).is_none());

        // New metrics clear the overflow and repack from scratch.
        let mut metrics = atlas.metrics();
        metrics.height_px += 1;
        cache.begin_frame(metrics, 1.0);
        assert!(!cache.overflowed);
        assert!(cache.slot(&mut atlas, 'A', GlyphStyle::Regular).is_some());
    }

    #[test]
    fn text_produces_one_quad_per_glyph_with_pixel_uvs() {
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 14.0, 1.0);
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let metrics = atlas.metrics();
        let rect = Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(
                (metrics.width_px * 4) as f32,
                (metrics.height_px * 2) as f32,
            ),
        );
        let spec = spec(rect, theme, &tokens);

        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cells[0].ch = 'A';
        snapshot.cells[1].ch = 'A';

        let (vertices, mask, color) = test_vertices(&mut atlas, &snapshot, &spec);
        assert_eq!(mask, 12, "two glyphs should produce two quads");
        assert_eq!(color, 0);
        assert_eq!(vertices.len(), 12 * VERTEX_SIZE);

        let positions = quad_positions(&vertices);
        for position in &positions {
            assert!((-1.0..=1.0).contains(&position[0]));
            assert!((-1.0..=1.0).contains(&position[1]));
        }

        // The second glyph starts exactly one cell to the right of the first.
        let cell_px = 2.0 * metrics.width_px as f32 / surface_width(&spec);
        let first = positions[0];
        let second = positions[6];
        assert!((second[0] - first[0] - cell_px).abs() < 0.01);

        let uvs: Vec<[f32; 2]> = vertices
            .as_chunks::<VERTEX_SIZE>()
            .0
            .iter()
            .map(|vertex| {
                [
                    f32::from_le_bytes(vertex[8..12].try_into().expect("u")),
                    f32::from_le_bytes(vertex[12..16].try_into().expect("v")),
                ]
            })
            .collect();
        let u0 = uvs[0][0];
        let u1 = uvs[1][0];
        assert!(u1 > u0, "glyph UVs must advance left to right");
        assert!((u1 - u0) * ATLAS_SIZE as f32 >= 1.0);
    }

    fn surface_width(spec: &SurfaceSpec<'_>) -> f32 {
        soft::surface_size(spec.rect, spec.ppp).expect("size")[0] as f32
    }

    #[test]
    fn empty_cells_emit_no_geometry() {
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 14.0, 1.0);
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let metrics = atlas.metrics();
        let rect = Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(
                (metrics.width_px * 4) as f32,
                (metrics.height_px * 2) as f32,
            ),
        );
        let spec = spec(rect, theme, &tokens);
        let (vertices, mask, color) = test_vertices(&mut atlas, &Snapshot::empty(4, 2), &spec);
        assert!(vertices.is_empty());
        assert_eq!(mask, 0);
        assert_eq!(color, 0);
    }

    #[test]
    fn selection_search_and_cursor_are_gpu_fills() {
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 14.0, 1.0);
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let metrics = atlas.metrics();
        let rect = Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(
                (metrics.width_px * 4) as f32,
                (metrics.height_px * 2) as f32,
            ),
        );
        let mut spec = spec(rect, theme, &tokens);
        spec.show_cursor = true;
        let search = crate::terminal::SearchState {
            query: "AA".to_string(),
            case_sensitive: true,
        };
        spec.search = Some(&search);

        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cells[0].ch = 'A';
        snapshot.cells[1].ch = 'A';
        snapshot.cursor.visible = true;
        snapshot.cursor.kind = CursorKind::Block;
        snapshot.selection = vec![illness_term::SelectionSpan {
            start: (0, 0),
            end: (0, 1),
        }];

        let (vertices, counts) = test_layers(&mut atlas, &snapshot, &spec, true);
        assert_eq!(
            counts[LAYER_FILLS], 18,
            "selection, search, and block cursor fills should be solid quads"
        );
        assert_eq!(counts[LAYER_MASK], 18);
        assert_eq!(counts[LAYER_CURSOR], 0);
        assert_eq!(
            vertices.len(),
            counts.iter().sum::<u32>() as usize * VERTEX_SIZE
        );
    }

    #[test]
    fn color_glyphs_use_the_color_pipeline() {
        if !emoji_font_present() {
            eprintln!("skipping GPU color-glyph assertion: no emoji system font");
            return;
        }
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 14.0, 1.0);
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let metrics = atlas.metrics();
        let rect = Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(
                (metrics.width_px * 4) as f32,
                (metrics.height_px * 2) as f32,
            ),
        );
        let spec = spec(rect, theme, &tokens);
        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cells[0].ch = '\u{1f600}';

        let (vertices, mask, color) = test_vertices(&mut atlas, &snapshot, &spec);
        assert_eq!(mask, 0, "emoji must not enter the mask pipeline");
        assert_eq!(color, 6, "emoji must be one quad in the color pipeline");
        assert_eq!(vertices.len(), 6 * VERTEX_SIZE);
    }

    #[test]
    fn cursor_block_uses_background_foreground() {
        let ctx = egui::Context::default();
        let mut atlas = GlyphAtlas::new(&ctx, CASCADIA_MONO, 14.0, 1.0);
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let metrics = atlas.metrics();
        let rect = Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(
                (metrics.width_px * 4) as f32,
                (metrics.height_px * 2) as f32,
            ),
        );
        let mut spec = spec(rect, theme, &tokens);
        spec.show_cursor = true;

        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cells[0].ch = 'A';
        snapshot.cursor.visible = true;
        snapshot.cursor.col = 0;
        snapshot.cursor.row = 0;

        let (vertices, mask, _) = test_vertices(&mut atlas, &snapshot, &spec);
        assert_eq!(mask, 12);
        let bg = design::color(theme.bg);
        let color = [
            f32::from(bg.r()) / 255.0,
            f32::from(bg.g()) / 255.0,
            f32::from(bg.b()) / 255.0,
            1.0,
        ];
        // Grid glyphs come first; the block cursor's inverted glyph is last.
        let last = vertices.len() - 6 * VERTEX_SIZE;
        let first_color = [
            f32::from_le_bytes(vertices[last + 16..last + 20].try_into().expect("r")),
            f32::from_le_bytes(vertices[last + 20..last + 24].try_into().expect("g")),
            f32::from_le_bytes(vertices[last + 24..last + 28].try_into().expect("b")),
            f32::from_le_bytes(vertices[last + 28..last + 32].try_into().expect("a")),
        ];
        assert_eq!(first_color, color);
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
