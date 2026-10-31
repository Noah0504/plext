//! The theme picker (`T`): a list of every theme that previews each one as
//! you move, and saves the one you keep to config.toml.

use super::playlists::Modal;
use super::*;
use crate::theme::{self, Choice, Settings, AUTO};

/// What decides the theme while Plext runs.
#[derive(Default)]
pub(super) struct ThemeSetup {
    pub(super) settings: Settings,
    /// What the terminal said about its background (asked once, for auto).
    term_light: Option<bool>,
    asked_terminal: bool,
    /// Save choices to config.toml and ask the terminal (off in tests).
    live: bool,
}

impl App {
    /// Uses the configured theme, saving choices made with `T` back to
    /// config.toml. For auto, this asks the terminal about its background,
    /// so the terminal must already be in raw mode.
    pub fn use_theme(&mut self, settings: Settings) {
        self.theme = ThemeSetup { settings, term_light: None, asked_terminal: false, live: true };
        let problems = self.apply_theme(&self.theme.settings.clone());
        if !problems.is_empty() {
            for p in &problems {
                crate::log::write(&format!("theme: {}", p));
            }
            self.warn(format!("config.toml: {}", problems.join("; ")));
        }
    }

    /// Draws with the theme `settings` describe; returns any problems.
    fn apply_theme(&mut self, settings: &Settings) -> Vec<String> {
        if settings.is_auto() && !self.theme.asked_terminal && self.theme.live {
            self.theme.term_light = crate::termbg::terminal_is_light();
            self.theme.asked_terminal = true;
        }
        let (t, problems) = theme::resolve(settings, self.theme.term_light);
        theme::set_current(t);
        problems
    }

    /// The id of the theme chosen in the config.
    fn chosen_theme(&self) -> &'static str {
        self.theme.settings.theme.as_deref().and_then(theme::canonical).unwrap_or(theme::DEFAULT_DARK)
    }

    pub(super) fn open_themes(&mut self) {
        let chosen = self.chosen_theme();
        let sel = theme::choices().iter().position(|c| c.id == chosen).unwrap_or(0);
        let background = self.theme.settings.background.unwrap_or(true);
        self.modal = Some(Modal::Themes { sel, background });
    }

    /// The settings with `id` and `background` tried on.
    fn trying(&self, id: &str, background: bool) -> Settings {
        let mut s = self.theme.settings.clone();
        s.theme = Some(id.to_string());
        s.background = (!background).then_some(false);
        s
    }

    pub(super) fn themes_key(&mut self, code: KeyCode, sel: usize, background: bool) -> Option<Modal> {
        let list = theme::choices();
        let last = list.len() - 1;
        let (sel, background) = match code {
            KeyCode::Up | KeyCode::Char('k') => (sel.saturating_sub(1), background),
            KeyCode::Down | KeyCode::Char('j') => ((sel + 1).min(last), background),
            KeyCode::PageUp => (sel.saturating_sub(10), background),
            KeyCode::PageDown => ((sel + 10).min(last), background),
            KeyCode::Home | KeyCode::Char('g') => (0, background),
            KeyCode::End | KeyCode::Char('G') => (last, background),
            KeyCode::Char('b') => (sel, !background),
            KeyCode::Enter => {
                let id = list[sel].id;
                let settings = self.trying(id, background);
                self.apply_theme(&settings);
                self.theme.settings = settings;
                self.save_theme();
                return None;
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => {
                // Back to what it was.
                let before = self.theme.settings.clone();
                self.apply_theme(&before);
                return None;
            }
            _ => return Some(Modal::Themes { sel, background }),
        };
        let trying = self.trying(list[sel].id, background);
        self.apply_theme(&trying);
        Some(Modal::Themes { sel, background })
    }

    fn save_theme(&mut self) {
        let name = theme_label(self.chosen_theme(), self.theme.term_light);
        if !self.theme.live {
            return self.say(format!("Theme: {}", name));
        }
        let saved = crate::config::Config::load().and_then(|mut cfg| {
            cfg.theme = self.theme.settings.theme.clone();
            cfg.theme_background = self.theme.settings.background;
            cfg.save()
        });
        match saved {
            Ok(()) => self.say(format!("Theme: {} (saved)", name)),
            Err(e) => self.warn(format!("Theme {} is on, but couldn't be saved: {:#}", name, e)),
        }
    }

    pub(super) fn draw_themes(&mut self, f: &mut Frame, sel: usize, background: bool) {
        let area = f.area();
        let list = theme::choices();
        let chosen = self.chosen_theme();
        let t = th();
        let w: u16 = 50.min(area.width.saturating_sub(4));
        let inner_w = w.saturating_sub(4) as usize;
        let rows = (area.height.saturating_sub(10) as usize).clamp(3, list.len());
        let first = sel.saturating_sub(rows.saturating_sub(1)).min(list.len() - rows);
        let mut lines: Vec<Line> = Vec::new();
        for (i, c) in list.iter().enumerate().skip(first).take(rows) {
            let on = i == sel;
            let mark = if c.id == chosen { "✓ " } else { "  " };
            let tag = tag_for(c, self.theme.term_light);
            let name = truncate(c.name, inner_w.saturating_sub(tag.chars().count() + 4));
            let pad = inner_w.saturating_sub(name.chars().count() + tag.chars().count() + 3);
            let base = if on { Style::default().bg(t.sel) } else { Style::default() };
            lines.push(Line::from(vec![
                Span::styled(format!(" {}", mark), base.fg(t.accent)),
                Span::styled(
                    name,
                    if on { base.fg(t.bright).add_modifier(Modifier::BOLD) } else { base.fg(t.fg) },
                ),
                Span::styled(format!("{}{} ", " ".repeat(pad), tag), base.fg(t.faint)),
            ]));
        }
        let paints = !matches!(list[sel].id, "plext" | "terminal");
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" b ", Style::default().fg(t.bright)),
            Span::styled(
                if !paints {
                    "background: the terminal's".to_string()
                } else if background {
                    "background: the theme's".to_string()
                } else {
                    "background: the terminal's (b: the theme's)".to_string()
                },
                Style::default().fg(t.dim),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " ↑↓ try   enter keep   esc put back",
            Style::default().fg(t.faint),
        )));
        super::playlists::draw_window(f, " Theme ", lines, w);
    }
}

/// "light", or for auto what it's showing now.
fn tag_for(c: &Choice, term_light: Option<bool>) -> String {
    if c.id == AUTO {
        return if term_light == Some(true) { "light now".into() } else { "dark now".into() };
    }
    if c.light {
        "light".into()
    } else {
        String::new()
    }
}

/// A theme's name for messages, with what auto picked.
fn theme_label(id: &str, term_light: Option<bool>) -> String {
    let name = |id: &str| theme::choices().into_iter().find(|c| c.id == id).map(|c| c.name).unwrap_or("Plext");
    if id == AUTO {
        format!("Auto ({} terminal)", if term_light == Some(true) { "light" } else { "dark" })
    } else {
        name(id).to_string()
    }
}

#[cfg(test)]
impl App {
    /// Uses a theme without saving it or asking the terminal.
    pub(super) fn use_theme_for_test(&mut self, id: &str) {
        let settings = Settings { theme: Some(id.to_string()), ..Settings::default() };
        self.apply_theme(&settings);
        self.theme.settings = settings;
    }
}
