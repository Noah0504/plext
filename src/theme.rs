//! Color themes.
//!
//! A theme is the handful of colors the whole interface is drawn with.
//! Plext's own theme ("plext") draws on the terminal's background, as Plext
//! always has; the other named themes paint their own background too (unless
//! `theme_background = false`), so they look right in any terminal. The
//! "terminal" theme uses the terminal's own 16 colors, so Plext matches
//! whatever scheme the terminal has. `theme = "auto"` asks the terminal
//! whether its background is light or dark and picks a theme for each.
//!
//! On top of any theme, single colors can be changed in a `[colors]` table.
//!
//! The album cover keeps its own colors, and in every theme "good" stays
//! green-ish (bit-perfect) and "warn" orange or red (problems).

use ratatui::style::Color;
use std::cell::Cell;
use std::collections::BTreeMap;

/// The colors the interface is drawn with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// The id it was chosen by ("nord"), or "custom" pieces aside.
    pub id: &'static str,
    /// Behind everything; `Color::Reset` leaves the terminal's own.
    pub bg: Color,
    /// Ordinary text.
    pub fg: Color,
    /// Titles, the highlighted row's text.
    pub bright: Color,
    /// Secondary text: details, durations, hints.
    pub dim: Color,
    /// Tertiary text: headings in lists, key hints, numbers.
    pub faint: Color,
    /// Pane edges when not focused.
    pub border: Color,
    /// Behind the highlighted row.
    pub sel: Color,
    /// Plext's highlight color: what's playing, stars, focused titles.
    pub accent: Color,
    /// Good news: bit-perfect, connected, done.
    pub good: Color,
    /// Problems and warnings.
    pub warn: Color,
    /// Half a star: the accent at about half strength.
    pub half_star: Color,
    /// A light theme (dark text on a light background).
    pub light: bool,
}

/// A built-in theme's colors, as 0xRRGGBB.
struct Palette {
    id: &'static str,
    name: &'static str,
    light: bool,
    /// Plext's own theme leaves the terminal's background alone.
    paints: bool,
    bg: u32,
    fg: u32,
    bright: u32,
    /// None: worked out from the text and background colors.
    dim: Option<u32>,
    faint: Option<u32>,
    border: Option<u32>,
    sel: Option<u32>,
    accent: u32,
    good: u32,
    warn: u32,
    half_star: Option<u32>,
}

const fn p(
    id: &'static str,
    name: &'static str,
    light: bool,
    [bg, fg, bright]: [u32; 3],
    [dim, faint, border, sel]: [Option<u32>; 4],
    [accent, good, warn]: [u32; 3],
) -> Palette {
    Palette { id, name, light, paints: true, bg, fg, bright, dim, faint, border, sel, accent, good, warn, half_star: None }
}

const fn s(v: u32) -> Option<u32> {
    Some(v)
}

