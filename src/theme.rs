//! Colour themes, modelled on torrentui: a small semantic palette, built-in
//! presets, one editable file per theme in `<config dir>/themes/`, and user
//! themes under `themes:` in `config.yml`. The renderer asks for "accent",
//! "error" and so on, never for literal colours.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, anyhow};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// Semantic colours used by the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub fg: Color,
    /// Secondary text: hints, labels, empty cells.
    pub muted: Color,
    /// Titles, key hints, focused borders.
    pub accent: Color,
    pub border: Color,
    /// Background of the selected row. `Color::Reset` falls back to reverse video.
    pub selection_bg: Color,
    /// The page colour. Painted only when the background is opaque; with a
    /// transparent background (see [`Theme::transparent`]) it is `Reset` and
    /// the terminal's own shows through. `default` has none.
    pub bg: Color,
    /// Good outcomes, "connected".
    pub success: Color,
    /// Directories, informational.
    pub info: Color,
    /// Warnings and prompts.
    pub warning: Color,
    pub error: Color,
    /// Symlinks and other asides.
    pub special: Color,
}

impl Theme {
    pub fn base(&self) -> Style {
        Style::new().fg(self.fg).bg(self.bg)
    }

    /// This theme with the background dropped when `on`, so the terminal's
    /// own (possibly translucent) background shows through everywhere.
    pub fn transparent(mut self, on: bool) -> Self {
        if on {
            self.bg = Color::Reset;
        }
        self
    }

    /// Map 24-bit colours onto the 256-colour palette for terminals that don't
    /// advertise truecolor, so every theme stays readable.
    pub fn quantized(mut self, truecolor: bool) -> Self {
        if !truecolor {
            for c in [
                &mut self.fg,
                &mut self.muted,
                &mut self.accent,
                &mut self.border,
                &mut self.selection_bg,
                &mut self.bg,
                &mut self.success,
                &mut self.info,
                &mut self.warning,
                &mut self.error,
                &mut self.special,
            ] {
                if let Color::Rgb(r, g, b) = *c {
                    *c = Color::Indexed(to_256(r, g, b));
                }
            }
        }
        self
    }

