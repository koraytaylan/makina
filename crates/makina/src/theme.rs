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
}

pub const ALL_ROLES: [ThemeRole; 10] = [
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
    /// Role color; panics (with the role name) if undefined — the value-pinning
    /// test guards every built-in theme so this never fires at render time.
    pub fn get(&self, role: ThemeRole) -> Color {
        *self
            .colors
            .get(&role)
            .unwrap_or_else(|| panic!("theme {:?} missing role {:?}", self.name, role))
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

        // Check ANSI entries (normal red at index 1, bright red at index 9)
        assert_eq!(th.ansi(1), Color::Rgb(211, 99, 106));
        assert_eq!(th.ansi(9), Color::Rgb(240, 113, 120));
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
}