/// Every built-in theme, in the order the picker lists them. Colors come
/// from each theme's published palette; where a theme has no color for a
/// role (some have no "secondary text"), it's blended from its text and
/// background colors. A few light themes' accents are too pale to read as
/// text on their own background: Catppuccin Latte uses its maroon for
/// warnings, and Everforest Light its red, with its green and aqua 10% darker.
#[rustfmt::skip]
const PALETTES: &[Palette] = &[
    Palette { id: "plext", name: "Plext", light: false, paints: false,
        bg: 0x0d1117, fg: 0xc9d1d9, bright: 0xe6edf3, dim: s(0x8b949e), faint: s(0x6e7681), border: s(0x545c66),
        sel: s(0x1f2630), accent: 0xe5a00d, good: 0x7ee787, warn: 0xf0883e, half_star: s(0x8a6a2a) },
    p("plext-light", "Plext Light", true, [0xffffff, 0x24292f, 0x0d1117],
        [s(0x57606a), s(0x6e7781), s(0xafb8c1), s(0xeaeef2)], [0xa86800, 0x1a7f37, 0xbc4c00]),
    p("catppuccin-mocha", "Catppuccin Mocha", false, [0x1e1e2e, 0xbac2de, 0xcdd6f4],
        [s(0x9399b2), s(0x7f849c), s(0x585b70), s(0x313244)], [0xcba6f7, 0xa6e3a1, 0xfab387]),
    p("catppuccin-macchiato", "Catppuccin Macchiato", false, [0x24273a, 0xb8c0e0, 0xcad3f5],
        [s(0x939ab7), s(0x8087a2), s(0x5b6078), s(0x363a4f)], [0xc6a0f6, 0xa6da95, 0xf5a97f]),
    p("catppuccin-frappe", "Catppuccin Frappé", false, [0x303446, 0xb5bfe2, 0xc6d0f5],
        [s(0x949cbb), s(0x838ba7), s(0x626880), s(0x414559)], [0xca9ee6, 0xa6d189, 0xef9f76]),
    p("catppuccin-latte", "Catppuccin Latte", true, [0xeff1f5, 0x5c5f77, 0x4c4f69],
        [s(0x6c6f85), s(0x8c8fa1), s(0xacb0be), s(0xccd0da)], [0x8839ef, 0x40a02b, 0xe64553]),
    p("dracula", "Dracula", false, [0x282a36, 0xf8f8f2, 0xffffff],
        [None, s(0x6272a4), s(0x6272a4), s(0x44475a)], [0xbd93f9, 0x50fa7b, 0xffb86c]),
    p("alucard", "Dracula Light (Alucard)", true, [0xfffbeb, 0x1f1f1f, 0x000000],
        [None, s(0x6c664b), s(0xcfcfde), s(0xcfcfde)], [0x644ac9, 0x14710a, 0xa34d14]),
    p("everforest-dark", "Everforest Dark", false, [0x2d353b, 0xd3c6aa, 0xfdf6e3],
        [s(0x9da9a0), s(0x859289), s(0x4f585e), s(0x3d484d)], [0xa7c080, 0x83c092, 0xe69875]),
    p("everforest-light", "Everforest Light", true, [0xfdf6e3, 0x5c6a72, 0x2d353b],
        [s(0x829181), s(0x939f91), s(0xbdc3af), s(0xe6e2cc)], [0x7e9000, 0x2f966f, 0xf85552]),
    p("github-dark", "GitHub Dark", false, [0x0d1117, 0xe6edf3, 0xffffff],
        [s(0x9198a1), s(0x6e7681), s(0x3d444d), s(0x21262d)], [0x4493f8, 0x3fb950, 0xdb6d28]),
    p("github-light", "GitHub Light", true, [0xffffff, 0x1f2328, 0x000000],
        [s(0x59636e), s(0x818b98), s(0xd1d9e0), s(0xeff2f5)], [0x0969da, 0x1a7f37, 0xbc4c00]),
    p("gruvbox-dark", "Gruvbox Dark", false, [0x282828, 0xebdbb2, 0xfbf1c7],
        [s(0xa89984), s(0x928374), s(0x665c54), s(0x3c3836)], [0xfabd2f, 0xb8bb26, 0xfe8019]),
    p("gruvbox-light", "Gruvbox Light", true, [0xfbf1c7, 0x3c3836, 0x282828],
        [s(0x7c6f64), s(0x928374), s(0xbdae93), s(0xebdbb2)], [0xb57614, 0x79740e, 0xaf3a03]),
    p("kanagawa-wave", "Kanagawa Wave", false, [0x1f1f28, 0xdcd7ba, 0xf2ecbc],
        [None, s(0x727169), s(0x54546d), s(0x2d4f67)], [0x7e9cd8, 0x98bb6c, 0xffa066]),
    p("kanagawa-lotus", "Kanagawa Lotus", true, [0xf2ecbc, 0x545464, 0x43436c],
        [None, s(0x8a8980), None, None], [0x4d699b, 0x6f894e, 0xcc6d00]),
    p("nord", "Nord", false, [0x2e3440, 0xd8dee9, 0xeceff4],
        [None, s(0x7b88a1), s(0x4c566a), s(0x434c5e)], [0x88c0d0, 0xa3be8c, 0xd08770]),
    p("nord-light", "Nord Light", true, [0xeceff4, 0x3b4252, 0x2e3440],
        [None, None, s(0xc8cfdc), s(0xd8dee9)], [0x5e81ac, 0x557a46, 0xb5523b]),
    p("one-dark", "One Dark", false, [0x282c34, 0xabb2bf, 0xd7dae0],
        [None, s(0x5c6370), s(0x4b5263), s(0x3e4451)], [0x61afef, 0x98c379, 0xd19a66]),
    p("one-light", "One Light", true, [0xfafafa, 0x383a42, 0x232324],
        [None, s(0x8e8f96), s(0xd4d4d6), s(0xe5e5e6)], [0x4078f2, 0x50a14f, 0x986801]),
    p("rose-pine", "Rosé Pine", false, [0x191724, 0xe0def4, 0xe0def4],
        [s(0x908caa), s(0x6e6a86), s(0x524f67), s(0x403d52)], [0xc4a7e7, 0x9ccfd8, 0xeb6f92]),
    p("rose-pine-moon", "Rosé Pine Moon", false, [0x232136, 0xe0def4, 0xe0def4],
        [s(0x908caa), s(0x6e6a86), s(0x56526e), s(0x44415a)], [0xc4a7e7, 0x9ccfd8, 0xeb6f92]),
    p("rose-pine-dawn", "Rosé Pine Dawn", true, [0xfaf4ed, 0x575279, 0x575279],
        [s(0x797593), s(0x9893a5), s(0xcecacd), s(0xdfdad9)], [0x907aa9, 0x286983, 0xb4637a]),
    p("solarized-dark", "Solarized Dark", false, [0x002b36, 0x839496, 0x93a1a1],
        [s(0x748891), s(0x657b83), s(0x586e75), s(0x073642)], [0xb58900, 0x859900, 0xcb4b16]),
    p("solarized-light", "Solarized Light", true, [0xfdf6e3, 0x586e75, 0x073642],
        [s(0x657b83), s(0x839496), s(0x93a1a1), s(0xeee8d5)], [0xb58900, 0x859900, 0xcb4b16]),
    p("tokyo-night", "Tokyo Night", false, [0x1a1b26, 0xa9b1d6, 0xc0caf5],
        [s(0x8089b3), s(0x565f89), s(0x414868), s(0x292e42)], [0x7aa2f7, 0x9ece6a, 0xff9e64]),
    p("tokyo-night-storm", "Tokyo Night Storm", false, [0x24283b, 0xa9b1d6, 0xc0caf5],
        [s(0x8089b3), s(0x5f6896), s(0x414868), s(0x2f3549)], [0x7aa2f7, 0x9ece6a, 0xff9e64]),
    p("tokyo-night-day", "Tokyo Night Day", true, [0xe1e2e7, 0x3760bf, 0x1f2a5c],
        [s(0x6172b0), s(0x8990b3), s(0xa8aecb), s(0xc4c8da)], [0x2e7de9, 0x587539, 0xb15c00]),
];