    /// Secondary text.
    pub fn dim_style(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn selection(&self) -> Style {
        if self.selection_bg == Color::Reset {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
                .bg(self.selection_bg)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// The selected row of a pane that doesn't have focus.
    pub fn selection_inactive(&self) -> Style {
        match self.selection_bg {
            Color::Reset => Style::new().bg(Color::DarkGray),
            c => Style::new().bg(c),
        }
    }

    pub fn gauge_track(&self) -> Color {
        match self.selection_bg {
            Color::Reset => Color::Black,
            c => c,
        }
    }

    /// Resolve a saved-connection colour name against this theme's palette.
    pub fn conn_color(&self, name: &str) -> Option<Color> {
        Some(match name {
            "red" => self.error,
            "green" => self.success,
            "yellow" => self.warning,
            "blue" => self.info,
            "purple" => self.special,
            "cyan" => self.accent,
            "gray" => self.muted,
            _ => return None,
        })
    }

    /// The six accent colours, handy for swatches.
    pub fn swatch(&self) -> [Color; 6] {
        [
            self.accent,
            self.success,
            self.info,
            self.warning,
            self.error,
            self.special,
        ]
    }
}

/// Whether the terminal advertises 24-bit colour.
pub fn detect_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| matches!(v.as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
}

/// Nearest xterm-256 colour (6×6×6 cube or the 24-step grey ramp).
fn to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] - i32::from(v)).abs())
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let dist = |(r2, g2, b2): (i32, i32, i32)| {
        (i32::from(r) - r2).pow(2) + (i32::from(g) - g2).pow(2) + (i32::from(b) - b2).pow(2)
    };
    let cube_dist = dist((LEVELS[ri], LEVELS[gi], LEVELS[bi]));

    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let gi_idx = ((avg - 8 + 5) / 10).clamp(0, 23);
    let grey = 8 + 10 * gi_idx;
    let grey_dist = dist((grey, grey, grey));

    if grey_dist < cube_dist {
        232 + gi_idx as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Each preset's page colour, by name.
fn preset_bg(name: &str) -> Color {
    rgb(match name {
        "tokyo-night" => 0x1a1b26,
        "catppuccin-frappe" => 0x303446,
        "catppuccin-mocha" => 0x1e1e2e,
        "catppuccin-macchiato" => 0x24273a,
        "catppuccin-latte" => 0xeff1f5,
        "dracula" => 0x282a36,
        "one-dark" => 0x282c34,
        "gruvbox-dark" => 0x282828,
        "nord" => 0x2e3440,
        "solarized-dark" => 0x002b36,
        "rose-pine" => 0x191724,
        "monokai" => 0x272822,
        "github-dark" => 0x0d1117,
        "everforest" => 0x2d353b,
        "kanagawa" => 0x1f1f28,
        _ => 0x000000,
    })
}

/// `c` is: fg, muted, accent, border, selection_bg, success, info,
/// warning, error, special. The page colour comes from [`preset_bg`].
fn preset(name: &str, c: [u32; 10]) -> Theme {
    Theme {
        name: name.to_string(),
        bg: preset_bg(name),
        fg: rgb(c[0]),
        muted: rgb(c[1]),
        accent: rgb(c[2]),
        border: rgb(c[3]),
        selection_bg: rgb(c[4]),
        success: rgb(c[5]),
        info: rgb(c[6]),
        warning: rgb(c[7]),
        error: rgb(c[8]),
        special: rgb(c[9]),
    }
}

/// Plain ANSI colours: works everywhere and follows the terminal's palette.
pub fn default_theme() -> Theme {
    Theme {
        name: "default".into(),
        fg: Color::Reset,
        muted: Color::DarkGray,
        accent: Color::Cyan,
        border: Color::Reset,
        selection_bg: Color::Reset,
        bg: Color::Reset,
        success: Color::Green,
        info: Color::Blue,
        warning: Color::Yellow,
        error: Color::Red,
        special: Color::Magenta,
    }
}

/// All built-in themes; `default` first. The RGB presets need a true-colour
/// terminal.
pub fn builtin_themes() -> Vec<Theme> {
    vec![
        default_theme(),
        preset(
            "tokyo-night",
            [
                0xc0caf5, 0x565f89, 0x7aa2f7, 0x3b4261, 0x283457, 0x9ece6a, 0x7dcfff, 0xe0af68,
                0xf7768e, 0xbb9af7,
            ],
        ),
        preset(
            "catppuccin-frappe",
            [
                0xc6d0f5, 0x737994, 0x8caaee, 0x51576d, 0x51576d, 0xa6d189, 0x85c1dc, 0xe5c890,
                0xe78284, 0xca9ee6,
            ],
        ),
        preset(
            "catppuccin-mocha",
            [
                0xcdd6f4, 0x6c7086, 0x89b4fa, 0x45475a, 0x45475a, 0xa6e3a1, 0x74c7ec, 0xf9e2af,
                0xf38ba8, 0xcba6f7,
            ],
        ),
        preset(
            "catppuccin-macchiato",
            [
                0xcad3f5, 0x6e738d, 0x8aadf4, 0x494d64, 0x494d64, 0xa6da95, 0x7dc4e4, 0xeed49f,
                0xed8796, 0xc6a0f6,
            ],
        ),
        // The light variant: it needs the painted background (or a light
        // terminal) to be readable.
        preset(
            "catppuccin-latte",
            [
                0x4c4f69, 0x9ca0b0, 0x1e66f5, 0xbcc0cc, 0xbcc0cc, 0x40a02b, 0x209fb5, 0xdf8e1d,
                0xd20f39, 0x8839ef,
            ],
        ),
        preset(
            "dracula",
            [
                0xf8f8f2, 0x6272a4, 0xbd93f9, 0x44475a, 0x44475a, 0x50fa7b, 0x8be9fd, 0xf1fa8c,
                0xff5555, 0xff79c6,
            ],
        ),
        preset(
            "one-dark",
            [
                0xabb2bf, 0x5c6370, 0x61afef, 0x4b5263, 0x3e4451, 0x98c379, 0x56b6c2, 0xe5c07b,
                0xe06c75, 0xc678dd,
            ],
        ),
        preset(
            "gruvbox-dark",
            [
                0xebdbb2, 0x928374, 0x83a598, 0x665c54, 0x504945, 0xb8bb26, 0x8ec07c, 0xfabd2f,
                0xfb4934, 0xd3869b,
            ],
        ),
        preset(
            "nord",
            [
                0xd8dee9, 0x616e88, 0x88c0d0, 0x4c566a, 0x434c5e, 0xa3be8c, 0x81a1c1, 0xebcb8b,
                0xbf616a, 0xb48ead,
            ],
        ),
        preset(
            "solarized-dark",
            [
                0x839496, 0x586e75, 0x268bd2, 0x586e75, 0x073642, 0x859900, 0x2aa198, 0xb58900,
                0xdc322f, 0xd33682,
            ],
        ),
        preset(
            "rose-pine",
            [
                0xe0def4, 0x6e6a86, 0xc4a7e7, 0x403d52, 0x403d52, 0x9ccfd8, 0xebbcba, 0xf6c177,
                0xeb6f92, 0x908caa,
            ],
        ),
        preset(
            "monokai",
            [
                0xf8f8f2, 0x75715e, 0xfd971f, 0x49483e, 0x49483e, 0xa6e22e, 0x66d9ef, 0xe6db74,
                0xf92672, 0xae81ff,
            ],
        ),
        preset(
            "github-dark",
            [
                0xc9d1d9, 0x8b949e, 0x58a6ff, 0x30363d, 0x30363d, 0x3fb950, 0x79c0ff, 0xd29922,
                0xf85149, 0xbc8cff,
            ],
        ),
        preset(
            "everforest",
            [
                0xd3c6aa, 0x859289, 0x7fbbb3, 0x4f585e, 0x475258, 0xa7c080, 0x83c092, 0xdbbc7f,
                0xe67e80, 0xd699b6,
            ],
        ),
        preset(
            "kanagawa",
            [
                0xdcd7ba, 0x727169, 0x7e9cd8, 0x54546d, 0x2d4f67, 0x98bb6c, 0x7fb4ca, 0xe6c384,
                0xc34043, 0x957fb8,
            ],
        ),
    ]
}

/// Lower-case, accent-free, punctuation-free form used to match theme names, so
/// `Catppuccin Frappé`, `catppuccin_frappe` and `catpuccin-frappe` all agree.
pub fn normalize(name: &str) -> String {
    let s: String = name
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'é' { 'e' } else { c })
        .filter(char::is_ascii_alphanumeric)
        .collect();
    // A very common misspelling.
    match s.strip_prefix("catpuccin") {
        Some(rest) => format!("catppuccin{rest}"),
        None => s,
    }
}

