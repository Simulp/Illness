use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2, Visuals};
use illness_core::{Material, Rgb, Theme};

use crate::fonts;

/// Convert a terminal theme colour into an egui colour.
pub fn color(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// Alpha for chrome surfaces painted over the window material. Opaque for
/// Solid, and progressively more translucent for Mica and Acrylic so the
/// compositor backdrop actually shows through.
pub fn material_alpha(material: Material) -> u8 {
    match material {
        Material::Solid => 255,
        Material::Mica => 210,
        Material::Acrylic => 185,
    }
}

/// Mix `amount` of `other` into `base` (sRGB-space interpolation).
fn mix(base: Color32, other: Color32, amount: f32) -> Color32 {
    let channel =
        |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
    Color32::from_rgb(
        channel(base.r(), other.r()),
        channel(base.g(), other.g()),
        channel(base.b(), other.b()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub gap_xs: f32,
    pub gap_sm: f32,
    pub gap_md: f32,
    pub gap_lg: f32,

    pub radius_sm: f32,
    pub radius_md: f32,
    pub radius_lg: f32,

    pub titlebar_h: f32,
    pub tabbar_h: f32,
    pub caption_w: f32,

    pub font_small: f32,
    pub font_body: f32,
    pub font_title: f32,

    pub bg: Color32,
    pub bg_elev: Color32,
    pub bg_sunken: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub hover: Color32,
    pub danger: Color32,
    pub selection: Color32,

    // Aliases kept for the other UI modules and the skeleton's existing callers.
    pub bg_elevated: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,
    pub fg: Color32,
    pub fg_muted: Color32,
    pub accent_hover: Color32,
    pub warning: Color32,
    pub success: Color32,
    pub titlebar_height: f32,
    pub tab_height: f32,
    pub caption_width: f32,
    pub caption_height: f32,
}

impl Default for Tokens {
    fn default() -> Self {
        Self::from_theme(Theme::by_name("Illness Dark"))
    }
}

impl Tokens {
    pub fn from_theme(theme: &Theme) -> Self {
        let light = theme.is_light();
        let bg = color(theme.bg);
        let text = color(theme.fg);
        let accent = color(theme.accent);

        let bg_elev = mix(bg, Color32::WHITE, 0.06);
        let bg_sunken = mix(bg, Color32::BLACK, 0.04);
        let border = if light {
            mix(bg, Color32::BLACK, 0.10)
        } else {
            mix(bg, Color32::WHITE, 0.08)
        };
        let hover = if light {
            mix(bg, Color32::BLACK, 0.07)
        } else {
            mix(bg, Color32::WHITE, 0.07)
        };
        let text_dim = mix(bg, text, 0.60);
        let danger = Color32::from_rgb(0xE5, 0x53, 0x4B);

        Self {
            gap_xs: 4.0,
            gap_sm: 8.0,
            gap_md: 12.0,
            gap_lg: 20.0,

            radius_sm: 6.0,
            radius_md: 8.0,
            radius_lg: 12.0,

            titlebar_h: 40.0,
            tabbar_h: 34.0,
            caption_w: 48.0,

            font_small: 12.0,
            font_body: 13.5,
            font_title: 14.5,

            bg,
            bg_elev,
            bg_sunken,
            border,
            text,
            text_dim,
            accent,
            hover,
            danger,
            selection: mix(bg, accent, 0.30),

            bg_elevated: bg_elev,
            bg_hover: hover,
            bg_active: if light {
                mix(bg, Color32::BLACK, 0.12)
            } else {
                mix(bg, Color32::WHITE, 0.12)
            },
            fg: text,
            fg_muted: text_dim,
            accent_hover: mix(accent, Color32::WHITE, 0.15),
            warning: Color32::from_rgb(0xF0, 0xB2, 0x32),
            success: Color32::from_rgb(0x6C, 0xCB, 0x5F),
            titlebar_height: 40.0,
            tab_height: 34.0,
            caption_width: 48.0,
            caption_height: 40.0,
        }
    }

    /// WCAG contrast of primary text on the base background.
    pub fn contrast_ok(&self) -> bool {
        contrast_ratio(self.text, self.bg) >= 4.5
    }
}

/// WCAG 2.x relative luminance.
fn luminance(color: Color32) -> f32 {
    fn linear(channel: u8) -> f32 {
        let channel = f32::from(channel) / 255.0;
        if channel <= 0.040_45 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    0.212_6 * linear(color.r()) + 0.715_2 * linear(color.g()) + 0.072_2 * linear(color.b())
}

fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (lighter, darker) = {
        let (la, lb) = (luminance(a), luminance(b));
        if la >= lb { (la, lb) } else { (lb, la) }
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// A small status/accent dot, e.g. the profile colour on a tab.
pub fn dot(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    painter.circle_filled(center, radius, color);
}

/// Compact rounded label used for badges and hints.
pub fn pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let padding = Vec2::new(8.0, 4.0);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(11.0, FontFamily::Proportional),
        color,
    );
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + padding * 2.0, egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(255),
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40),
    );
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, color);
    response
}