/// The terminal's own colors: works with any terminal scheme, light or dark.
const TERMINAL: Theme = Theme {
    id: "terminal",
    bg: Color::Reset,
    fg: Color::Reset,
    bright: Color::Reset,
    dim: Color::DarkGray,
    faint: Color::DarkGray,
    border: Color::DarkGray,
    sel: Color::DarkGray,
    accent: Color::Yellow,
    good: Color::Green,
    warn: Color::Red,
    half_star: Color::DarkGray,
    light: false,
};

/// Other names people may type for a theme.
const ALIASES: &[(&str, &str)] = &[
    ("default", "plext"),
    ("catppuccin", "catppuccin-mocha"),
    ("dracula-light", "alucard"),
    ("everforest", "everforest-dark"),
    ("github", "github-dark"),
    ("gruvbox", "gruvbox-dark"),
    ("kanagawa", "kanagawa-wave"),
    ("onedark", "one-dark"),
    ("onelight", "one-light"),
    ("rosepine", "rose-pine"),
    ("solarized", "solarized-dark"),
    ("tokyonight", "tokyo-night"),
];

pub const AUTO: &str = "auto";
pub const DEFAULT_DARK: &str = "plext";
pub const DEFAULT_LIGHT: &str = "plext-light";

/// One entry in the theme picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub id: &'static str,
    pub name: &'static str,
    pub light: bool,
}

/// Every theme by id and name, in picker order (auto and terminal last).
pub fn choices() -> Vec<Choice> {
    let mut v: Vec<Choice> = PALETTES.iter().map(|p| Choice { id: p.id, name: p.name, light: p.light }).collect();
    v.push(Choice { id: "terminal", name: "Terminal colors", light: false });
    v.push(Choice { id: AUTO, name: "Auto (follows the terminal)", light: false });
    v
}

