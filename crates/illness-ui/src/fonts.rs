use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontTweak};

pub const CASCADIA_MONO: &[u8] = include_bytes!("../../../assets/fonts/CascadiaMono.ttf");
pub const INTER: &[u8] = include_bytes!("../../../assets/fonts/InterVariable.ttf");
pub const INTER_ITALIC: &[u8] = include_bytes!("../../../assets/fonts/InterVariable-Italic.ttf");

/// Named font family for oblique UI text.
pub const FAMILY_INTER_ITALIC: &str = "inter-italic";
/// Named font family for Inter at weight 700.
pub const FAMILY_INTER_BOLD: &str = "inter-bold";

const INTER_KEY: &str = "illness-inter";
const INTER_BOLD_KEY: &str = "illness-inter-bold";
const INTER_ITALIC_KEY: &str = "illness-inter-italic";
const CASCADIA_KEY: &str = "illness-cascadia-mono";
const INSTALLED_ID: &str = "illness-fonts-installed";

/// Register bundled UI and terminal fonts.
///
/// egui has no italic family, so the italic face lives in the named family
/// [`FAMILY_INTER_ITALIC`]. Bold is the same variable font with `wght = 700`.
pub fn install(ctx: &egui::Context) {
    let installed = ctx.data(|data| data.get_temp::<bool>(egui::Id::unique(INSTALLED_ID)));
    if installed == Some(true) {
        return;
    }

    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert(INTER_KEY.to_owned(), Arc::new(FontData::from_static(INTER)));
    fonts.font_data.insert(
        CASCADIA_KEY.to_owned(),
        Arc::new(FontData::from_static(CASCADIA_MONO)),
    );
    fonts.font_data.insert(
        INTER_ITALIC_KEY.to_owned(),
        Arc::new(FontData::from_static(INTER_ITALIC)),
    );
    fonts.font_data.insert(
        INTER_BOLD_KEY.to_owned(),
        Arc::new(FontData::from_static(INTER).tweak(FontTweak {
            coords: egui::epaint::text::VariationCoords::new([("wght", 700.0)]),
            ..Default::default()
        })),
    );

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, INTER_KEY.to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, CASCADIA_KEY.to_owned());
    fonts.families.insert(
        FontFamily::Name(FAMILY_INTER_ITALIC.into()),
        vec![INTER_ITALIC_KEY.to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name(FAMILY_INTER_BOLD.into()),
        vec![INTER_BOLD_KEY.to_owned()],
    );

    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|style| {
        style.visuals.text_options.font_hinting = true;
        style.visuals.text_options.subpixel_binning = false;
    });
    ctx.data_mut(|data| data.insert_temp(egui::Id::unique(INSTALLED_ID), true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_font_bytes() {
        assert!(!INTER.is_empty());
        assert!(!INTER_ITALIC.is_empty());
        assert!(!CASCADIA_MONO.is_empty());
    }

    #[test]
    fn test_install_text_options() {
        let ctx = egui::Context::default();
        install(&ctx);
        let options = ctx.global_style().visuals.text_options;
        assert!(options.font_hinting);
        assert!(!options.subpixel_binning);
    }
}
