//! Per-channel software compositor for the terminal grid.
//!
//! The whole terminal surface (backgrounds, selection, search highlights,
//! text, decorations, and cursor) is composed at physical-pixel resolution
//! into one premultiplied RGBA image. Mask glyphs contribute three channels
//! of linear coverage, so each RGB channel is blended independently: true
//! ClearType-style subpixel antialiasing instead of a grayscale mesh.
//!
//! After composition the image is premultiplied by a uniform material alpha
//! (Mica/Acrylic), which keeps the subpixel fringes intact while letting the
//! window backdrop show through.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use egui::{Color32, ColorImage, Rect};
use illness_core::{CursorKind, Material, Theme};
use illness_term::Snapshot;
use illness_term::snapshot::CellFlags;

use crate::design::{self, Tokens};
use crate::glyph::{GlyphAtlas, SoftGlyph};
use crate::terminal::{
    SearchState, cell_colors, dim, find_matches, glyph_style, row_text, selection_ranges,
};

/// Refuse allocations that would exceed common GPU texture limits; the
/// caller falls back to the mesh path.
const MAX_SURFACE_DIMENSION: f32 = 8_192.0;
const MAX_SURFACE_PIXELS: usize = 36_000_000;
/// Alpha used for search highlights, matching the mesh path.
const SEARCH_ALPHA: u8 = 96;

/// Everything needed to render or fingerprint one terminal surface.
#[derive(Clone, Copy)]
pub struct SurfaceSpec<'a> {
    pub cols: usize,
    pub rows: usize,
    /// Full terminal rectangle, in logical points, including padding.
    pub rect: Rect,
    pub ppp: f32,
    pub padding: f32,
    pub material: Material,
    pub show_cursor: bool,
    pub search: Option<&'a SearchState>,
    /// Paint glyph pixels. `false` keeps backgrounds, decorations, and the
    /// cursor fill so a GPU pass can draw the glyph quads on top.
    pub text: bool,
    /// Paint selection, search, and cursor fills. `false` leaves them to the
    /// GPU quad pass, so changes there do not invalidate this surface.
    pub fills: bool,
    pub theme: &'a Theme,
    pub tokens: &'a Tokens,
    pub font_size: f32,
    pub line_height: f32,
}

/// Align a logical rect to the physical pixel grid.
///
/// The compositor works in whole physical pixels. Painting its texture at a
/// fractional physical position would resample it even with `NEAREST`, which
/// softens hinted ClearType glyphs, so snap the destination before layout and
/// rendering.
pub fn snap_rect(rect: Rect, ppp: f32) -> Rect {
    if !ppp.is_finite() || ppp <= 0.0 {
        return rect;
    }
    let snap = |value: f32| (value * ppp).round() / ppp;
    Rect::from_min_max(
        egui::pos2(snap(rect.min.x), snap(rect.min.y)),
        egui::pos2(snap(rect.max.x), snap(rect.max.y)),
    )
}

/// Physical pixel size of the surface, or `None` when the rect is unusable.
pub fn surface_size(rect: Rect, ppp: f32) -> Option<[usize; 2]> {
    if !ppp.is_finite() || ppp <= 0.0 || !rect.width().is_finite() || !rect.height().is_finite() {
        return None;
    }
    let width = (rect.width() * ppp).round();
    let height = (rect.height() * ppp).round();
    if width < 1.0
        || height < 1.0
        || width > MAX_SURFACE_DIMENSION
        || height > MAX_SURFACE_DIMENSION
        || width as usize * height as usize > MAX_SURFACE_PIXELS
    {
        return None;
    }
    Some([width as usize, height as usize])
}

