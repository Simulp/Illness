use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use egui::{FontData, FontDefinitions, FontFamily, FontTweak};

use crate::glyph::FontFace;

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

/// Installed font families, deduplicated and sorted case-insensitively.
///
/// Monospace families come first (they are the useful terminal choices) but
/// proportional families are still listed. The list is loaded once per process
/// because enumerating system fonts is comparatively expensive, and the cached
/// slice is shared so per-frame callers never clone it.
pub fn installed_families() -> &'static [String] {
    static FAMILIES: OnceLock<Vec<String>> = OnceLock::new();
    FAMILIES.get_or_init(load_installed_families).as_slice()
}

/// One system font database shared by enumeration and face loading.
///
/// Loading system fonts costs ~250 ms, so it must happen at most once per
/// process even when Settings enumerates families and then loads a face.
fn database() -> &'static fontdb::Database {
    static DATABASE: OnceLock<fontdb::Database> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

fn load_installed_families() -> Vec<String> {
    let db = database();

    // Some families appear on several faces; treat a family as monospace when
    // any of its faces is, so grouping and the settings list agree.
    let monospace_names: HashSet<String> = db
        .faces()
        .filter(|face| face.monospaced)
        .flat_map(|face| face.families.iter().map(|(name, _)| name.to_lowercase()))
        .collect();

    let mut families: Vec<(bool, String)> = Vec::new();
    let mut seen = HashSet::new();
    for face in db.faces() {
        for (name, _) in &face.families {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let key = name.to_lowercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            families.push((monospace_names.contains(&key), name.to_owned()));
        }
    }

    // Stable grouping: monospace faces first, then case-insensitive names.
    families.sort_by_cached_key(|(monospaced, name)| (!*monospaced, name.to_lowercase()));
    families.into_iter().map(|(_, name)| name).collect()
}

/// Loads a font family for the terminal grid.
///
/// Returns the regular font file bytes plus the face index within that file,
/// or `None` when no installed face matches `family`. Typographic and legacy
/// family names are both considered by `fontdb`; matching here is
/// case-insensitive.
pub fn load_terminal_font(family: &str) -> Option<(Vec<u8>, u32)> {
    let font = load_terminal_font_faces(family)?;
    Some(((*font.regular.data).clone(), font.regular.index))
}

/// The regular and real styled faces of one terminal font family.
#[derive(Clone)]
pub struct TerminalFont {
    pub regular: FontFace,
    pub bold: Option<FontFace>,
    pub italic: Option<FontFace>,
    pub bold_italic: Option<FontFace>,
}

/// Loads a terminal family together with any real bold and italic faces.
///
/// A family that only ships one face (or a variable font whose bold comes from
/// the `wght` axis) returns `None` for the styles it does not install; the
/// atlas synthesizes those styles instead.
pub fn load_terminal_font_faces(family: &str) -> Option<TerminalFont> {
    let requested = family.trim();
    if requested.is_empty() {
        return None;
    }

    let db = database();
    let resolved = resolve_family(db, requested)?;
    let regular = load_face(
        db,
        &resolved,
        fontdb::Weight::NORMAL,
        fontdb::Style::Normal,
        false,
        false,
    )?;

    Some(TerminalFont {
        regular,
        bold: load_face(
            db,
            &resolved,
            fontdb::Weight::BOLD,
            fontdb::Style::Normal,
            true,
            false,
        ),
        italic: load_face(
            db,
            &resolved,
            fontdb::Weight::NORMAL,
            fontdb::Style::Italic,
            false,
            true,
        ),
        bold_italic: load_face(
            db,
            &resolved,
            fontdb::Weight::BOLD,
            fontdb::Style::Italic,
            true,
            true,
        ),
    })
}

/// Resolves a user-supplied family spelling to the installed family name.
fn resolve_family(db: &fontdb::Database, requested: &str) -> Option<String> {
    db.faces()
        .flat_map(|face| face.families.iter().map(|(name, _)| name.as_str()))
        .find(|name| *name == requested)
        .or_else(|| {
            db.faces()
                .flat_map(|face| face.families.iter().map(|(name, _)| name.as_str()))
                .find(|name| name.eq_ignore_ascii_case(requested))
        })
        .map(str::to_owned)
}

