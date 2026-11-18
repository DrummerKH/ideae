//! The themes of the UI and the terminals, each light/dark/auto; Fira Code for code.

use gpui_cn::terminal::TerminalColors;
use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    /// Follow the OS.
    Auto,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::Auto, Appearance::Light, Appearance::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::Auto => "Auto",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label().eq_ignore_ascii_case(s))
    }
}

/// The code font of the editor, diffs and lists of code; terminals keep
/// gpui-cn's JetBrains Mono, which they draw without ligatures anyway.
/// Embedded (SIL OFL 1.1, `assets/fonts/OFL-FiraCode.txt`), so the machine
/// need not have it. Ligatures are on: GPUI shapes with `calt` by default.
pub const CODE_FONT: &str = "Fira Code";

fn register_code_font(cx: &mut App) {
    use std::borrow::Cow;
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/FiraCode-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/FiraCode-Bold.ttf").as_slice()),
    ];
    match cx.text_system().add_fonts(fonts) {
        Ok(()) => Theme::global_mut(cx).mono_font_family = CODE_FONT.into(),
        Err(error) => eprintln!("{CODE_FONT}: {error:#}"),
    }
}

/// A theme to choose: a light and a dark one that go together, by the
/// names they have in their file.
pub struct Family {
    pub name: &'static str,
    pub light: &'static str,
    pub dark: &'static str,
    file: &'static str,
}

/// The themes built in; the first is the one to start with. All but Ayu
/// are the files of gpui-component (Apache-2.0), as they are there.
pub const FAMILIES: [Family; 11] = [
    Family { name: "Ayu", light: "Ayu Light", dark: "Ayu Dark", file: include_str!("../themes/ayu.json") },
    Family { name: "Catppuccin", light: "Catppuccin Latte", dark: "Catppuccin Mocha", file: include_str!("../themes/catppuccin.json") },
    Family { name: "Gruvbox", light: "Gruvbox Light", dark: "Gruvbox Dark", file: include_str!("../themes/gruvbox.json") },
    Family { name: "Solarized", light: "Solarized Light", dark: "Solarized Dark", file: include_str!("../themes/solarized.json") },
    Family { name: "Everforest", light: "Everforest Light", dark: "Everforest Dark", file: include_str!("../themes/everforest.json") },
    Family { name: "Flexoki", light: "Flexoki Light", dark: "Flexoki Dark", file: include_str!("../themes/flexoki.json") },
    Family { name: "GitHub", light: "GitHub Light", dark: "GitHub Dark", file: include_str!("../themes/github.json") },
    Family { name: "One", light: "One Light", dark: "One Dark", file: include_str!("../themes/one.json") },
    Family { name: "Rosé Pine", light: "Rosé Pine Dawn", dark: "Rosé Pine", file: include_str!("../themes/rose-pine.json") },
    Family { name: "Tokyo Night", light: "Tokyo Night Day", dark: "Tokyo Night", file: include_str!("../themes/tokyonight.json") },
    Family { name: "Islands", light: "Islands Light", dark: "Islands Dark", file: include_str!("../themes/islands.json") },
];

/// The families of the light and the dark side.
struct Chosen(&'static str, &'static str);
impl Global for Chosen {}

/// The family drawn now: of the light or the dark side, as the window is.
pub fn family(cx: &App) -> &'static str {
    let dark = Theme::global(cx).mode.is_dark();
    cx.try_global::<Chosen>().map_or(FAMILIES[0].name, |chosen| if dark { chosen.1 } else { chosen.0 })
}

/// Registers the themes and the code font; Ayu until another is chosen.
pub fn init(cx: &mut App) {
    register_code_font(cx);
    for family in &FAMILIES {
        if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(family.file) {
            eprintln!("{} theme: {error:#}", family.name);
        }
    }
    choose(FAMILIES[0].name, FAMILIES[0].name, cx);
}

fn named(name: &str) -> &'static Family {
    FAMILIES.iter().find(|family| family.name == name).unwrap_or(&FAMILIES[0])
}

/// Makes the light side of the family `light` the light theme and the dark
/// side of `dark` the dark one; one not known is the first.
/// What is on the screen follows at the next [`apply`].
pub fn choose(light: &str, dark: &str, cx: &mut App) {
    let (light, dark) = (named(light), named(dark));
    cx.set_global(Chosen(light.name, dark.name));
    let themes = ThemeRegistry::global(cx).themes();
    let (light, dark) = (themes.get(light.light).cloned(), themes.get(dark.dark).cloned());
    let theme = Theme::global_mut(cx);
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
    }
}

/// The corner the window's notices show in, as Settings says.
pub fn place_notices(cx: &mut App) {
    use gpui_kit::Anchor;
    let at = match crate::state::settings().notices_at.as_str() {
        "top-left" => Anchor::TopLeft,
        "top-right" => Anchor::TopRight,
        "bottom-left" => Anchor::BottomLeft,
        _ => Anchor::BottomRight,
    };
    let notices = &mut Theme::global_mut(cx).notification;
    if notices.placement != at {
        notices.placement = at;
        cx.refresh_windows();
    }
}

