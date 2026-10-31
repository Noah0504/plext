//! The key legend at the bottom (it changes with where you are) and the
//! `?` window listing every shortcut.

use super::playlists::{draw_window, Modal};
use super::*;

/// Every shortcut, by section, for the help window.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Browsing",
        &[
            ("↑ ↓  j k", "Move"),
            ("PgUp PgDn", "Move a page"),
            ("Home End  g G", "Jump to the top / bottom"),
            ("→  l", "Open an artist; go to an album's or playlist's tracks"),
            ("←  h", "Close / go back"),
            ("Tab", "Switch pane"),
            ("v", "Switch view: Artists → Playlists → Discover"),
            ("/", "Filter the list (Esc clears)"),
        ],
    ),
    (
        "Playing",
        &[
            ("Enter", "Play the album or playlist, replacing the queue"),
            ("Space", "Pause / resume"),
            ("n  p", "Next / previous track"),
            (".  ]   ,  [", "Seek forward / back 10 s"),
            ("s", "Stop"),
        ],
    ),
    (
        "Queue",
        &[
            ("a", "Add to the end of the queue"),
            ("A", "Play next, after the current track"),
            ("u", "Show the queue (u or ← goes back)"),
            ("z", "Shuffle on / off"),
            ("r", "Repeat: off → queue → track"),
            ("", "In the queue:"),
            ("Enter", "Play the highlighted track"),
            ("d", "Remove it"),
            ("J  K", "Move it down / up"),
            ("c", "Clear everything after the playing track"),
            ("S", "Save the queue as a new playlist"),
        ],
    ),
    (
        "Playlists",
        &[
            ("P", "Add to a playlist, or start a new one"),
            ("R", "Rename the highlighted playlist"),
            ("D", "Delete it (asks first)"),
            ("", "In a playlist's tracks:"),
            ("d", "Remove the track"),
            ("J  K", "Move it down / up"),
            ("", "Smart playlists are built by Plex from rules, so they can't be edited."),
        ],
    ),
    (
        "Discover and mixes",
        &[
            ("Enter", "On a station or mix in Discover: play it"),
            ("→", "On a shelf in Discover: open its albums and artists"),
            ("m", "Start a mix: track radio from a track, an artist mix from an artist"),
            ("M", "Sonic adventure from what's playing to the highlighted track"),
            ("", "Stations and mixes keep adding tracks until you play something else."),
            ("", "Mixes need Plex Pass and sonic analysis turned on for the library."),
        ],
    ),
    (
        "Ratings",
        &[("1 – 5", "Rate the track that many stars"), ("-  =", "Half a star less / more"), ("0", "Clear the rating")],
    ),
    ("Other", &[("?  F1", "This help"), ("q  Ctrl+C", "Quit")]),
];

const HELP_W: u16 = 78;
const KEY_W: usize = 16;

impl App {
    /// The keys worth knowing where you are now.
    fn legend(&self) -> Vec<(&'static str, &'static str)> {
        if self.filtering {
            return vec![("type", "to filter"), ("enter", "done"), ("esc", "clear")];
        }
        let smart = self.shown_playlist.as_deref().and_then(|k| self.playlist(k)).map(|p| p.smart).unwrap_or(false);
        match (self.focus, self.right, self.view) {
            (Focus::Tracks, RightView::Queue, _) => vec![
                ("enter", "play"),
                ("d", "remove"),
                ("J/K", "reorder"),
                ("c", "clear upcoming"),
                ("S", "save as playlist"),
                ("P", "add to playlist"),
                ("u", "back"),
                ("space", "pause"),
            ],
            (Focus::Tracks, RightView::Album, LeftView::Discover) => vec![
                ("↑↓", "move"),
                ("enter", "play / open"),
                ("a/A", "add/play next"),
                ("m", "artist mix"),
                ("←", "back"),
                ("v", "artists"),
                ("space", "pause"),
            ],
            (Focus::Tracks, RightView::Album, LeftView::Playlists) if !smart => vec![
                ("enter", "play from here"),
                ("a/A", "add/play next"),
                ("d", "remove"),
                ("J/K", "reorder"),
                ("P", "add to playlist"),
                ("0-5", "rate"),
                ("←", "back"),
                ("space", "pause"),
            ],
            (Focus::Tracks, RightView::Album, _) => vec![
                ("enter", "play from here"),
                ("a/A", "add/play next"),
                ("P", "add to playlist"),
                ("0-5", "rate"),
                ("←", "back"),
                ("u", "queue"),
                ("space", "pause"),
                ("n/p", "next/prev"),
            ],
            (Focus::Tree, _, LeftView::Playlists) => vec![
                ("↑↓", "move"),
                ("→", "tracks"),
                ("enter", "play"),
                ("a/A", "add/play next"),
                ("R", "rename"),
                ("D", "delete"),
                ("v", "artists"),
                ("u", "queue"),
                ("space", "pause"),
            ],
            (Focus::Tree, _, LeftView::Discover) => vec![
                ("↑↓", "move"),
                ("enter", "play"),
                ("→", "open shelf"),
                ("m", "mix from what's playing"),
                ("v", "artists"),
                ("u", "queue"),
                ("space", "pause"),
                ("n/p", "next/prev"),
            ],
            (Focus::Tree, _, LeftView::Artists) => vec![
                ("↑↓", "move"),
                ("→/←", "open/close"),
                ("enter", "play"),
                ("a/A", "add/play next"),
                ("P", "add to playlist"),
                ("v", "playlists"),
                ("u", "queue"),
                ("space", "pause"),
                ("n/p", "next/prev"),
            ],
        }
    }