/// Composes the complete terminal surface for one frame.
pub fn render_surface(
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
) -> Option<ColorImage> {
    let [width, height] = surface_size(spec.rect, spec.ppp)?;
    if snapshot.cells.is_empty() {
        return None;
    }

    let metrics = atlas.metrics();
    let cell_width = metrics.width_px as i32;
    let cell_height = metrics.height_px as i32;
    if cell_width <= 0 || cell_height <= 0 {
        return None;
    }

    let base = design::color(spec.theme.bg);
    let mut surface = Surface::new(width, height, rgb(base));
    let origin_x = (spec.padding.max(0.0) * spec.ppp).round() as i32;
    let origin_y = origin_x;
    let baseline = (metrics.baseline * spec.ppp).round() as i32;

    paint_backgrounds(
        &mut surface,
        snapshot,
        spec,
        base,
        origin_x,
        origin_y,
        cell_width,
        cell_height,
    );
    if spec.fills {
        paint_selection(
            &mut surface,
            snapshot,
            spec,
            origin_x,
            origin_y,
            cell_width,
            cell_height,
        );
        paint_search(
            &mut surface,
            snapshot,
            spec,
            origin_x,
            origin_y,
            cell_width,
            cell_height,
        );
    }
    paint_text(
        &mut surface,
        atlas,
        snapshot,
        spec,
        origin_x,
        origin_y,
        baseline,
        cell_width,
        cell_height,
    );
    if spec.fills && spec.show_cursor {
        paint_cursor(
            &mut surface,
            atlas,
            snapshot,
            spec,
            origin_x,
            origin_y,
            baseline,
            cell_width,
            cell_height,
        );
    }
    Some(surface.finish(design::material_alpha(spec.material)))
}

/// Cheap change key for the cached surface.
///
/// Covers every input that changes pixels: cell contents/colors/flags,
/// selection, search, cursor and blink state, theme/tokens, font geometry,
/// material, DPI, and the physical buffer size.
///
/// When `fills` is `false`, selection/search/cursor pixels are drawn by the
/// GPU quad pass and therefore excluded. When `text` is also `false`, glyph
/// characters cannot change this surface, so typing does not force a
/// re-composite of the background layer.
pub fn fingerprint(snapshot: &Snapshot, spec: &SurfaceSpec<'_>) -> u64 {
    let mut hasher = DefaultHasher::new();

    surface_size(spec.rect, spec.ppp).hash(&mut hasher);
    spec.cols.hash(&mut hasher);
    spec.rows.hash(&mut hasher);
    spec.ppp.to_bits().hash(&mut hasher);
    spec.padding.to_bits().hash(&mut hasher);
    spec.font_size.to_bits().hash(&mut hasher);
    spec.line_height.to_bits().hash(&mut hasher);
    material_tag(spec.material).hash(&mut hasher);
    spec.text.hash(&mut hasher);
    spec.fills.hash(&mut hasher);

    if spec.fills {
        spec.show_cursor.hash(&mut hasher);
        match spec.search {
            Some(search) => {
                true.hash(&mut hasher);
                search.query.hash(&mut hasher);
                search.case_sensitive.hash(&mut hasher);
            }
            None => false.hash(&mut hasher),
        }
    }

    for color in [
        spec.theme.bg,
        spec.theme.fg,
        spec.theme.cursor,
        spec.theme.accent,
    ] {
        hash_rgb(&mut hasher, color);
    }
    for color in &spec.theme.ansi {
        hash_rgb(&mut hasher, *color);
    }
    hash_color32(&mut hasher, spec.tokens.selection);
    hash_color32(&mut hasher, spec.tokens.accent);

    // Glyph characters only affect this surface through painted text and
    // search matches. Backgrounds and decorations depend on resolved colors
    // and a small flag subset, so plain output scrolling does not re-composite
    // the layer.
    let hashes_glyphs = spec.text || spec.fills;
    let mut soft_flags =
        CellFlags::ALL_UNDERLINES | CellFlags::STRIKEOUT | CellFlags::HIDDEN | CellFlags::WIDE_CHAR;
    if hashes_glyphs {
        soft_flags = soft_flags | CellFlags::BOLD | CellFlags::ITALIC;
    }
    snapshot.cells.len().hash(&mut hasher);
    for cell in &snapshot.cells {
        if hashes_glyphs {
            cell.ch.hash(&mut hasher);
            cell.combining_len.hash(&mut hasher);
            for ch in snapshot.zero_width(cell) {
                ch.hash(&mut hasher);
            }
        }
        // Hash resolved colors, not the enum representation: a default-bg
        // space and an explicitly painted background color produce identical
        // pixels and must not invalidate the surface.
        let (fg, bg) = cell_colors(cell, spec.theme);
        hash_color32(&mut hasher, bg);
        let flags = CellFlags::from_bits_truncate(cell.flags.bits() & soft_flags.bits());
        flags.bits().hash(&mut hasher);
        let decorated = flags.intersects(CellFlags::ALL_UNDERLINES | CellFlags::STRIKEOUT)
            && !flags.contains(CellFlags::HIDDEN);
        if hashes_glyphs || decorated {
            let fg = if cell.flags.contains(CellFlags::DIM) {
                dim(fg)
            } else {
                fg
            };
            hash_color32(&mut hasher, fg);
        }
    }

    if spec.fills {
        snapshot.selection.len().hash(&mut hasher);
        for span in &snapshot.selection {
            span.start.hash(&mut hasher);
            span.end.hash(&mut hasher);
        }

        snapshot.cursor.col.hash(&mut hasher);
        snapshot.cursor.row.hash(&mut hasher);
        cursor_tag(snapshot.cursor.kind).hash(&mut hasher);
        snapshot.cursor.blinking.hash(&mut hasher);
        snapshot.cursor.visible.hash(&mut hasher);
    }

    hasher.finish()
}

