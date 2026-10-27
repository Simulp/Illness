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
const UI_REGULAR_KEY: &str = "illness-ui-regular";
const UI_BOLD_KEY: &str = "illness-ui-bold";
const UI_ITALIC_KEY: &str = "illness-ui-italic";
const INSTALLED_ID: &str = "illness-fonts-installed";

/// Installed UI families to use for chrome text, most preferred first.
///
/// Windows Terminal uses Segoe UI Variable Text. Older Windows installs only
/// ship the static Segoe UI family, and the variable Display optical size is a
/// last resort before the bundled Inter face.
const UI_FACE_CANDIDATES: [&str; 3] = [
    "Segoe UI Variable Text",
    "Segoe UI",
    "Segoe UI Variable Display",
];

/// Inter at weight 700, used when no static bold face is available.
fn bold_tweak() -> FontTweak {
    FontTweak {
        coords: egui::epaint::text::VariationCoords::new([("wght", 700.0)]),
        ..Default::default()
    }
}

/// Register bundled UI and terminal fonts.
///
/// The system UI face (Segoe UI Variable Text, then Segoe UI) becomes the
/// primary proportional family when installed; bundled Inter follows as the
/// glyph fallback. egui has no italic family, so styled faces live in the
/// named families [`FAMILY_INTER_ITALIC`] and [`FAMILY_INTER_BOLD`].
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
        Arc::new(FontData::from_static(INTER).tweak(bold_tweak())),
    );

    // A real bold face wins; a variable face with a `wght` axis is asked for
    // weight 700 directly; otherwise the Inter bold definition stays in place.
    let ui = load_system_ui_font();
    let ui_bold = ui.as_ref().and_then(|font| match &font.bold {
        Some(face) => Some(face_font_data(face, None)),
        None if has_weight_axis(&font.regular) => {
            Some(face_font_data(&font.regular, Some(bold_tweak())))
        }
        None => None,
    });
    let ui_italic = ui
        .as_ref()
        .and_then(|font| font.italic.as_ref())
        .map(|face| face_font_data(face, None));

    if let Some(font) = &ui {
        log::info!("using system UI font {}", font.family);
        fonts.font_data.insert(
            UI_REGULAR_KEY.to_owned(),
            Arc::new(face_font_data(&font.regular, None)),
        );
    }
    if let Some(data) = ui_bold {
        fonts
            .font_data
            .insert(UI_BOLD_KEY.to_owned(), Arc::new(data));
    }
    if let Some(data) = ui_italic {
        fonts
            .font_data
            .insert(UI_ITALIC_KEY.to_owned(), Arc::new(data));
    }

    {
        let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
        let inter_index = if ui.is_some() {
            proportional.insert(0, UI_REGULAR_KEY.to_owned());
            1
        } else {
            0
        };
        proportional.insert(inter_index, INTER_KEY.to_owned());
    }
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, CASCADIA_KEY.to_owned());
    let italic_key = if fonts.font_data.contains_key(UI_ITALIC_KEY) {
        UI_ITALIC_KEY
    } else {
        INTER_ITALIC_KEY
    };
    let bold_key = if fonts.font_data.contains_key(UI_BOLD_KEY) {
        UI_BOLD_KEY
    } else {
        INTER_BOLD_KEY
    };
    let styled = |primary: &str, fallback: &str| {
        if primary == fallback {
            vec![fallback.to_owned()]
        } else {
            vec![primary.to_owned(), fallback.to_owned()]
        }
    };
    fonts.families.insert(
        FontFamily::Name(FAMILY_INTER_ITALIC.into()),
        styled(italic_key, INTER_ITALIC_KEY),
    );
    fonts.families.insert(
        FontFamily::Name(FAMILY_INTER_BOLD.into()),
        styled(bold_key, INTER_BOLD_KEY),
    );

    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|style| {
        style.visuals.text_options.font_hinting = true;
        style.visuals.text_options.subpixel_binning = false;
    });
    ctx.data_mut(|data| data.insert_temp(egui::Id::unique(INSTALLED_ID), true));
}

/// The system UI family and the styles installed for it.
struct SystemUiFont {
    family: String,
    regular: FontFace,
    bold: Option<FontFace>,
    italic: Option<FontFace>,
}