    /// The legend, with "? help" always on the right; keys that don't fit
    /// are left off the end.
    pub(super) fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let help = Line::from(vec![
            Span::styled("?", Style::default().fg(FG)),
            Span::styled(" all keys ", Style::default().fg(FAINT)),
        ]);
        let help_w = help.width() as u16;
        let room = area.width.saturating_sub(help_w + 2) as usize;
        let mut spans = vec![Span::raw(" ")];
        let mut used = 1;
        for (k, d) in self.legend() {
            let w = k.chars().count() + d.chars().count() + 4;
            if used + w > room {
                break;
            }
            used += w;
            spans.push(Span::styled(k, Style::default().fg(FG)));
            spans.push(Span::styled(format!(" {}   ", d), Style::default().fg(FAINT)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        f.render_widget(Paragraph::new(help).alignment(Alignment::Right), area);
    }

    pub(super) fn open_help(&mut self) {
        self.modal = Some(Modal::Help { scroll: 0 });
    }

    pub(super) fn help_key(&mut self, code: KeyCode, scroll: u16) -> Option<Modal> {
        let scroll = match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter | KeyCode::F(1) => return None,
            KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => scroll + 1,
            KeyCode::PageUp => scroll.saturating_sub(10),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll + 10,
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => u16::MAX,
            _ => scroll,
        };
        // Clamped to the end when drawn.
        Some(Modal::Help { scroll })
    }

    pub(super) fn draw_help(&mut self, f: &mut Frame, scroll: u16) {
        let area = f.area();
        let w = HELP_W.min(area.width.saturating_sub(2));
        let desc_w = (w as usize).saturating_sub(KEY_W + 5);
        let mut all: Vec<Line> = Vec::new();
        for (i, (section, keys)) in HELP.iter().enumerate() {
            if i > 0 {
                all.push(Line::from(""));
            }
            all.push(Line::from(Span::styled(
                format!(" {}", section),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            for (k, d) in keys.iter() {
                if k.is_empty() {
                    all.push(Line::from(Span::styled(format!("   {}", truncate(d, desc_w + KEY_W)), Style::default().fg(DIM))));
                } else {
                    all.push(Line::from(vec![
                        Span::styled(format!("   {:<KEY_W$}", k), Style::default().fg(BRIGHT)),
                        Span::styled(truncate(d, desc_w), Style::default().fg(FG)),
                    ]));
                }
            }
        }
        // Fit the window to the screen, scrolling what doesn't fit.
        let body_h = (area.height.saturating_sub(5) as usize).max(3);
        let max_scroll = all.len().saturating_sub(body_h);
        let scroll = (scroll as usize).min(max_scroll);
        if let Some(Modal::Help { scroll: s }) = &mut self.modal {
            *s = scroll as u16;
        }
        let mut lines: Vec<Line> = all.into_iter().skip(scroll).take(body_h).collect();
        lines.push(Line::from(""));
        let more = if max_scroll > 0 { "↑↓ scroll   " } else { "" };
        lines.push(Line::from(Span::styled(format!(" {}esc close", more), Style::default().fg(FAINT))));
        draw_window(f, " Keys ", lines, w);
    }
}