/// Row bands whose pixels differ between two surfaces of the same size.
///
/// Consecutive dirty rows are merged so one partial texture update covers a
/// block of changed lines instead of one upload per row. A size change always
/// requires a full upload.
pub fn dirty_bands(previous: &ColorImage, next: &ColorImage) -> Vec<(usize, usize)> {
    if previous.size != next.size {
        return vec![(0, next.size[1])];
    }
    let width = next.size[0];
    let mut bands = Vec::new();
    let mut band_start = None;
    for row in 0..next.size[1] {
        let start = row * width;
        let end = start + width;
        let dirty = previous.pixels[start..end] != next.pixels[start..end];
        match (dirty, band_start) {
            (true, None) => band_start = Some(row),
            (false, Some(start)) => {
                bands.push((start, row - start));
                band_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = band_start {
        bands.push((start, next.size[1] - start));
    }
    bands
}

fn material_tag(material: Material) -> u8 {
    match material {
        Material::Solid => 0,
        Material::Mica => 1,
        Material::Acrylic => 2,
    }
}

fn cursor_tag(kind: CursorKind) -> u8 {
    match kind {
        CursorKind::Block => 0,
        CursorKind::Beam => 1,
        CursorKind::Underline => 2,
    }
}

fn hash_rgb<H: Hasher>(hasher: &mut H, color: illness_core::Rgb) {
    color.r.hash(hasher);
    color.g.hash(hasher);
    color.b.hash(hasher);
}

fn hash_color32<H: Hasher>(hasher: &mut H, color: Color32) {
    color.to_array().hash(hasher);
}

fn rgb(color: Color32) -> [u8; 3] {
    [color.r(), color.g(), color.b()]
}

#[allow(clippy::too_many_arguments)]
fn paint_backgrounds(
    surface: &mut Surface,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    base: Color32,
    origin_x: i32,
    origin_y: i32,
    cell_width: i32,
    cell_height: i32,
) {
    for row in 0..spec.rows {
        for col in 0..spec.cols {
            let bg = snapshot
                .cell(col, row)
                .map(|cell| cell_colors(cell, spec.theme).1)
                .unwrap_or(base);
            if bg != base {
                let x0 = origin_x + col as i32 * cell_width;
                let y0 = origin_y + row as i32 * cell_height;
                surface.fill_rect(x0, y0, x0 + cell_width, y0 + cell_height, rgb(bg));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_selection(
    surface: &mut Surface,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    origin_x: i32,
    origin_y: i32,
    cell_width: i32,
    cell_height: i32,
) {
    let color = rgb(spec.tokens.selection);
    for (row, start, end) in selection_ranges(snapshot, spec.cols, spec.rows) {
        if start >= end {
            continue;
        }
        let x0 = origin_x + start as i32 * cell_width;
        let x1 = origin_x + end as i32 * cell_width;
        let y0 = origin_y + row as i32 * cell_height;
        surface.fill_rect(x0, y0, x1, y0 + cell_height, color);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_search(
    surface: &mut Surface,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    origin_x: i32,
    origin_y: i32,
    cell_width: i32,
    cell_height: i32,
) {
    let Some(search) = spec.search else {
        return;
    };
    if search.query.is_empty() {
        return;
    }
    let color = rgb(spec.tokens.accent);
    for row in 0..spec.rows {
        let text = row_text(snapshot, row, spec.cols);
        for (start, end) in find_matches(&text, &search.query, search.case_sensitive) {
            if start >= end {
                continue;
            }
            let x0 = origin_x + start as i32 * cell_width;
            let x1 = origin_x + end as i32 * cell_width;
            let y0 = origin_y + row as i32 * cell_height;
            surface.blend_rect(x0, y0, x1, y0 + cell_height, color, SEARCH_ALPHA);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_text(
    surface: &mut Surface,
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    origin_x: i32,
    origin_y: i32,
    baseline: i32,
    cell_width: i32,
    cell_height: i32,
) {
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
                rgb(dim(fg))
            } else {
                rgb(fg)
            };
            let x0 = origin_x + col as i32 * cell_width;
            let y0 = origin_y + row as i32 * cell_height;
            let x1 = if flags.contains(CellFlags::WIDE_CHAR) {
                x0 + cell_width * 2
            } else {
                x0 + cell_width
            };
            let y1 = y0 + cell_height;

            if cell.ch != ' '
                && spec.text
                && let Some(glyph) = atlas.soft_glyph(cell.ch, glyph_style(flags))
                && glyph.width > 0
                && glyph.height > 0
            {
                let x = x0 + glyph.left;
                let y = y0 + baseline - glyph.top;
                if glyph.color {
                    surface.blend_rgba(x, y, &glyph);
                } else {
                    surface.blend_mask(x, y, &glyph, fg);
                }
            }

            // Combining marks share the base cell's origin and never advance.
            for &mark in snapshot.zero_width(cell) {
                if spec.text
                    && let Some(glyph) = atlas.soft_glyph(mark, glyph_style(flags))
                    && glyph.width > 0
                    && glyph.height > 0
                {
                    let x = x0 + glyph.left;
                    let y = y0 + baseline - glyph.top;
                    if glyph.color {
                        surface.blend_rgba(x, y, &glyph);
                    } else {
                        surface.blend_mask(x, y, &glyph, fg);
                    }
                }
            }

            if flags.intersects(CellFlags::ALL_UNDERLINES) {
                surface.fill_rect(x0, y1 - 1, x1, y1, fg);
            }
            if flags.contains(CellFlags::STRIKEOUT) {
                let y = y0 + cell_height / 2;
                surface.fill_rect(x0, y, x1, y + 1, fg);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cursor(
    surface: &mut Surface,
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    spec: &SurfaceSpec<'_>,
    origin_x: i32,
    origin_y: i32,
    baseline: i32,
    cell_width: i32,
    cell_height: i32,
) {
    if spec.cols == 0 || spec.rows == 0 {
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

    let x0 = origin_x + col as i32 * cell_width;
    let y0 = origin_y + row as i32 * cell_height;
    let x1 = if wide {
        x0 + cell_width * 2
    } else {
        x0 + cell_width
    };
    let y1 = y0 + cell_height;
    let color = rgb(design::color(spec.theme.cursor));

    match snapshot.cursor.kind {
        CursorKind::Block => {
            surface.fill_rect(x0, y0, x1, y1, color);
            if let Some(cell) = snapshot.cell(col, row) {
                let flags = cell.flags;
                if !flags.intersects(
                    CellFlags::HIDDEN
                        | CellFlags::WIDE_CHAR_SPACER
                        | CellFlags::LEADING_WIDE_CHAR_SPACER,
                ) && cell.ch != ' '
                    && spec.text
                    && let Some(glyph) = atlas.soft_glyph(cell.ch, glyph_style(flags))
                    && glyph.width > 0
                    && glyph.height > 0
                {
                    let x = x0 + glyph.left;
                    let y = y0 + baseline - glyph.top;
                    if glyph.color {
                        surface.blend_rgba(x, y, &glyph);
                    } else {
                        surface.blend_mask(x, y, &glyph, rgb(design::color(spec.theme.bg)));
                    }
                }
                for &mark in snapshot.zero_width(cell) {
                    if spec.text
                        && let Some(glyph) = atlas.soft_glyph(mark, glyph_style(flags))
                        && glyph.width > 0
                        && glyph.height > 0
                    {
                        let x = x0 + glyph.left;
                        let y = y0 + baseline - glyph.top;
                        if glyph.color {
                            surface.blend_rgba(x, y, &glyph);
                        } else {
                            surface.blend_mask(x, y, &glyph, rgb(design::color(spec.theme.bg)));
                        }
                    }
                }
            }
        }
        CursorKind::Beam => {
            let width = 2.min(cell_width).max(1);
            surface.fill_rect(x0, y0, x0 + width, y1, color);
        }
        CursorKind::Underline => {
            let thickness = 2.min(cell_height).max(1);
            surface.fill_rect(x0, y1 - thickness, x1, y1, color);
        }
    }
}

/// Intermediate surface: straight sRGB plus opaque alpha.
struct Surface {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl Surface {
    fn new(width: usize, height: usize, base: [u8; 3]) -> Self {
        let mut rgba = vec![255u8; width * height * 4];
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[0] = base[0];
            pixel[1] = base[1];
            pixel[2] = base[2];
        }
        Self {
            width,
            height,
            rgba,
        }
    }

    fn bounds(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> Option<(usize, usize, usize, usize)> {
        let x0 = x0.clamp(0, self.width as i32);
        let y0 = y0.clamp(0, self.height as i32);
        let x1 = x1.clamp(0, self.width as i32);
        let y1 = y1.clamp(0, self.height as i32);
        (x0 < x1 && y0 < y1).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 3]) {
        let Some((x0, y0, x1, y1)) = self.bounds(x0, y0, x1, y1) else {
            return;
        };
        for y in y0..y1 {
            let row = &mut self.rgba[y * self.width * 4..];
            for x in x0..x1 {
                let index = x * 4;
                row[index] = color[0];
                row[index + 1] = color[1];
                row[index + 2] = color[2];
            }
        }
    }

    fn blend_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 3], alpha: u8) {
        let Some((x0, y0, x1, y1)) = self.bounds(x0, y0, x1, y1) else {
            return;
        };
        let alpha = u32::from(alpha);
        let inverse = 255 - alpha;
        for y in y0..y1 {
            let row = &mut self.rgba[y * self.width * 4..];
            for x in x0..x1 {
                let index = x * 4;
                for channel in 0..3 {
                    let source = u32::from(color[channel]);
                    let destination = u32::from(row[index + channel]);
                    row[index + channel] =
                        ((source * alpha + destination * inverse + 127) / 255) as u8;
                }
            }
        }
    }

    fn blend_mask(&mut self, x: i32, y: i32, glyph: &SoftGlyph, fg: [u8; 3]) {
        let Some((x0, y0, x1, y1)) =
            self.bounds(x, y, x + glyph.width as i32, y + glyph.height as i32)
        else {
            return;
        };
        let stride = glyph.width as usize * 3;
        for pixel_y in y0..y1 {
            let source_row = (pixel_y as i32 - y) as usize * stride;
            let destination_row = pixel_y * self.width * 4;
            for pixel_x in x0..x1 {
                let source = source_row + (pixel_x as i32 - x) as usize * 3;
                let coverage = [
                    u32::from(glyph.data[source]),
                    u32::from(glyph.data[source + 1]),
                    u32::from(glyph.data[source + 2]),
                ];
                if coverage == [0, 0, 0] {
                    continue;
                }
                let destination = destination_row + pixel_x * 4;
                for channel in 0..3 {
                    let inverse = 255 - coverage[channel];
                    let blended = (u32::from(fg[channel]) * coverage[channel]
                        + u32::from(self.rgba[destination + channel]) * inverse
                        + 127)
                        / 255;
                    self.rgba[destination + channel] = blended as u8;
                }
            }
        }
    }

    fn blend_rgba(&mut self, x: i32, y: i32, glyph: &SoftGlyph) {
        let Some((x0, y0, x1, y1)) =
            self.bounds(x, y, x + glyph.width as i32, y + glyph.height as i32)
        else {
            return;
        };
        let stride = glyph.width as usize * 4;
        for pixel_y in y0..y1 {
            let source_row = (pixel_y as i32 - y) as usize * stride;
            let destination_row = pixel_y * self.width * 4;
            for pixel_x in x0..x1 {
                let source = source_row + (pixel_x as i32 - x) as usize * 4;
                let alpha = u32::from(glyph.data[source + 3]);
                if alpha == 0 {
                    continue;
                }
                let inverse = 255 - alpha;
                let destination = destination_row + pixel_x * 4;
                for channel in 0..3 {
                    let source_channel = u32::from(glyph.data[source + channel]);
                    let destination_channel = u32::from(self.rgba[destination + channel]);
                    let blended = source_channel + (destination_channel * inverse + 127) / 255;
                    self.rgba[destination + channel] = blended.min(255) as u8;
                }
            }
        }
    }

    fn finish(mut self, alpha: u8) -> ColorImage {
        let size = [self.width, self.height];
        if alpha != 255 {
            for pixel in self.rgba.as_chunks_mut::<4>().0 {
                pixel[0] = premultiply(pixel[0], alpha);
                pixel[1] = premultiply(pixel[1], alpha);
                pixel[2] = premultiply(pixel[2], alpha);
                pixel[3] = alpha;
            }
        }
        ColorImage::from_rgba_premultiplied(size, &self.rgba)
    }
}

fn premultiply(value: u8, alpha: u8) -> u8 {
    ((u16::from(value) * u16::from(alpha) + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use egui::{pos2, vec2};

    use super::*;
    use crate::fonts::CASCADIA_MONO;

    fn themed_spec<'a>(rect: Rect, theme: &'a Theme, tokens: &'a Tokens) -> SurfaceSpec<'a> {
        SurfaceSpec {
            cols: 4,
            rows: 2,
            rect,
            ppp: 1.0,
            padding: 0.0,
            material: Material::Mica,
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

    #[test]
    fn render_surface_composites_text_and_material_alpha() {
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
        let spec = themed_spec(rect, theme, &tokens);

        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cells[0].ch = 'A';
        snapshot.cells[1].ch = 'B';

        let image = render_surface(&mut atlas, &snapshot, &spec).expect("software surface");
        assert_eq!(
            image.size,
            [
                metrics.width_px as usize * 4,
                metrics.height_px as usize * 2
            ]
        );

        let alpha = design::material_alpha(spec.material);
        assert_eq!(alpha, design::material_alpha(Material::Mica));
        assert!(image.pixels.iter().all(|pixel| pixel.a() == alpha));

        let colors: HashSet<(u8, u8, u8)> = image
            .pixels
            .iter()
            .map(|pixel| (pixel.r(), pixel.g(), pixel.b()))
            .collect();
        assert!(
            colors.len() > 8,
            "expected text/background variety, got {} colors",
            colors.len()
        );
    }

    #[test]
    fn fingerprint_tracks_cell_edits() {
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(36.0, 36.0));
        let spec = themed_spec(rect, theme, &tokens);
        let mut snapshot = Snapshot::empty(4, 2);

        let before = fingerprint(&snapshot, &spec);
        assert_eq!(before, fingerprint(&snapshot, &spec));

        snapshot.cells[0].ch = 'A';
        let changed = fingerprint(&snapshot, &spec);
        assert_ne!(before, changed);

        snapshot.cells[0].ch = ' ';
        assert_eq!(before, fingerprint(&snapshot, &spec));

        snapshot.combining.push('\u{301}');
        snapshot.cells[0].combining_start = 0;
        snapshot.cells[0].combining_len = 1;
        let marked = fingerprint(&snapshot, &spec);
        assert_ne!(before, marked);
        assert_eq!(marked, fingerprint(&snapshot, &spec));
    }

    /// With GPU text and GPU fills, typing and selection changes must not
    /// invalidate the background surface.
    #[test]
    fn gpu_layer_fingerprint_ignores_glyphs_and_fills() {
        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(36.0, 36.0));
        let mut spec = themed_spec(rect, theme, &tokens);
        spec.text = false;
        spec.fills = false;
        let mut snapshot = Snapshot::empty(4, 2);

        let before = fingerprint(&snapshot, &spec);

        snapshot.cells[0].ch = 'A';
        assert_eq!(
            before,
            fingerprint(&snapshot, &spec),
            "glyph text alone must not re-composite backgrounds"
        );
        snapshot.combining.push('\u{301}');
        snapshot.cells[0].combining_start = 0;
        snapshot.cells[0].combining_len = 1;
        assert_eq!(before, fingerprint(&snapshot, &spec));

        snapshot.selection = vec![illness_term::SelectionSpan {
            start: (0, 0),
            end: (0, 3),
        }];
        assert_eq!(before, fingerprint(&snapshot, &spec));

        snapshot.cursor.visible = true;
        snapshot.cursor.col = 2;
        snapshot.cursor.kind = CursorKind::Beam;
        assert_eq!(before, fingerprint(&snapshot, &spec));

        let search = SearchState {
            query: "A".to_string(),
            case_sensitive: false,
        };
        spec.search = Some(&search);
        assert_eq!(before, fingerprint(&snapshot, &spec));

        // A cell background edit still invalidates the surface.
        snapshot.cells[1].bg = illness_term::Color::Indexed(3);
        assert_ne!(before, fingerprint(&snapshot, &spec));
    }

    #[test]
    fn dirty_bands_reports_changed_row_groups() {
        let base = ColorImage::filled([8, 4], Color32::BLACK);
        assert!(dirty_bands(&base, &base).is_empty());

        let mut changed = base.clone();
        changed.pixels[8 * 2 + 1] = Color32::WHITE;
        assert_eq!(dirty_bands(&base, &changed), vec![(2, 1)]);

        let mut block = base.clone();
        for row in 1..=2 {
            block.pixels[8 * row] = Color32::WHITE;
        }
        assert_eq!(dirty_bands(&base, &block), vec![(1, 2)]);

        let resized = ColorImage::filled([8, 6], Color32::BLACK);
        assert_eq!(dirty_bands(&base, &resized), vec![(0, 6)]);
    }

    /// The GPU path reuses this compositor for backgrounds and decorations.
    #[test]
    fn text_can_be_omitted_without_losing_decorations() {
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
        let mut spec = themed_spec(rect, theme, &tokens);
        spec.material = Material::Solid;

        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cells[0].ch = 'A';
        snapshot.cells[0].flags = CellFlags::UNDERLINE;

        let with_text = render_surface(&mut atlas, &snapshot, &spec).expect("text surface");
        spec.text = false;
        let without_text = render_surface(&mut atlas, &snapshot, &spec).expect("plain surface");

        assert_ne!(with_text.pixels, without_text.pixels);
        let fg = design::color(theme.fg);
        let underline_y = metrics.height_px as usize - 1;
        for x in 0..metrics.width_px as usize {
            let pixel = without_text.pixels[underline_y * without_text.size[0] + x];
            assert_eq!(
                (pixel.r(), pixel.g(), pixel.b()),
                (fg.r(), fg.g(), fg.b()),
                "underline should stay on the CPU surface"
            );
        }
    }

    #[test]
    fn material_alpha_matches_design() {
        assert_eq!(design::material_alpha(Material::Solid), 255);
        assert_eq!(design::material_alpha(Material::Mica), 210);
        assert_eq!(design::material_alpha(Material::Acrylic), 185);
    }

    #[test]
    fn snap_rect_lands_on_physical_pixels() {
        let rect = Rect::from_min_max(pos2(0.3, 1.1), pos2(100.7, 50.2));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let snapped = snap_rect(rect, ppp);
            for value in [snapped.min.x, snapped.min.y, snapped.max.x, snapped.max.y] {
                let physical = value * ppp;
                assert!(
                    (physical - physical.round()).abs() < 1e-3,
                    "{value} is not on the {ppp}x pixel grid"
                );
            }
        }
    }

    #[test]
    fn cursor_and_selection_reach_the_surface() {
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
        let mut spec = themed_spec(rect, theme, &tokens);
        spec.material = Material::Solid;
        spec.show_cursor = true;

        let mut snapshot = Snapshot::empty(4, 2);
        snapshot.cursor.visible = true;
        snapshot.cursor.kind = CursorKind::Block;
        snapshot.selection = vec![illness_term::SelectionSpan {
            start: (0, 0),
            end: (0, 3),
        }];

        let image = render_surface(&mut atlas, &snapshot, &spec).expect("software surface");
        assert!(image.pixels.iter().all(|pixel| pixel.a() == 255));
        let cursor = design::color(theme.cursor);
        assert!(image.pixels.iter().any(|pixel| {
            pixel.r() == cursor.r() && pixel.g() == cursor.g() && pixel.b() == cursor.b()
        }));
    }
}
