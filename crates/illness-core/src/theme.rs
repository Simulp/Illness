use serde::{Deserialize, Serialize};

/// An sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Lowercase CSS-style hex, e.g. `#0c0c0f`.
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Linear interpolation towards `other`; `t` is clamped to `0.0..=1.0`.
    pub fn blend(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |from: u8, to: u8| -> u8 {
            (f32::from(from) + (f32::from(to) - f32::from(from)) * t).round() as u8
        };
        Self {
            r: mix(self.r, other.r),
            g: mix(self.g, other.g),
            b: mix(self.b, other.b),
        }
    }
}

/// A terminal color scheme. All themes are static data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Rgb,
    pub fg: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    pub selection_fg: Rgb,
    pub accent: Rgb,
    pub ansi: [Rgb; 16],
}

impl Theme {
    /// Every built-in theme, Illness Dark first.
    pub fn all() -> &'static [Theme] {
        &THEMES
    }

    /// Exact match first, then case-insensitive, falling back to Illness Dark.
    pub fn by_name(name: &str) -> &'static Theme {
        THEMES
            .iter()
            .find(|theme| theme.name == name)
            .or_else(|| {
                THEMES
                    .iter()
                    .find(|theme| theme.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or(&THEMES[0])
    }

    /// Perceived luma of the background, using Rec. 709 weights.
    pub fn is_light(&self) -> bool {
        let luma = 0.2126 * f32::from(self.bg.r)
            + 0.7152 * f32::from(self.bg.g)
            + 0.0722 * f32::from(self.bg.b);
        luma / 255.0 > 0.6
    }
}

static THEMES: [Theme; 7] = [
    Theme {
        name: "Illness Dark",
        bg: Rgb::new(0x0c, 0x0c, 0x0f),
        fg: Rgb::new(0xd6, 0xd6, 0xdd),
        cursor: Rgb::new(0xd6, 0xd6, 0xdd),
        selection_bg: Rgb::new(0x26, 0x4f, 0x3a),
        selection_fg: Rgb::new(0xea, 0xff, 0xf1),
        accent: Rgb::new(0x7e, 0xe7, 0x87),
        ansi: [
            Rgb::new(0x16, 0x16, 0x1a),
            Rgb::new(0xf7, 0x76, 0x8e),
            Rgb::new(0x7e, 0xe7, 0x87),
            Rgb::new(0xe3, 0xb3, 0x41),
            Rgb::new(0x6c, 0xb6, 0xff),
            Rgb::new(0xd2, 0xa8, 0xff),
            Rgb::new(0x56, 0xd4, 0xdd),
            Rgb::new(0xc9, 0xd1, 0xd9),
            Rgb::new(0x4d, 0x4d, 0x5a),
            Rgb::new(0xff, 0x9a, 0xa8),
            Rgb::new(0xa5, 0xf3, 0xa0),
            Rgb::new(0xf2, 0xcc, 0x60),
            Rgb::new(0x8d, 0xc8, 0xff),
            Rgb::new(0xe2, 0xc1, 0xff),
            Rgb::new(0x8b, 0xe9, 0xf2),
            Rgb::new(0xf0, 0xf0, 0xf5),
        ],
    },
    Theme {
        name: "Campbell",
        bg: Rgb::new(0x0c, 0x0c, 0x0c),
        fg: Rgb::new(0xcc, 0xcc, 0xcc),
        cursor: Rgb::new(0xff, 0xff, 0xff),
        selection_bg: Rgb::new(0x26, 0x4f, 0x78),
        selection_fg: Rgb::new(0xff, 0xff, 0xff),
        accent: Rgb::new(0x3b, 0x78, 0xff),
        ansi: [
            Rgb::new(0x0c, 0x0c, 0x0c),
            Rgb::new(0xc5, 0x0f, 0x1f),
            Rgb::new(0x13, 0xa1, 0x0e),
            Rgb::new(0xc1, 0x9c, 0x00),
            Rgb::new(0x00, 0x37, 0xbd),
            Rgb::new(0x88, 0x17, 0x98),
            Rgb::new(0x3a, 0x96, 0xdd),
            Rgb::new(0xcc, 0xcc, 0xcc),
            Rgb::new(0x76, 0x76, 0x76),
            Rgb::new(0xe7, 0x48, 0x56),
            Rgb::new(0x16, 0xc6, 0x0c),
            Rgb::new(0xf9, 0xf1, 0xa5),
            Rgb::new(0x3b, 0x78, 0xff),
            Rgb::new(0xb4, 0x00, 0x9e),
            Rgb::new(0x61, 0xd6, 0xd6),
            Rgb::new(0xf2, 0xf2, 0xf2),
        ],
    },
    Theme {
        name: "One Half Dark",
        bg: Rgb::new(0x28, 0x2c, 0x34),
        fg: Rgb::new(0xdc, 0xdf, 0xe4),
        cursor: Rgb::new(0xdc, 0xdf, 0xe4),
        selection_bg: Rgb::new(0x3e, 0x44, 0x51),
        selection_fg: Rgb::new(0xdc, 0xdf, 0xe4),
        accent: Rgb::new(0x61, 0xaf, 0xef),
        ansi: [
            Rgb::new(0x28, 0x2c, 0x34),
            Rgb::new(0xe0, 0x6c, 0x75),
            Rgb::new(0x98, 0xc3, 0x79),
            Rgb::new(0xe5, 0xc0, 0x7b),
            Rgb::new(0x61, 0xaf, 0xef),
            Rgb::new(0xc6, 0x78, 0xdd),
            Rgb::new(0x56, 0xb6, 0xc2),
            Rgb::new(0xdc, 0xdf, 0xe4),
            Rgb::new(0x5c, 0x63, 0x70),
            Rgb::new(0xe0, 0x6c, 0x75),
            Rgb::new(0x98, 0xc3, 0x79),
            Rgb::new(0xe5, 0xc0, 0x7b),
            Rgb::new(0x61, 0xaf, 0xef),
            Rgb::new(0xc6, 0x78, 0xdd),
            Rgb::new(0x56, 0xb6, 0xc2),
            Rgb::new(0xdc, 0xdf, 0xe4),
        ],
    },
    Theme {
        name: "Nord",
        bg: Rgb::new(0x2e, 0x34, 0x40),
        fg: Rgb::new(0xd8, 0xde, 0xe9),
        cursor: Rgb::new(0xd8, 0xde, 0xe9),
        selection_bg: Rgb::new(0x43, 0x4c, 0x5e),
        selection_fg: Rgb::new(0xec, 0xef, 0xf4),
        accent: Rgb::new(0x88, 0xc0, 0xd0),
        ansi: [
            Rgb::new(0x3b, 0x42, 0x52),
            Rgb::new(0xbf, 0x61, 0x6a),
            Rgb::new(0xa3, 0xbe, 0x8c),
            Rgb::new(0xeb, 0xcb, 0x8b),
            Rgb::new(0x81, 0xa1, 0xc1),
            Rgb::new(0xb4, 0x8e, 0xad),
            Rgb::new(0x88, 0xc0, 0xd0),
            Rgb::new(0xe5, 0xe9, 0xf0),
            Rgb::new(0x4c, 0x56, 0x6a),
            Rgb::new(0xbf, 0x61, 0x6a),
            Rgb::new(0xa3, 0xbe, 0x8c),
            Rgb::new(0xeb, 0xcb, 0x8b),
            Rgb::new(0x81, 0xa1, 0xc1),
            Rgb::new(0xb4, 0x8e, 0xad),
            Rgb::new(0x8f, 0xbc, 0xbb),
            Rgb::new(0xec, 0xef, 0xf4),
        ],
    },
    Theme {
        name: "Gruvbox Dark",
        bg: Rgb::new(0x28, 0x28, 0x28),
        fg: Rgb::new(0xeb, 0xdb, 0xb2),
        cursor: Rgb::new(0xeb, 0xdb, 0xb2),
        selection_bg: Rgb::new(0x50, 0x49, 0x45),
        selection_fg: Rgb::new(0xeb, 0xdb, 0xb2),
        accent: Rgb::new(0x83, 0xa5, 0x98),
        ansi: [
            Rgb::new(0x28, 0x28, 0x28),
            Rgb::new(0xcc, 0x24, 0x1d),
            Rgb::new(0x98, 0x97, 0x1a),
            Rgb::new(0xd7, 0x99, 0x21),
            Rgb::new(0x45, 0x85, 0x88),
            Rgb::new(0xb1, 0x62, 0x86),
            Rgb::new(0x68, 0x9d, 0x6a),
            Rgb::new(0xa8, 0x99, 0x84),
            Rgb::new(0x92, 0x83, 0x74),
            Rgb::new(0xfb, 0x49, 0x34),
            Rgb::new(0xb8, 0xbb, 0x26),
            Rgb::new(0xfa, 0xbd, 0x2f),
            Rgb::new(0x83, 0xa5, 0x98),
            Rgb::new(0xd3, 0x86, 0x9b),
            Rgb::new(0x8e, 0xc0, 0x7c),
            Rgb::new(0xeb, 0xdb, 0xb2),
        ],
    },
    Theme {
        name: "Solarized Dark",
        bg: Rgb::new(0x00, 0x2b, 0x36),
        fg: Rgb::new(0x83, 0x94, 0x96),
        cursor: Rgb::new(0x93, 0xa1, 0xa1),
        selection_bg: Rgb::new(0x07, 0x36, 0x42),
        selection_fg: Rgb::new(0x93, 0xa1, 0xa1),
        accent: Rgb::new(0x26, 0x8b, 0xd2),
        ansi: [
            Rgb::new(0x07, 0x36, 0x42),
            Rgb::new(0xdc, 0x32, 0x2f),
            Rgb::new(0x85, 0x99, 0x00),
            Rgb::new(0xb5, 0x89, 0x00),
            Rgb::new(0x26, 0x8b, 0xd2),
            Rgb::new(0xd3, 0x36, 0x82),
            Rgb::new(0x2a, 0xa1, 0x98),
            Rgb::new(0xee, 0xe8, 0xd5),
            Rgb::new(0x00, 0x2b, 0x36),
            Rgb::new(0xcb, 0x4b, 0x16),
            Rgb::new(0x93, 0xa1, 0xa1),
            Rgb::new(0x65, 0x7b, 0x83),
            Rgb::new(0x83, 0x94, 0x96),
            Rgb::new(0x6c, 0x71, 0xc4),
            Rgb::new(0x93, 0xa1, 0xa1),
            Rgb::new(0xfd, 0xf6, 0xe3),
        ],
    },
    Theme {
        name: "Light",
        bg: Rgb::new(0xf5, 0xf5, 0xf5),
        fg: Rgb::new(0x1c, 0x1c, 0x1c),
        cursor: Rgb::new(0x1c, 0x1c, 0x1c),
        selection_bg: Rgb::new(0xd0, 0xe4, 0xff),
        selection_fg: Rgb::new(0x1c, 0x1c, 0x1c),
        accent: Rgb::new(0x00, 0x67, 0xc0),
        ansi: [
            Rgb::new(0x1c, 0x1c, 0x1c),
            Rgb::new(0xc4, 0x2b, 0x1c),
            Rgb::new(0x10, 0x7c, 0x10),
            Rgb::new(0x9a, 0x67, 0x00),
            Rgb::new(0x00, 0x50, 0xc0),
            Rgb::new(0x88, 0x17, 0x98),
            Rgb::new(0x00, 0x74, 0x84),
            Rgb::new(0x76, 0x76, 0x76),
            Rgb::new(0x50, 0x50, 0x50),
            Rgb::new(0xa4, 0x26, 0x1a),
            Rgb::new(0x0e, 0x6b, 0x0e),
            Rgb::new(0x8a, 0x5c, 0x00),
            Rgb::new(0x00, 0x46, 0xa8),
            Rgb::new(0x76, 0x14, 0x82),
            Rgb::new(0x00, 0x66, 0x74),
            Rgb::new(0x1c, 0x1c, 0x1c),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(Rgb::new(255, 0, 0).hex(), "#ff0000");
        assert_eq!(Rgb::new(0x0c, 0x0c, 0x0f).hex(), "#0c0c0f");
    }

    #[test]
    fn catalog_has_seven_themes() {
        assert_eq!(Theme::all().len(), 7);
        assert_eq!(Theme::all()[0].name, "Illness Dark");
    }

    #[test]
    fn lookup_exact_case_insensitive_and_fallback() {
        assert_eq!(Theme::by_name("Nord").name, "Nord");
        assert_eq!(Theme::by_name("nord").name, "Nord");
        assert_eq!(Theme::by_name("NORD").name, "Nord");
        assert_eq!(Theme::by_name("not a theme").name, "Illness Dark");
    }

    #[test]
    fn every_theme_has_sixteen_ansi_colors() {
        for theme in Theme::all() {
            assert_eq!(theme.ansi.len(), 16, "{}", theme.name);
        }
    }

    #[test]
    fn illness_dark_matches_the_brand() {
        let theme = Theme::by_name("Illness Dark");
        assert_eq!(theme.bg.hex(), "#0c0c0f");
        assert_eq!(theme.fg.hex(), "#d6d6dd");
        assert_eq!(theme.accent.hex(), "#7ee787");
        assert_eq!(theme.selection_bg.hex(), "#264f3a");
    }

    #[test]
    fn light_detection_uses_luma() {
        assert!(Theme::by_name("Light").is_light());
        assert!(!Theme::by_name("Illness Dark").is_light());
        assert!(!Theme::by_name("Solarized Dark").is_light());
    }

    #[test]
    fn blend_mixes_channels() {
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(255, 255, 255);
        assert_eq!(black.blend(white, 0.0), black);
        assert_eq!(black.blend(white, 1.0), white);
        assert_eq!(black.blend(white, 0.5), Rgb::new(128, 128, 128));
    }
}