fn style_widget(widget: &mut egui::style::WidgetVisuals, fill: Color32, tokens: &Tokens) {
    widget.bg_fill = fill;
    widget.weak_bg_fill = fill;
    widget.bg_stroke = Stroke::new(1.0, tokens.border);
    widget.fg_stroke = Stroke::new(1.0, tokens.text);
    widget.corner_radius = CornerRadius::same(tokens.radius_md as u8);
    widget.expansion = 0.0;
}

pub fn install(ctx: &egui::Context, theme: &Theme, material: Material) {
    fonts::install(ctx);

    let tokens = Tokens::from_theme(theme);
    let egui_theme = if theme.is_light() {
        egui::Theme::Light
    } else {
        egui::Theme::Dark
    };
    ctx.set_theme(egui_theme);

    let mut visuals = if theme.is_light() {
        Visuals::light()
    } else {
        Visuals::dark()
    };

    visuals.dark_mode = !theme.is_light();
    visuals.text_options.font_hinting = true;
    visuals.text_options.subpixel_binning = false;
    visuals.override_text_color = Some(tokens.text);
    visuals.panel_fill = if material == Material::Solid {
        tokens.bg
    } else {
        Color32::TRANSPARENT
    };
    visuals.window_fill = Color32::TRANSPARENT;
    visuals.window_stroke = Stroke::new(1.0, tokens.border);
    visuals.window_corner_radius = CornerRadius::same(tokens.radius_md as u8);
    visuals.menu_corner_radius = CornerRadius::same(tokens.radius_md as u8);
    visuals.extreme_bg_color = tokens.bg;
    visuals.faint_bg_color = tokens.bg_elev;
    visuals.code_bg_color = tokens.bg_sunken;
    visuals.text_edit_bg_color = Some(tokens.bg_sunken);
    visuals.selection.bg_fill = tokens.selection;
    visuals.selection.stroke = Stroke::new(1.0, tokens.text);
    visuals.hyperlink_color = tokens.accent;
    visuals.warn_fg_color = Color32::from_rgb(0xF0, 0xB2, 0x32);
    visuals.error_fg_color = tokens.danger;

    style_widget(&mut visuals.widgets.noninteractive, tokens.bg_elev, &tokens);
    style_widget(&mut visuals.widgets.inactive, tokens.bg_elev, &tokens);
    style_widget(&mut visuals.widgets.hovered, tokens.hover, &tokens);
    style_widget(&mut visuals.widgets.active, tokens.bg_sunken, &tokens);
    style_widget(&mut visuals.widgets.open, tokens.hover, &tokens);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, tokens.border);

    ctx.set_visuals(visuals);

    ctx.style_mut_of(egui_theme, |style| {
        style.spacing.item_spacing = Vec2::new(tokens.gap_sm, 6.0);
        style.spacing.button_padding = Vec2::new(tokens.gap_sm, 5.0);
        style.spacing.interact_size = Vec2::new(28.0, 26.0);
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.handle_min_length = 32.0;
        style.text_styles.insert(
            TextStyle::Body,
            FontId::new(tokens.font_body, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(tokens.font_body, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(tokens.font_small, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(tokens.font_title, FontFamily::Proportional),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dark_contrast() {
        let tokens = Tokens::from_theme(Theme::by_name("illness-dark"));
        assert!(
            contrast_ratio(tokens.text, tokens.bg) >= 7.0,
            "dark contrast: {}",
            contrast_ratio(tokens.text, tokens.bg)
        );
        assert!(tokens.contrast_ok());
    }

    #[test]
    fn test_light_contrast() {
        let tokens = Tokens::from_theme(Theme::by_name("light"));
        assert!(
            contrast_ratio(tokens.text, tokens.bg) >= 7.0,
            "light contrast: {}",
            contrast_ratio(tokens.text, tokens.bg)
        );
        assert!(tokens.contrast_ok());
    }

    #[test]
    fn test_tokens_radius() {
        let tokens = Tokens::from_theme(Theme::by_name("illness-dark"));
        assert_eq!(tokens.radius_md, 8.0);
        assert_eq!(tokens.gap_xs, 4.0);
        assert_eq!(tokens.titlebar_h, 40.0);
        assert_eq!(tokens.tabbar_h, 34.0);
        assert_eq!(tokens.caption_w, 48.0);
    }

    #[test]
    fn test_tokens_accent_and_danger() {
        let tokens = Tokens::from_theme(Theme::by_name("nord"));
        assert_eq!(tokens.accent, Color32::from_rgb(0x88, 0xC0, 0xD0));
        assert_eq!(tokens.danger, Color32::from_rgb(0xE5, 0x53, 0x4B));
    }

    #[test]
    fn material_alpha_gets_more_translucent() {
        assert_eq!(material_alpha(Material::Solid), 255);
        assert_eq!(material_alpha(Material::Mica), 210);
        assert_eq!(material_alpha(Material::Acrylic), 185);
    }
}