/// The canonical id for a name ("Gruvbox" → "gruvbox-dark"), if it's a theme.
pub fn canonical(name: &str) -> Option<&'static str> {
    let n = name.trim().to_lowercase().replace([' ', '_'], "-").replace('é', "e");
    let n = ALIASES.iter().find(|(a, _)| *a == n).map(|(_, to)| to.to_string()).unwrap_or(n);
    choices().into_iter().map(|c| c.id).find(|id| *id == n)
}

/// What decides the theme, from the config.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// `theme`: an id, "auto", or None for Plext's own.
    pub theme: Option<String>,
    /// `theme_background`: named themes paint their background (default yes).
    pub background: Option<bool>,
    /// `theme_dark` / `theme_light`: what auto picks.
    pub dark: Option<String>,
    pub light: Option<String>,
    /// `[colors]`: single colors changed on top of the theme.
    pub colors: BTreeMap<String, String>,
}

impl Settings {
    pub fn is_auto(&self) -> bool {
        self.theme.as_deref().and_then(canonical) == Some(AUTO)
    }
}

/// The theme the settings describe. `terminal_is_light` is what the
/// terminal said about its background (for auto). Problems (an unknown
/// theme or color) come back as messages; the theme falls back to Plext's
/// own, or the color to the theme's.
pub fn resolve(s: &Settings, terminal_is_light: Option<bool>) -> (Theme, Vec<String>) {
    let mut problems = Vec::new();
    let mut pick = |name: Option<&str>, fallback: &'static str, what: &str| -> &'static str {
        match name {
            None => fallback,
            Some(n) => match canonical(n) {
                Some(AUTO) if what != "theme" => {
                    problems.push(format!("{} can't be \"auto\"; using {}", what, fallback));
                    fallback
                }
                Some(id) => id,
                None => {
                    problems.push(format!("unknown theme \"{}\" in {}; using {}", n, what, fallback));
                    fallback
                }
            },
        }
    };
    let mut id = pick(s.theme.as_deref(), DEFAULT_DARK, "theme");
    if id == AUTO {
        id = if terminal_is_light == Some(true) {
            pick(s.light.as_deref(), DEFAULT_LIGHT, "theme_light")
        } else {
            pick(s.dark.as_deref(), DEFAULT_DARK, "theme_dark")
        };
    }
    let mut theme = builtin(id, s.background.unwrap_or(true));
    for (role, value) in &s.colors {
        match parse_color(value) {
            Some(c) => {
                if !set_role(&mut theme, role, c) {
                    problems.push(format!(
                        "unknown color \"{}\" in [colors] (they are: {})",
                        role,
                        ROLES.join(", ")
                    ));
                }
            }
            None => problems.push(format!("[colors] {} = \"{}\" isn't a color (try \"#e5a00d\" or \"yellow\")", role, value)),
        }
    }
    (theme, problems)
}

/// A built-in theme by id (Plext's own if there's no such theme).
pub fn builtin(id: &str, paint_background: bool) -> Theme {
    if id == "terminal" {
        return TERMINAL;
    }
    let p = PALETTES.iter().find(|p| p.id == id).unwrap_or(&PALETTES[0]);
    let mix = |a: u32, b: u32, t: f32| -> u32 {
        let ch = |sh: u32| -> u32 {
            let (x, y) = (((a >> sh) & 0xff) as f32, ((b >> sh) & 0xff) as f32);
            ((x * t + y * (1.0 - t)).round() as u32).min(255)
        };
        (ch(16) << 16) | (ch(8) << 8) | ch(0)
    };
    Theme {
        id: p.id,
        bg: if p.paints && paint_background { rgb(p.bg) } else { Color::Reset },
        fg: rgb(p.fg),
        bright: rgb(p.bright),
        dim: rgb(p.dim.unwrap_or_else(|| mix(p.fg, p.bg, 0.68))),
        faint: rgb(p.faint.unwrap_or_else(|| mix(p.fg, p.bg, 0.5))),
        border: rgb(p.border.unwrap_or_else(|| mix(p.fg, p.bg, 0.3))),
        sel: rgb(p.sel.unwrap_or_else(|| mix(p.fg, p.bg, 0.12))),
        accent: rgb(p.accent),
        good: rgb(p.good),
        warn: rgb(p.warn),
        half_star: rgb(p.half_star.unwrap_or_else(|| mix(p.accent, p.bg, 0.5))),
        light: p.light,
    }
}

