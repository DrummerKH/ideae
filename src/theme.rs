//! The themes of the UI and the terminals, each light/dark/auto; the font of code.

use gpui_cn::terminal::{TerminalAppearance, TerminalColors};
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

/// The code font of the editor, diffs and lists of code until Settings
/// names another; terminals keep gpui-cn's JetBrains Mono, which they draw
/// without ligatures anyway. Embedded (SIL OFL 1.1,
/// `assets/fonts/OFL-FiraCode.txt`), so the machine need not have it.
pub const CODE_FONT: &str = "Fira Code";

/// The code fonts that come with the app: Fira Code, and gpui-cn's
/// JetBrains Mono, which the terminals draw with.
pub const BUILT_IN_FONTS: [&str; 2] = [CODE_FONT, "JetBrains Mono"];

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

/// Draws code in `family`: one built in, or one installed. Any other is
/// Fira Code: GPUI panics at the first line laid out in a family it cannot
/// find.
pub fn set_code_font(family: &str, cx: &mut App) {
    let family = if BUILT_IN_FONTS.contains(&family) || installed(family, cx) { family } else { CODE_FONT };
    let theme = Theme::global_mut(cx);
    if theme.mono_font_family.as_ref() != family {
        theme.mono_font_family = SharedString::from(family.to_string());
        cx.refresh_windows();
    }
}

/// Whether the machine has `family`. The families are listed once: that
/// takes about a tenth of a second, and only a font not built in asks.
fn installed(family: &str, cx: &App) -> bool {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names()).iter().any(|name| name == family)
}

/// The monospaced families installed, found once off the UI thread; None
/// while they are looked for.
struct Monospaced(Option<Vec<SharedString>>);
impl Global for Monospaced {}

/// The code fonts to choose from: those built in, then the monospaced ones
/// installed once they are found, the windows drawn again then.
pub fn code_fonts(cx: &mut App) -> Vec<SharedString> {
    let mut fonts: Vec<SharedString> = BUILT_IN_FONTS.into_iter().map(SharedString::from).collect();
    match cx.try_global::<Monospaced>() {
        Some(Monospaced(Some(found))) => fonts.extend(found.iter().filter(|name| !BUILT_IN_FONTS.contains(&name.as_ref())).cloned()),
        Some(Monospaced(None)) => {}
        None => {
            cx.set_global(Monospaced(None));
            let text = cx.text_system().clone();
            let found = cx.background_spawn(async move { monospaced(&text) });
            cx.spawn(async move |cx| {
                let found = found.await;
                let _ = cx.update(|cx| {
                    cx.set_global(Monospaced(Some(found)));
                    cx.refresh_windows();
                });
            })
            .detach();
        }
    }
    fonts
}

/// The families whose letters are all as wide, the ones a family only
/// stands in for left out.
fn monospaced(text: &TextSystem) -> Vec<SharedString> {
    let size = px(16.);
    text.all_font_names()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .filter(|name| {
            let id = text.resolve_font(&font(name.clone()));
            if text.get_font_for_id(id).is_none_or(|found| found.family.as_ref() != name) {
                return false;
            }
            let widths: Vec<f32> = ['i', 'M', 'W', '.'].into_iter().filter_map(|c| text.advance(id, size, c).ok()).map(|a| f32::from(a.width)).collect();
            widths.len() == 4 && widths.iter().all(|w| (w - widths[0]).abs() < 0.01)
        })
        .map(SharedString::from)
        .collect()
}

/// The cell of every terminal as Settings sizes it.
pub fn terminal_appearance(settings: &crate::state::Settings) -> TerminalAppearance {
    let (least, most) = crate::state::SPACING;
    let fraction = |percent: u32| percent.clamp(least, most) as f32 / 100. - 1.;
    TerminalAppearance::default()
        .with_cell_width_adjust(fraction(settings.terminal_letter_spacing))
        .with_cell_height_adjust(fraction(settings.terminal_line_height))
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
    use super::{Appearance, BUILT_IN_FONTS, CODE_FONT, FAMILIES, apply_everywhere, choose, family, hex, init, set_code_font, terminal_appearance, terminal_colors};
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

    /// A code font built in is taken as it is; one the machine lacks would
    /// panic at the first line laid out, so Fira Code stays.
    #[gpui_kit::test]
    fn a_code_font_the_machine_lacks_is_fira_code(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
            set_code_font(BUILT_IN_FONTS[1], cx);
            assert_eq!(Theme::global(cx).mono_font_family.as_ref(), BUILT_IN_FONTS[1]);
            set_code_font("No Such Mono", cx);
            assert_eq!(Theme::global(cx).mono_font_family.as_ref(), CODE_FONT);
        });
    }

    /// Percent in Settings, a fraction of the font's cell for the terminal.
    #[test]
    fn a_terminal_s_cell_is_sized_by_percent() {
        let settings = crate::state::Settings { terminal_letter_spacing: 95, terminal_line_height: 103, ..Default::default() };
        let appearance = terminal_appearance(&settings);
        assert!((appearance.cell_width_adjust() + 0.05).abs() < 1e-6);
        assert!((appearance.cell_height_adjust() - 0.03).abs() < 1e-6);
        let plain = terminal_appearance(&crate::state::Settings::default());
        assert_eq!((plain.cell_width_adjust(), plain.cell_height_adjust()), (0., 0.));
    }
}