/// A user-defined theme from the config file. Every colour is optional and
/// falls back to `base` (a built-in theme, default `default`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSpec {
    pub base: Option<String>,
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub muted: Option<String>,
    pub accent: Option<String>,
    pub border: Option<String>,
    pub selection_bg: Option<String>,
    pub success: Option<String>,
    pub info: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
    pub special: Option<String>,
}

fn color(theme: &str, field: &str, value: &Option<String>, fallback: Color) -> Result<Color> {
    match value {
        None => Ok(fallback),
        Some(v) => Color::from_str(v.trim()).map_err(|_| {
            anyhow!(
                "theme '{theme}': invalid colour '{v}' for '{field}' \
                 (use #rrggbb, a name like 'red', or a 0-255 index)"
            )
        }),
    }
}

impl ThemeSpec {
    /// `same_name` is a theme already known under this name (a built-in a
    /// file or the config tweaks, or a file the config tweaks): without an
    /// explicit `base`, unset colours come from it, else from `default`.
    fn resolve(&self, name: &str, builtins: &[Theme], same_name: Option<&Theme>) -> Result<Theme> {
        let base = match &self.base {
            None => same_name.cloned().unwrap_or_else(default_theme),
            Some(b) => find(builtins, b)
                .cloned()
                .ok_or_else(|| anyhow!("theme '{name}': unknown base theme '{b}'"))?,
        };
        Ok(Theme {
            name: name.to_string(),
            bg: color(name, "bg", &self.bg, base.bg)?,
            fg: color(name, "fg", &self.fg, base.fg)?,
            muted: color(name, "muted", &self.muted, base.muted)?,
            accent: color(name, "accent", &self.accent, base.accent)?,
            border: color(name, "border", &self.border, base.border)?,
            selection_bg: color(name, "selection_bg", &self.selection_bg, base.selection_bg)?,
            success: color(name, "success", &self.success, base.success)?,
            info: color(name, "info", &self.info, base.info)?,
            warning: color(name, "warning", &self.warning, base.warning)?,
            error: color(name, "error", &self.error, base.error)?,
            special: color(name, "special", &self.special, base.special)?,
        })
    }
}