/// Loads one face and rejects a closest-weight fallback that is not actually
/// the requested style (for example a regular face when bold was asked for).
fn load_face(
    db: &fontdb::Database,
    family: &str,
    weight: fontdb::Weight,
    style: fontdb::Style,
    bold: bool,
    italic: bool,
) -> Option<FontFace> {
    let families = [fontdb::Family::Name(family)];
    let query = fontdb::Query {
        families: &families,
        weight,
        stretch: fontdb::Stretch::Normal,
        style,
    };
    let id = db.query(&query)?;
    let face = db.face(id)?;

    if !face
        .families
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(family))
    {
        return None;
    }
    let is_italic = matches!(face.style, fontdb::Style::Italic | fontdb::Style::Oblique);
    if italic != is_italic {
        return None;
    }
    if bold && face.weight < fontdb::Weight(600) {
        return None;
    }
    if italic && !bold && face.weight >= fontdb::Weight(600) {
        return None;
    }

    db.with_face_data(id, |data, index| FontFace::new(data.to_vec(), index))
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
    fn system_font_database_is_shared() {
        assert!(std::ptr::eq(database(), database()));
        let first = installed_families();
        let second = installed_families();
        assert_eq!(first, second);
    }

    #[test]
    fn installed_families_reuses_cached_storage() {
        let first = installed_families();
        let second = installed_families();
        assert!(
            std::ptr::eq(first, second),
            "installed_families must return the cached slice, not a clone"
        );
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn test_install_text_options() {
        let ctx = egui::Context::default();
        install(&ctx);
        let options = ctx.global_style().visuals.text_options;
        assert!(options.font_hinting);
        assert!(!options.subpixel_binning);
    }

    #[test]
    fn test_installed_families_sorted_and_monospace_first() {
        let families = installed_families();
        if families.is_empty() {
            eprintln!("skipping installed-family assertions: no system fonts");
            return;
        }

        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let is_monospace = |family: &str| {
            db.faces().any(|face| {
                face.monospaced
                    && face
                        .families
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(family))
            })
        };

        let mut previous: Option<(bool, String)> = None;
        let mut seen_proportional = false;
        for family in families {
            let monospace = is_monospace(family);
            assert!(
                !monospace || !seen_proportional,
                "monospace family {family:?} ranked after a proportional family"
            );
            seen_proportional |= !monospace;

            if let Some((previous_monospace, previous_name)) = &previous
                && *previous_monospace == monospace
            {
                assert!(
                    previous_name.to_lowercase() <= family.to_lowercase(),
                    "{family:?} sorts before {previous_name:?} in the same group"
                );
            }
            previous = Some((monospace, family.clone()));
        }
    }

    #[test]
    fn test_per_user_nerd_font_is_listed() {
        let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let user_fonts = std::path::PathBuf::from(local_app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Fonts");
        let installed = user_fonts.join("CaskaydiaCoveNerdFont-Regular.ttf");
        if !installed.is_file() {
            eprintln!("skipping per-user font assertion: Nerd Font is not installed");
            return;
        }

        let families = installed_families();
        assert!(
            families
                .iter()
                .any(|family| family == "CaskaydiaCove Nerd Font"),
            "per-user CaskaydiaCove Nerd Font missing from installed families"
        );
    }

    #[test]
    fn test_load_terminal_font() {
        let family = installed_families()
            .iter()
            .find(|family| family.as_str() == "Cascadia Mono")
            .or_else(|| installed_families().first())
            .cloned();
        let Some(family) = family else {
            eprintln!("skipping load assertion: no system fonts");
            return;
        };

        let (data, index) =
            load_terminal_font(&family).unwrap_or_else(|| panic!("{family:?} should load"));
        assert!(!data.is_empty(), "{family:?} returned empty font data");
        assert!(
            swash::FontRef::from_index(&data, index as usize).is_some(),
            "{family:?} face index {index} is not readable"
        );
    }

    #[test]
    fn test_load_terminal_font_faces_nerd_font_styles() {
        let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let user_fonts = std::path::PathBuf::from(local_app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Fonts");
        let installed = user_fonts.join("CaskaydiaCoveNerdFont-Regular.ttf");
        if !installed.is_file() {
            eprintln!("skipping styled-face assertion: Nerd Font is not installed");
            return;
        }

        let font =
            load_terminal_font_faces("CaskaydiaCove Nerd Font").expect("Nerd Font should load");
        let bold = font.bold.expect("Nerd Font should load its Bold face");
        let italic = font.italic.expect("Nerd Font should load its Italic face");
        let bold_italic = font
            .bold_italic
            .expect("Nerd Font should load its BoldItalic face");

        for (label, face) in [
            ("regular", &font.regular),
            ("bold", &bold),
            ("italic", &italic),
            ("bold italic", &bold_italic),
        ] {
            assert!(
                swash::FontRef::from_index(&face.data, face.index as usize).is_some(),
                "{label} face index {} is not readable",
                face.index
            );
        }
        assert!(
            !Arc::ptr_eq(&font.regular.data, &bold.data),
            "Bold should be a separate static font file"
        );
    }

    #[test]
    fn test_load_missing_font_returns_none() {
        assert!(load_terminal_font("definitely-not-a-font").is_none());
        assert!(load_terminal_font("").is_none());
    }
}