/// Picks the first candidate family the font database actually has.
fn select_ui_family<'a>(
    candidates: &[&'a str],
    available: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    candidates.iter().copied().find(|family| available(family))
}

/// The typographic family behind a named optical-size instance.
///
/// `fontdb` reports the Windows variable UI font as `Segoe UI Variable`, not as
/// its `Text`/`Display` named instances, so those spellings resolve through the
/// shared variable family first.
fn ui_family_alias(candidate: &str) -> Option<&str> {
    let (base, instance) = candidate.rsplit_once(' ')?;
    matches!(instance, "Text" | "Display").then_some(base)
}

/// Resolves a UI candidate to an installed family, trying the named-instance
/// alias before giving up.
fn resolve_ui_family(db: &fontdb::Database, candidate: &str) -> Option<String> {
    resolve_family(db, candidate)
        .or_else(|| ui_family_alias(candidate).and_then(|base| resolve_family(db, base)))
}

/// Loads the Windows UI face, or `None` when the platform has no such face.
///
/// Other platforms keep the bundled Inter UI font, so the lookup is compiled
/// to a no-op there.
fn load_system_ui_font() -> Option<SystemUiFont> {
    if !cfg!(windows) {
        return None;
    }
    let db = database();
    let family = select_ui_family(&UI_FACE_CANDIDATES, |candidate| {
        resolve_ui_family(db, candidate).is_some()
    })?;
    let resolved = resolve_ui_family(db, family)?;
    let regular = load_face(
        db,
        &resolved,
        fontdb::Weight::NORMAL,
        fontdb::Style::Normal,
        false,
        false,
    )?;

    Some(SystemUiFont {
        family: resolved.clone(),
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
    })
}

/// Wraps system face bytes for egui, keeping the file index and any variation
/// coordinates so a variable face can stand in for bold.
fn face_font_data(face: &FontFace, tweak: Option<FontTweak>) -> FontData {
    let mut font = FontData::from_owned((*face.data).clone());
    font.index = face.index;
    match tweak {
        Some(tweak) => font.tweak(tweak),
        None => font,
    }
}

/// Whether the face exposes a `wght` variation axis.
fn has_weight_axis(face: &FontFace) -> bool {
    swash::FontRef::from_index(&face.data, face.index as usize).is_some_and(|font| {
        font.variations()
            .any(|var| var.tag() == swash::tag_from_bytes(b"wght"))
    })
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
    fn ui_family_selection_prefers_variable_text_then_segoe_ui() {
        let available = |names: &[&str]| -> HashSet<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };

        let all = available(&["Segoe UI Variable Text", "Segoe UI"]);
        assert_eq!(
            select_ui_family(&UI_FACE_CANDIDATES, |family| all.contains(family)),
            Some("Segoe UI Variable Text")
        );

        let static_only = available(&["Segoe UI", "Segoe UI Variable Display"]);
        assert_eq!(
            select_ui_family(&UI_FACE_CANDIDATES, |family| static_only.contains(family)),
            Some("Segoe UI")
        );

        let display_only = available(&["Segoe UI Variable Display"]);
        assert_eq!(
            select_ui_family(&UI_FACE_CANDIDATES, |family| display_only.contains(family)),
            Some("Segoe UI Variable Display")
        );

        assert_eq!(
            select_ui_family(&UI_FACE_CANDIDATES, |_| false),
            None,
            "no installed candidate must fall back to the Inter-only setup"
        );
    }

    #[test]
    fn variable_instance_alias_maps_to_typographic_family() {
        assert_eq!(
            ui_family_alias("Segoe UI Variable Text"),
            Some("Segoe UI Variable")
        );
        assert_eq!(
            ui_family_alias("Segoe UI Variable Display"),
            Some("Segoe UI Variable")
        );
        assert_eq!(ui_family_alias("Segoe UI"), None);
    }

    #[test]
    fn test_system_ui_font_loads_or_falls_back() {
        let found = load_system_ui_font();
        if !cfg!(windows) {
            assert!(found.is_none(), "only Windows supplies the UI face");
            return;
        }
        let Some(font) = found else {
            eprintln!("no Segoe UI face installed; Inter remains the UI font");
            return;
        };
        eprintln!("system UI font: {}", font.family);
        assert!(
            swash::FontRef::from_index(&font.regular.data, font.regular.index as usize).is_some(),
            "{} regular face is not readable",
            font.family
        );
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