/// A colour as it is written in a theme file; the inverse of what
/// `Color::from_str` accepts.
fn color_text(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => n.to_string(),
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "darkgray".into(),
        Color::LightRed => "lightred".into(),
        Color::LightGreen => "lightgreen".into(),
        Color::LightYellow => "lightyellow".into(),
        Color::LightBlue => "lightblue".into(),
        Color::LightMagenta => "lightmagenta".into(),
        Color::LightCyan => "lightcyan".into(),
        Color::White => "white".into(),
    }
}

/// The text of a theme's file: every colour spelled out, so the file is a
/// complete, editable copy.
pub fn theme_file_text(theme: &Theme) -> String {
    let mut out = format!(
        "# remotefiletui theme \"{}\". The file name is the theme's name.\n\
         # Colours: #rrggbb, an ANSI name (red, darkgray, reset, ...) or 0-255.\n\
         # Add `base: <built-in theme>` to fill in any colour you remove.\n\
         # `bg` is the page colour; `transparent_background: true` in config.yml (or B\n\
         # in the app) lets the terminal's own background show instead.\n",
        theme.name
    );
    for (key, c) in [
        ("bg", theme.bg),
        ("fg", theme.fg),
        ("muted", theme.muted),
        ("accent", theme.accent),
        ("border", theme.border),
        ("selection_bg", theme.selection_bg),
        ("success", theme.success),
        ("info", theme.info),
        ("warning", theme.warning),
        ("error", theme.error),
        ("special", theme.special),
    ] {
        out.push_str(&format!("{key}: \"{}\"\n", color_text(c)));
    }
    out
}

fn is_theme_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "yml" || e == "yaml")
}

/// Write a file for each built-in theme that has none yet (matched by
/// normalised name, so `Nord.yml` counts as `nord`). Best effort: a read-only
/// config directory just means the built-ins stay in the code.
pub fn seed_theme_files(dir: &Path) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let existing: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_theme_file(p))
        .filter_map(|p| p.file_stem().map(|s| normalize(&s.to_string_lossy())))
        .collect();
    for theme in builtin_themes() {
        if !existing.contains(&normalize(&theme.name)) {
            let _ = crate::config::write_atomic(
                &dir.join(format!("{}.yml", theme.name)),
                &theme_file_text(&theme),
            );
        }
    }
}

/// The theme files in a directory as `(name, spec)`, sorted by file name.
fn read_theme_files(dir: &Path) -> Result<Vec<(String, ThemeSpec)>> {
    let mut paths: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    paths.retain(|p| is_theme_file(p) && p.is_file());
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = fs::read_to_string(&path)
                .with_context(|| format!("cannot read theme file {}", path.display()))?;
            let blank = text
                .lines()
                .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
            let spec = if blank {
                ThemeSpec::default()
            } else {
                serde_yaml_ng::from_str(&text)
                    .with_context(|| format!("invalid theme file {}", path.display()))?
            };
            Ok((name, spec))
        })
        .collect()
}