/// The names of the colors `[colors]` can change.
pub const ROLES: &[&str] =
    &["background", "text", "bright", "dim", "faint", "border", "selection", "accent", "good", "warn", "half_star"];

fn set_role(t: &mut Theme, role: &str, c: Color) -> bool {
    let slot = match role.trim().to_lowercase().replace('-', "_").as_str() {
        "background" | "bg" => &mut t.bg,
        "text" | "fg" => &mut t.fg,
        "bright" => &mut t.bright,
        "dim" => &mut t.dim,
        "faint" => &mut t.faint,
        "border" => &mut t.border,
        "selection" | "sel" => &mut t.sel,
        "accent" => &mut t.accent,
        "good" => &mut t.good,
        "warn" => &mut t.warn,
        "half_star" => &mut t.half_star,
        _ => return false,
    };
    *slot = c;
    true
}

/// "#e5a00d", "e5a00d", a terminal color name ("yellow", "bright-black"),
/// a 256-color index ("208"), or "default" for the terminal's own.
pub fn parse_color(s: &str) -> Option<Color> {
    let v = s.trim().to_lowercase().replace(['_', ' '], "-");
    let hex = v.strip_prefix('#').unwrap_or(&v);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return u32::from_str_radix(hex, 16).ok().map(rgb);
    }
    if let Ok(n) = v.parse::<u8>() {
        return Some(Color::Indexed(n));
    }
    Some(match v.as_str() {
        "default" | "none" | "reset" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "bright-black" | "gray" | "grey" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        "bright-white" => Color::White,
        _ => return None,
    })
}

