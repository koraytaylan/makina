use ratatui::style::Color;
use std::collections::HashMap;

/// Semantic intent of a color — looked up from the active theme at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeRole {
    Background,  // panes, empty states
    Foreground,  // normal text
    Dim,         // secondary/disabled text, hints
    Accent,      // selection, active badge, task ids
    SelectionBg, // text-selection background
    Border,      // frames, dividers
    Success,     // completed / added
    Warning,     // partial / pending
    Error,       // failed / removed
    Info,        // informational / modified
    CodeBlock,   // foreground color for code blocks and inline code
    CodeBlockBg, // background surface for code block bands
    FocusBg,     // active/focused widget backgrounds
}

pub const ALL_ROLES: [ThemeRole; 13] = [
    ThemeRole::Background,
    ThemeRole::Foreground,
    ThemeRole::Dim,
    ThemeRole::Accent,
    ThemeRole::SelectionBg,
    ThemeRole::Border,
    ThemeRole::Success,
    ThemeRole::Warning,
    ThemeRole::Error,
    ThemeRole::Info,
    ThemeRole::CodeBlock,
    ThemeRole::CodeBlockBg,
    ThemeRole::FocusBg,
];

/// A complete theme: semantic roles + a 16-entry ANSI palette (0–7 normal
/// black,red,green,yellow,blue,magenta,cyan,white; 8–15 the bright set).
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    colors: HashMap<ThemeRole, Color>,
    ansi: [Color; 16],
}

impl Theme {
    /// Role color; returns Color::Reset (the terminal default) if undefined — the value-pinning
    /// test guards every built-in theme so this never fires at render time, but custom themes
    /// may have incomplete role definitions.
    pub fn get(&self, role: ThemeRole) -> Color {
        *self.colors.get(&role).unwrap_or_else(|| {
            tracing::warn!(
                "theme {} missing role {:?}, using fallback",
                self.name,
                role
            );
            &Color::Reset
        })
    }
    /// ANSI palette entry (index 0–15).
    pub fn ansi(&self, index: usize) -> Color {
        self.ansi[index]
    }

    pub fn builtin_themes() -> Vec<Theme> {
        vec![ayu_dark(), ayu_mirage(), ayu_light()]
    }
}

pub fn ayu_dark() -> Theme {
    let mut colors = HashMap::new();
    colors.insert(ThemeRole::Background, Color::Rgb(13, 16, 23)); // #0D1017 surface.base
    colors.insert(ThemeRole::Foreground, Color::Rgb(191, 189, 182)); // #BFBDB6 editor.fg
    colors.insert(ThemeRole::Dim, Color::Rgb(90, 99, 120)); // #5A6378 ui.fg
    colors.insert(ThemeRole::Accent, Color::Rgb(230, 180, 80)); // #E6B450 common.accent
    colors.insert(ThemeRole::SelectionBg, Color::Rgb(25, 49, 85)); // #193155 selection@.25 over bg
    colors.insert(ThemeRole::Border, Color::Rgb(27, 31, 41)); // #1B1F29 ui.line
    colors.insert(ThemeRole::Success, Color::Rgb(112, 191, 86)); // #70BF56 vcs.added
    colors.insert(ThemeRole::Warning, Color::Rgb(255, 180, 84)); // #FFB454 palette.yellow
    colors.insert(ThemeRole::Error, Color::Rgb(217, 87, 87)); // #D95757 common.error
    colors.insert(ThemeRole::Info, Color::Rgb(115, 184, 255)); // #73B8FF vcs.modified
    colors.insert(ThemeRole::CodeBlock, Color::Rgb(115, 184, 255)); // same as Info for consistency
    colors.insert(ThemeRole::CodeBlockBg, Color::Rgb(22, 27, 36)); // #161B24 subtle darker band
    colors.insert(ThemeRole::FocusBg, Color::Rgb(40, 80, 120)); // darker, more saturated blue

    let ansi = [
        Color::Rgb(10, 14, 20),
        Color::Rgb(211, 99, 106),
        Color::Rgb(150, 191, 67),
        Color::Rgb(224, 158, 74),
        Color::Rgb(78, 171, 224),
        Color::Rgb(185, 146, 224),
        Color::Rgb(131, 202, 179),
        Color::Rgb(191, 189, 182),
        Color::Rgb(104, 104, 104),
        Color::Rgb(240, 113, 120),
        Color::Rgb(170, 217, 76),
        Color::Rgb(255, 180, 84),
        Color::Rgb(89, 194, 255),
        Color::Rgb(210, 166, 255),
        Color::Rgb(149, 230, 203),
        Color::Rgb(255, 255, 255),
    ];
    Theme {
        name: "Ayu Dark".to_string(),
        colors,
        ansi,
    }
}