/// Every theme, in the order the picker shows them: the built-ins (in their
/// order, as edited in their files), then theme files that are new, then the
/// config's own `themes:`. Later sources replace earlier ones by name.
///
/// `dir` is first filled in with a file for each built-in that lacks one.
pub fn load_themes(dir: &Path, custom: &BTreeMap<String, ThemeSpec>) -> Result<Vec<Theme>> {
    seed_theme_files(dir);
    let builtins = builtin_themes();
    let mut all = builtins.clone();
    let from_files = read_theme_files(dir)?;
    for (name, spec) in from_files.iter().map(|(n, s)| (n, s)).chain(custom.iter()) {
        merge_theme(&mut all, &builtins, name, spec)?;
    }
    Ok(all)
}

/// Add or replace one theme, layering it over any theme of the same name.
fn merge_theme(
    all: &mut Vec<Theme>,
    builtins: &[Theme],
    name: &str,
    spec: &ThemeSpec,
) -> Result<()> {
    let at = all
        .iter()
        .position(|t| normalize(&t.name) == normalize(name));
    let theme = spec.resolve(name, builtins, at.map(|i| &all[i]))?;
    match at {
        // Keep the earlier spelling of the name.
        Some(i) => {
            let keep = all[i].name.clone();
            all[i] = Theme {
                name: keep,
                ..theme
            };
        }
        None => all.push(theme),
    }
    Ok(())
}

/// Built-in themes plus the config's own, without any files (tests).
/// Built-in themes plus the config's own, without any theme files (used when
/// there is no config directory, and by tests).
pub fn all_themes(custom: &BTreeMap<String, ThemeSpec>) -> Result<Vec<Theme>> {
    let builtins = builtin_themes();
    let mut all = builtins.clone();
    for (name, spec) in custom {
        merge_theme(&mut all, &builtins, name, spec)?;
    }
    Ok(all)
}

pub fn all_themes_unfiled(custom: &BTreeMap<String, ThemeSpec>) -> Vec<Theme> {
    all_themes(custom).unwrap_or_else(|_| builtin_themes())
}

pub fn find<'a>(themes: &'a [Theme], name: &str) -> Option<&'a Theme> {
    position(themes, name).map(|i| &themes[i])
}