fn rgb(v: u32) -> Color {
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Whether a background color (from the terminal) is light.
pub fn is_light(r: u8, g: u8, b: u8) -> bool {
    luminance(r, g, b) > 0.4
}

/// Relative luminance (WCAG).
fn luminance(r: u8, g: u8, b: u8) -> f64 {
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

// ---------- the theme in use ----------

thread_local! {
    /// The theme the interface draws with. Per thread, so tests drawing in
    /// parallel don't see each other's themes; Plext draws on one thread.
    static CURRENT: Cell<Theme> = Cell::new(builtin(DEFAULT_DARK, true));
}

/// The theme in use.
pub fn current() -> Theme {
    CURRENT.with(|c| c.get())
}

pub fn set_current(t: Theme) {
    CURRENT.with(|c| c.set(t));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_of(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("not an RGB color: {:?}", other),
        }
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance_of(a), luminance_of(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn luminance_of(c: Color) -> f64 {
        let (r, g, b) = rgb_of(c);
        luminance(r, g, b)
    }

    /// Every built-in theme is readable: text on the background and on the
    /// highlighted row, and the accent, good and warn colors on the
    /// background. Solarized is a little under for plain text by design;
    /// it's kept as its authors made it.
    #[test]
    fn every_theme_is_readable() {
        let mut failures = Vec::new();
        for p in PALETTES {
            let t = builtin(p.id, true);
            let bg = rgb(p.bg);
            let text_min = if p.id.starts_with("solarized") { 4.0 } else { 4.5 };
            let checks = [
                ("text", contrast(t.fg, bg), text_min),
                ("bright", contrast(t.bright, bg), 4.5),
                ("bright on selection", contrast(t.bright, t.sel), 4.0),
                ("dim", contrast(t.dim, bg), 3.0),
                ("faint", contrast(t.faint, bg), 2.0),
                ("accent", contrast(t.accent, bg), 2.9),
                ("good", contrast(t.good, bg), 2.9),
                ("warn", contrast(t.warn, bg), 2.9),
                ("selection stands out", contrast(t.sel, bg), 1.1),
            ];
            for (what, got, want) in checks {
                if got < want {
                    failures.push(format!("{}: {} {:.2} < {}", p.id, what, got, want));
                }
            }
            // Secondary text is quieter than ordinary text.
            if (luminance_of(t.dim) - luminance_of(bg)).abs() > (luminance_of(t.fg) - luminance_of(bg)).abs() {
                failures.push(format!("{}: dim is louder than text", p.id));
            }
            assert_eq!(t.light, luminance_of(bg) > 0.4, "{} light flag", p.id);
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    #[test]
    fn ids_are_unique_and_every_light_theme_has_a_dark_one() {
        let ids: Vec<&str> = choices().iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
        assert!(PALETTES.iter().filter(|p| p.light).count() >= 10);
    }

    #[test]
    fn names_aliases_and_spelling_find_themes() {
        assert_eq!(canonical("nord"), Some("nord"));
        assert_eq!(canonical("Gruvbox"), Some("gruvbox-dark"));
        assert_eq!(canonical("Catppuccin Latte"), Some("catppuccin-latte"));
        assert_eq!(canonical("catppuccin_frappé"), Some("catppuccin-frappe"));
        assert_eq!(canonical("rosé pine dawn"), Some("rose-pine-dawn"));
        assert_eq!(canonical("default"), Some("plext"));
        assert_eq!(canonical("AUTO"), Some("auto"));
        assert_eq!(canonical("solarised"), None);
    }

    #[test]
    fn plext_keeps_the_terminals_background_and_others_paint_theirs_unless_told_not_to() {
        assert_eq!(builtin("plext", true).bg, Color::Reset);
        assert_eq!(builtin("plext", true).accent, Color::Rgb(0xe5, 0xa0, 0x0d));
        assert_eq!(builtin("nord", true).bg, Color::Rgb(0x2e, 0x34, 0x40));
        assert_eq!(builtin("nord", false).bg, Color::Reset);
        assert_eq!(builtin("terminal", true).bg, Color::Reset);
    }

    #[test]
    fn auto_follows_the_terminal() {
        let s = Settings { theme: Some("auto".into()), ..Settings::default() };
        assert_eq!(resolve(&s, Some(true)).0.id, "plext-light");
        assert_eq!(resolve(&s, Some(false)).0.id, "plext");
        assert_eq!(resolve(&s, None).0.id, "plext", "unknown: dark, as before");
        let s = Settings {
            theme: Some("auto".into()),
            dark: Some("catppuccin-mocha".into()),
            light: Some("Catppuccin Latte".into()),
            ..Settings::default()
        };
        assert_eq!(resolve(&s, Some(true)).0.id, "catppuccin-latte");
        assert_eq!(resolve(&s, Some(false)).0.id, "catppuccin-mocha");
        assert!(is_light(0xfd, 0xf6, 0xe3) && !is_light(0x28, 0x28, 0x28));
    }

    #[test]
    fn colors_can_be_changed_and_mistakes_are_reported() {
        let mut colors = BTreeMap::new();
        colors.insert("accent".to_string(), "#ff0000".to_string());
        colors.insert("selection".to_string(), "bright-black".to_string());
        colors.insert("background".to_string(), "default".to_string());
        colors.insert("accnet".to_string(), "#00ff00".to_string());
        colors.insert("warn".to_string(), "orangeish".to_string());
        let s = Settings { theme: Some("nord".into()), colors, ..Settings::default() };
        let (t, problems) = resolve(&s, None);
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.sel, Color::DarkGray);
        assert_eq!(t.bg, Color::Reset);
        assert_eq!(t.warn, builtin("nord", true).warn);
        assert_eq!(problems.len(), 2, "{:?}", problems);
        let (t, problems) = resolve(&Settings { theme: Some("solarised".into()), ..Settings::default() }, None);
        assert_eq!(t.id, "plext");
        assert!(problems[0].contains("unknown theme \"solarised\""));
    }

    #[test]
    fn colors_parse_from_hex_names_and_indexes() {
        assert_eq!(parse_color("#E5A00D"), Some(Color::Rgb(0xe5, 0xa0, 0x0d)));
        assert_eq!(parse_color("e5a00d"), Some(Color::Rgb(0xe5, 0xa0, 0x0d)));
        assert_eq!(parse_color("Bright Blue"), Some(Color::LightBlue));
        assert_eq!(parse_color("208"), Some(Color::Indexed(208)));
        assert_eq!(parse_color("default"), Some(Color::Reset));
        assert_eq!(parse_color("#12345"), None);
    }
}