/// The same where there is no window at hand: the theme chosen in Settings.
pub fn apply_everywhere(appearance: Appearance, cx: &mut App) {
    match appearance {
        Appearance::Auto => Theme::sync_system_appearance(None, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, None, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

pub fn apply(appearance: Appearance, window: &mut Window, cx: &mut App) {
    match appearance {
        Appearance::Auto => Theme::sync_system_appearance(Some(window), cx),
        Appearance::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }
}

/// Terminal colors for the current UI theme: its background and foreground,
/// so the terminal sits flush with the panes, and Ayu's 16-color palette
/// (Ghostty's "Ayu" / "Ayu Light").
pub fn terminal_colors(cx: &App) -> TerminalColors {
    let theme = Theme::global(cx);
    let palette = match (family(cx), theme.mode.is_dark()) {
        ("Ayu", true) => AYU_DARK.to_string(),
        ("Ayu", false) => AYU_LIGHT.to_string(),
        // The theme's own six colors, and their bright ones where it has them.
        _ => {
            let dim = theme.muted_foreground;
            let colors = [
                theme.background, theme.red, theme.green, theme.yellow, theme.blue, theme.magenta, theme.cyan, dim,
                dim, theme.red_light, theme.green_light, theme.yellow_light, theme.blue_light, theme.magenta_light, theme.cyan_light, theme.foreground,
            ];
            let mut palette: String = colors.iter().enumerate().map(|(n, color)| format!("palette = {n}={}\n", hex(*color))).collect();
            palette.push_str(&format!("cursor-color = {}\nselection-background = {}\n", hex(theme.primary), hex(theme.selection)));
            palette
        }
    };
    let text = format!(
        "background = {}\nforeground = {}\n{palette}",
        hex(theme.background),
        hex(theme.foreground)
    );
    TerminalColors::from_ghostty_theme(&text).unwrap_or_default()
}

fn hex(color: Hsla) -> String {
    let c = color.to_rgb();
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

const AYU_DARK: &str = "\
palette = 0=#11151c
palette = 1=#ea6c73
palette = 2=#7fd962
palette = 3=#f9af4f
palette = 4=#53bdfa
palette = 5=#cda1fa
palette = 6=#90e1c6
palette = 7=#c7c7c7
palette = 8=#686868
palette = 9=#f07178
palette = 10=#aad94c
palette = 11=#ffb454
palette = 12=#59c2ff
palette = 13=#d2a6ff
palette = 14=#95e6cb
palette = 15=#ffffff
cursor-color = #e6b450
selection-background = #1b3a5b
";

const AYU_LIGHT: &str = "\
palette = 0=#000000
palette = 1=#ea6c6d
palette = 2=#6cbf43
palette = 3=#eca944
palette = 4=#3199e1
palette = 5=#9e75c7
palette = 6=#46ba94
palette = 7=#bababa
palette = 8=#686868
palette = 9=#f07171
palette = 10=#86b300
palette = 11=#f2ae49
palette = 12=#399ee6
palette = 13=#a37acc
palette = 14=#4cbf99
palette = 15=#d1d1d1
cursor-color = #ffaa33
selection-background = #d1e4f4
";

#[cfg(test)]
mod tests {
    use super::{Appearance, FAMILIES, apply_everywhere, choose, family, hex, init, terminal_colors};
    use gpui_cn::terminal::TerminalColors;
    use gpui_kit::TestAppContext;
    use gpui_kit::component::{Theme, ThemeRegistry};

    /// Every family is in its file under the names it is asked for, has a
    /// light and a dark side of its own, and colors for the terminals.
    #[gpui_kit::test]
    fn every_theme_has_both_sides_and_a_terminal_palette(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
            let mut grounds = Vec::new();
            for family in &FAMILIES {
                let themes = ThemeRegistry::global(cx).themes();
                assert!(themes.contains_key(family.light) && themes.contains_key(family.dark), "{}", family.name);
                choose(family.name, family.name, cx);
                assert_eq!(super::family(cx), family.name);
                for appearance in [Appearance::Light, Appearance::Dark] {
                    apply_everywhere(appearance, cx);
                    let theme = Theme::global(cx);
                    assert_eq!(theme.mode.is_dark(), appearance == Appearance::Dark, "{}", family.name);
                    let side = if appearance == Appearance::Dark { family.dark } else { family.light };
                    assert_eq!(theme.theme_name().as_ref(), side);
                    grounds.push(hex(theme.background));
                    let palette = format!("background = {}\nforeground = {}\n", hex(theme.background), hex(theme.foreground));
                    assert!(TerminalColors::from_ghostty_theme(&palette).is_ok());
                    let _ = terminal_colors(cx);
                }
            }
            assert!(grounds.chunks(2).all(|sides| sides[0] != sides[1]), "the light side and the dark draw on grounds of their own: {grounds:?}");
            choose("No such theme", "Gruvbox", cx);
            apply_everywhere(Appearance::Light, cx);
            assert_eq!(family(cx), "Ayu", "one not known is the first");
            apply_everywhere(Appearance::Dark, cx);
            assert_eq!((family(cx), Theme::global(cx).theme_name().as_ref()), ("Gruvbox", "Gruvbox Dark"), "each side its own family");
        });
    }
}
