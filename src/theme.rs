//! Ayu for the UI and the terminals, light/dark/auto.

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

    pub fn icon(self) -> &'static str {
        match self {
            Appearance::Auto => "◐",
            Appearance::Light => "☀",
            Appearance::Dark => "☾",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Appearance::Auto => Appearance::Light,
            Appearance::Light => Appearance::Dark,
            Appearance::Dark => Appearance::Auto,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label().eq_ignore_ascii_case(s))
    }
}

/// Registers Ayu as the light and dark theme.
pub fn init(cx: &mut App) {
    if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(include_str!("../themes/ayu.json")) {
        eprintln!("ayu theme: {error:#}");
        return;
    }
    let themes = ThemeRegistry::global(cx).themes();
    let (light, dark) = (themes.get("Ayu Light").cloned(), themes.get("Ayu Dark").cloned());
    let theme = Theme::global_mut(cx);
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
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
    let palette = if theme.mode.is_dark() { AYU_DARK } else { AYU_LIGHT };
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