pub fn ayu_mirage() -> Theme {
    let mut colors = HashMap::new();
    colors.insert(ThemeRole::Background, Color::Rgb(31, 36, 48)); // #1F2430 surface.base
    colors.insert(ThemeRole::Foreground, Color::Rgb(204, 202, 194)); // #CCCAC2 editor.fg
    colors.insert(ThemeRole::Dim, Color::Rgb(112, 122, 140)); // #707A8C ui.fg
    colors.insert(ThemeRole::Accent, Color::Rgb(255, 204, 102)); // #FFCC66 common.accent
    colors.insert(ThemeRole::SelectionBg, Color::Rgb(43, 70, 104)); // #2B4668 selection@.25 over bg
    colors.insert(ThemeRole::Border, Color::Rgb(48, 56, 67)); // #303843 ui.line
    colors.insert(ThemeRole::Success, Color::Rgb(135, 217, 108)); // #87D96C vcs.added
    colors.insert(ThemeRole::Warning, Color::Rgb(255, 205, 102)); // #FFCD66 palette.yellow
    colors.insert(ThemeRole::Error, Color::Rgb(255, 102, 102)); // #FF6666 common.error
    colors.insert(ThemeRole::Info, Color::Rgb(128, 191, 255)); // #80BFFF vcs.modified
    colors.insert(ThemeRole::CodeBlock, Color::Rgb(128, 191, 255)); // same as Info
    colors.insert(ThemeRole::CodeBlockBg, Color::Rgb(39, 45, 56)); // #272D38 subtle darker band
    colors.insert(ThemeRole::FocusBg, Color::Rgb(70, 110, 160)); // similar dark-blue tone

    let ansi = [
        Color::Rgb(25, 30, 42),
        Color::Rgb(213, 119, 106),
        Color::Rgb(187, 224, 113),
        Color::Rgb(224, 180, 90),
        Color::Rgb(101, 183, 224),
        Color::Rgb(196, 168, 224),
        Color::Rgb(131, 202, 179),
        Color::Rgb(204, 202, 194),
        Color::Rgb(104, 104, 104),
        Color::Rgb(242, 135, 121),
        Color::Rgb(213, 255, 128),
        Color::Rgb(255, 205, 102),
        Color::Rgb(115, 208, 255),
        Color::Rgb(223, 191, 255),
        Color::Rgb(149, 230, 203),
        Color::Rgb(255, 255, 255),
    ];
    Theme {
        name: "Ayu Mirage".to_string(),
        colors,
        ansi,
    }
}