pub fn position(themes: &[Theme], name: &str) -> Option<usize> {
    let wanted = normalize(name);
    themes.iter().position(|t| normalize(&t.name) == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_builtins_exist() {
        let all = builtin_themes();
        for n in [
            "Tokyo Night",
            "Catppuccin Frappé",
            "Catpuccin Mocha",
            "Catppuccin Macchiato",
            "catppuccin_latte",
            "dracula",
            "One_Dark",
            "Gruvbox Dark",
            "NORD",
            "Solarized-Dark",
            "default",
        ] {
            assert!(find(&all, n).is_some(), "missing {n}");
        }
        assert!(find(&all, "nope").is_none());
        assert_eq!(all[0].name, "default");
    }

    #[test]
    fn names_are_unique_after_normalizing() {
        let all = builtin_themes();
        let mut names: Vec<_> = all.iter().map(|t| normalize(&t.name)).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }

    #[test]
    fn every_palette_is_distinct_enough_to_read() {
        for t in builtin_themes().iter().skip(1) {
            assert_ne!(t.fg, t.bg, "{}", t.name);
            assert_ne!(t.accent, t.bg, "{}", t.name);
            assert_ne!(t.selection_bg, t.bg, "{}", t.name);
        }
    }

    #[test]
    fn rgb_to_256() {
        assert_eq!(to_256(0, 0, 0), 16);
        assert_eq!(to_256(255, 255, 255), 231);
        assert_eq!(to_256(255, 0, 0), 196);
        assert_eq!(to_256(0, 0, 255), 21);
        assert!((232..=255).contains(&to_256(128, 128, 128)));
    }

    #[test]
    fn quantizing_only_touches_rgb() {
        let all = builtin_themes();
        let d = find(&all, "dracula").unwrap().clone();
        assert_eq!(d.clone().quantized(true), d);
        assert!(matches!(d.quantized(false).accent, Color::Indexed(_)));
        let def = all[0].clone();
        assert_eq!(def.clone().quantized(false), def);
    }

    #[test]
    fn transparent_drops_only_the_background() {
        let all = builtin_themes();
        let t = find(&all, "catppuccin-mocha").unwrap().clone();
        let tr = t.clone().transparent(true);
        assert_eq!(tr.bg, Color::Reset);
        assert_eq!(tr.fg, t.fg);
        assert_eq!(t.clone().transparent(false), t);
        assert_eq!(all[0].clone().transparent(true), all[0]);
    }

    #[test]
    fn connection_colours_follow_the_palette() {
        let all = builtin_themes();
        let d = find(&all, "dracula").unwrap();
        assert_eq!(d.conn_color("red"), Some(Color::Rgb(0xff, 0x55, 0x55)));
        assert_eq!(d.conn_color("nope"), None);
    }

    fn spec(base: Option<&str>, accent: Option<&str>) -> ThemeSpec {
        ThemeSpec {
            base: base.map(str::to_string),
            accent: accent.map(str::to_string),
            ..ThemeSpec::default()
        }
    }

    #[test]
    fn custom_themes_inherit_from_a_base() {
        let mut custom = BTreeMap::new();
        custom.insert("mine".to_string(), spec(Some("dracula"), Some("#ff0000")));
        let all = all_themes(&custom).unwrap();
        let t = find(&all, "mine").unwrap();
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.fg, find(&all, "dracula").unwrap().fg);
    }

    #[test]
    fn a_config_entry_with_a_builtin_name_tweaks_it_in_place() {
        let mut custom = BTreeMap::new();
        custom.insert("Nord".to_string(), spec(None, Some("#123456")));
        let all = all_themes(&custom).unwrap();
        let n = all.iter().filter(|t| normalize(&t.name) == "nord").count();
        assert_eq!(n, 1);
        let t = find(&all, "nord").unwrap();
        assert_eq!(t.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(t.fg, Color::Rgb(0xd8, 0xde, 0xe9), "other colours kept");
    }

    #[test]
    fn bad_colours_and_bases_are_errors_that_name_the_theme() {
        let mut custom = BTreeMap::new();
        custom.insert("bad".to_string(), spec(None, Some("not-a-colour")));
        let e = all_themes(&custom).unwrap_err().to_string();
        assert!(e.contains("bad") && e.contains("not-a-colour"), "{e}");
        let mut custom = BTreeMap::new();
        custom.insert("x".to_string(), spec(Some("nonexistent"), None));
        assert!(
            all_themes(&custom)
                .unwrap_err()
                .to_string()
                .contains("unknown base")
        );
    }

    #[test]
    fn theme_files_are_seeded_read_and_layered() {
        let dir = std::env::temp_dir().join(format!("rftui-themes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let themes = load_themes(&dir, &BTreeMap::new()).unwrap();
        assert!(
            dir.join("dracula.yml").is_file(),
            "built-ins are written out"
        );
        assert_eq!(themes.len(), builtin_themes().len());
        // Editing a seeded file changes that theme; a new file adds one.
        let text = fs::read_to_string(dir.join("dracula.yml")).unwrap();
        fs::write(dir.join("dracula.yml"), text.replace("#bd93f9", "#010203")).unwrap();
        fs::write(dir.join("mine.yml"), "base: nord\naccent: \"#0a0b0c\"\n").unwrap();
        let themes = load_themes(&dir, &BTreeMap::new()).unwrap();
        assert_eq!(
            find(&themes, "dracula").unwrap().accent,
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            find(&themes, "mine").unwrap().accent,
            Color::Rgb(10, 11, 12)
        );
        // An unknown key in a theme file is a hard error naming the file.
        fs::write(dir.join("oops.yml"), "acent: red\n").unwrap();
        let e = format!("{:#}", load_themes(&dir, &BTreeMap::new()).unwrap_err());
        assert!(e.contains("oops.yml"), "{e}");
        let _ = fs::remove_dir_all(&dir);
    }
}