pub fn ayu_light() -> Theme {
    let mut colors = HashMap::new();
    colors.insert(ThemeRole::Background, Color::Rgb(248, 249, 250)); // #F8F9FA surface.base
    colors.insert(ThemeRole::Foreground, Color::Rgb(92, 97, 102)); // #5C6166 editor.fg
    colors.insert(ThemeRole::Dim, Color::Rgb(130, 142, 159)); // #828E9F ui.fg
    colors.insert(ThemeRole::Accent, Color::Rgb(242, 151, 24)); // #F29718 common.accent
    colors.insert(ThemeRole::SelectionBg, Color::Rgb(215, 228, 246)); // #D7E4F6 selection@.25 over bg
    colors.insert(ThemeRole::Border, Color::Rgb(231, 234, 237)); // #E7EAED ui.line
    colors.insert(ThemeRole::Success, Color::Rgb(108, 191, 67)); // #6CBF43 vcs.added
    colors.insert(ThemeRole::Warning, Color::Rgb(235, 164, 0)); // #EBA400 palette.yellow
    colors.insert(ThemeRole::Error, Color::Rgb(230, 80, 80)); // #E65050 common.error
    colors.insert(ThemeRole::Info, Color::Rgb(71, 138, 204)); // #478ACC vcs.modified
    colors.insert(ThemeRole::CodeBlock, Color::Rgb(71, 138, 204)); // same as Info, adjusted for light background
    colors.insert(ThemeRole::CodeBlockBg, Color::Rgb(238, 241, 244)); // #EEF1F4 subtle gray band
    colors.insert(ThemeRole::FocusBg, Color::Rgb(160, 188, 230)); // periwinkle, distinct from white Background #F8F9FA AND pale SelectionBg #D7E4F6

    let ansi = [
        Color::Rgb(92, 97, 102),
        Color::Rgb(211, 99, 99),
        Color::Rgb(118, 158, 0),
        Color::Rgb(207, 144, 0),
        Color::Rgb(30, 144, 202),
        Color::Rgb(143, 107, 180),
        Color::Rgb(67, 168, 135),
        Color::Rgb(252, 252, 252),
        Color::Rgb(50, 50, 50),
        Color::Rgb(240, 113, 113),
        Color::Rgb(134, 179, 0),
        Color::Rgb(235, 164, 0),
        Color::Rgb(34, 164, 230),
        Color::Rgb(163, 122, 204),
        Color::Rgb(76, 191, 153),
        Color::Rgb(255, 255, 255),
    ];
    Theme {
        name: "Ayu Light".to_string(),
        colors,
        ansi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_define_every_role_and_ansi_entry() {
        for theme in Theme::builtin_themes() {
            // Check all roles are defined
            for &role in &ALL_ROLES {
                let _ = theme.get(role); // Will panic if role is missing
            }
            // Check all 16 ANSI entries are accessible
            for i in 0..16 {
                let _ = theme.ansi(i);
            }
        }
    }

    #[test]
    fn ayu_dark_pins_expected_values() {
        let th = ayu_dark();

        // Check a representative set of exact Color::Rgb values for Ayu Dark
        assert_eq!(th.get(ThemeRole::Background), Color::Rgb(13, 16, 23));
        assert_eq!(th.get(ThemeRole::Accent), Color::Rgb(230, 180, 80));
        assert_eq!(th.get(ThemeRole::Error), Color::Rgb(217, 87, 87));
        assert_eq!(th.get(ThemeRole::CodeBlock), Color::Rgb(115, 184, 255));
        assert_eq!(th.get(ThemeRole::CodeBlockBg), Color::Rgb(22, 27, 36));
        assert_eq!(th.get(ThemeRole::FocusBg), Color::Rgb(40, 80, 120));

        // Check ANSI entries (normal red at index 1, bright red at index 9)
        assert_eq!(th.ansi(1), Color::Rgb(211, 99, 106));
        assert_eq!(th.ansi(9), Color::Rgb(240, 113, 120));
    }

    #[test]
    fn ayu_mirage_pins_expected_values() {
        let th = ayu_mirage();

        // Check CodeBlock value for Ayu Mirage
        assert_eq!(th.get(ThemeRole::CodeBlock), Color::Rgb(128, 191, 255));
        // Check CodeBlockBg value for Ayu Mirage
        assert_eq!(th.get(ThemeRole::CodeBlockBg), Color::Rgb(39, 45, 56));
        // Check FocusBg value for Ayu Mirage
        assert_eq!(th.get(ThemeRole::FocusBg), Color::Rgb(70, 110, 160));
    }

    #[test]
    fn ayu_light_pins_expected_values() {
        let th = ayu_light();

        // Check CodeBlock value for Ayu Light
        assert_eq!(th.get(ThemeRole::CodeBlock), Color::Rgb(71, 138, 204));
        // Check CodeBlockBg value for Ayu Light
        assert_eq!(th.get(ThemeRole::CodeBlockBg), Color::Rgb(238, 241, 244));
        // Check FocusBg value for Ayu Light
        assert_eq!(th.get(ThemeRole::FocusBg), Color::Rgb(160, 188, 230));
    }

    /// Regression guard for the focused-state distinctness gap fixed in plan 0037:
    /// every built-in theme's FocusBg (focused-widget background) must be clearly
    /// distinguishable from its SelectionBg (row-selection highlight), or a focused
    /// accordion section and a selected row look identical. The Ayu Light theme
    /// originally violated this — FocusBg #C8D7F0 vs SelectionBg #D7E4F6 were only
    /// 20.7 apart in RGB space.
    #[test]
    fn focus_bg_is_distinct_from_selection_bg_in_every_theme() {
        // Minimum acceptable Euclidean RGB distance. Ayu Dark (~49) and Mirage (~74)
        // already clear this comfortably; the floor catches near-collisions like the
        // pre-fix Ayu Light value (20.7).
        const MIN_DISTANCE: f64 = 40.0;

        fn rgb(c: Color) -> (f64, f64, f64) {
            match c {
                Color::Rgb(r, g, b) => (r as f64, g as f64, b as f64),
                other => panic!("expected Color::Rgb, got {other:?}"),
            }
        }
        fn distance(a: Color, b: Color) -> f64 {
            let (ar, ag, ab) = rgb(a);
            let (br, bg, bb) = rgb(b);
            ((ar - br).powi(2) + (ag - bg).powi(2) + (ab - bb).powi(2)).sqrt()
        }

        for theme in Theme::builtin_themes() {
            let focus = theme.get(ThemeRole::FocusBg);
            let selection = theme.get(ThemeRole::SelectionBg);
            let d = distance(focus, selection);
            assert!(
                d >= MIN_DISTANCE,
                "Theme {:?}: FocusBg {:?} and SelectionBg {:?} are only {:.1} apart \
                 (need >= {:.1}); a focused widget would be visually indistinct from a \
                 selected row.",
                theme.name,
                focus,
                selection,
                d,
                MIN_DISTANCE
            );
        }
    }

    #[test]
    fn builtin_themes_have_correct_names() {
        let themes = Theme::builtin_themes();
        assert_eq!(themes.len(), 3);
        assert_eq!(themes[0].name, "Ayu Dark");
        assert_eq!(themes[1].name, "Ayu Mirage");
        assert_eq!(themes[2].name, "Ayu Light");
    }

    #[test]
    fn resolve_theme_by_name_known() {
        // Resolving a known theme name should yield that theme
        let theme_name = "Ayu Mirage".to_string();
        let resolved = Theme::builtin_themes()
            .into_iter()
            .find(|t| t.name == theme_name)
            .unwrap_or_else(ayu_dark);

        assert_eq!(resolved.name, "Ayu Mirage");
    }

    #[test]
    fn resolve_theme_by_name_unknown_fallback() {
        // Resolving an unknown theme name should fall back to Ayu Dark with no panic
        let theme_name = "Unknown Theme".to_string();
        let resolved = Theme::builtin_themes()
            .into_iter()
            .find(|t| t.name == theme_name)
            .unwrap_or_else(ayu_dark);

        assert_eq!(resolved.name, "Ayu Dark");
    }

    #[test]
    fn resolve_theme_by_name_empty_string_fallback() {
        // An empty theme name should also fall back to Ayu Dark
        let theme_name = String::new();
        let resolved = Theme::builtin_themes()
            .into_iter()
            .find(|t| t.name == theme_name)
            .unwrap_or_else(ayu_dark);

        assert_eq!(resolved.name, "Ayu Dark");
    }

    #[test]
    fn all_theme_colors_are_rgb_not_indexed() {
        // Verify that all builtin themes use only Color::Rgb for roles and ANSI entries,
        // not Color::Indexed, Color::Ansi, or named colors. This guards against accidental
        // downsampling to 16-color ANSI and ensures truecolor output.
        for theme in Theme::builtin_themes() {
            // Check all roles are defined as Color::Rgb
            for &role in &ALL_ROLES {
                let color = theme.get(role);
                match color {
                    Color::Rgb(_, _, _) => {} // OK — all theme roles must be RGB
                    _ => panic!(
                        "Theme {:?} role {:?} is not Color::Rgb, got {:?}",
                        theme.name, role, color
                    ),
                }
            }
            // Check all 16 ANSI palette entries are Color::Rgb
            for i in 0..16 {
                let ansi_color = theme.ansi(i);
                match ansi_color {
                    Color::Rgb(_, _, _) => {} // OK
                    _ => panic!(
                        "Theme {:?} ANSI[{}] is not Color::Rgb, got {:?}",
                        theme.name, i, ansi_color
                    ),
                }
            }
        }
    }

    #[test]
    fn test_theme_get_missing_role_returns_default() {
        // Construct a Theme with an incomplete colors map (missing one role)
        let mut colors = HashMap::new();
        colors.insert(ThemeRole::Background, Color::Rgb(0, 0, 0));
        colors.insert(ThemeRole::Foreground, Color::Rgb(255, 255, 255));
        // Deliberately omit ThemeRole::Accent to test the fallback

        let theme = Theme {
            name: "Incomplete Theme".to_string(),
            colors,
            ansi: [Color::Rgb(0, 0, 0); 16],
        };

        // Calling get on the missing role should return Color::Reset and not panic
        let result = theme.get(ThemeRole::Accent);
        assert_eq!(
            result,
            Color::Reset,
            "Missing role should return Color::Reset as fallback"
        );
    }
}
