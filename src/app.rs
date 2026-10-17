//! Terminal user interface: artist/album tree, track list and now-playing bar.

use crate::player::{Cmd, PlayState, Player, Status};
use crate::plex::{khz, Album, Artist, ReleaseKind, Server, Track};
use crate::report::Reporter;
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

const ACCENT: Color = Color::Rgb(0xe5, 0xa0, 0x0d);
const FG: Color = Color::Rgb(0xc9, 0xd1, 0xd9);
const BRIGHT: Color = Color::Rgb(0xe6, 0xed, 0xf3);
const DIM: Color = Color::Rgb(0x8b, 0x94, 0x9e);
const FAINT: Color = Color::Rgb(0x6e, 0x76, 0x81);
const BORDER: Color = Color::Rgb(0x30, 0x36, 0x3d);
const SEL_BG: Color = Color::Rgb(0x1f, 0x26, 0x30);
const GOOD: Color = Color::Rgb(0x7e, 0xe7, 0x87);
const WARN: Color = Color::Rgb(0xf0, 0x88, 0x3e);

enum Msg {
    Artists(Result<Vec<Artist>, String>),
    Albums(String, Result<Vec<Album>, String>),
    Tracks(String, Result<Vec<Track>, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Tracks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Artist(usize),
    /// A release-type heading ("Albums", "EPs", …) under an expanded artist.
    Heading(usize, ReleaseKind),
    Album(usize, usize),
}

enum Loadable<T> {
    Loading,
    Ready(T),
    Failed(String),
}

pub struct App {
    server: Server,
    library_key: String,
    header: HeaderInfo,
    player: Player,
    _reporter: Option<Reporter>,
    /// Show release-type headings under an expanded artist.
    group_releases: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    artists: Loadable<Vec<Artist>>,
    albums: HashMap<String, Loadable<Vec<Album>>>,
    expanded: HashMap<String, bool>,
    tracks: HashMap<String, Loadable<Vec<Track>>>,

    tree_state: ListState,
    track_state: ListState,
    focus: Focus,
    filter: String,
    filtering: bool,
    /// Album shown in the right pane: (artist name, album).
    shown_album: Option<(String, Album)>,
    /// Album to start playing as soon as its tracks arrive.
    play_when_loaded: Option<(String, usize)>,
    quit: bool,
}

pub struct HeaderInfo {
    pub server_name: String,
    pub library_title: String,
}

impl App {
    pub fn new(
        server: Server,
        library_key: String,
        header: HeaderInfo,
        device: String,
        report_playback: bool,
        group_releases: bool,
    ) -> App {
        let (tx, rx) = mpsc::channel();
        let player = Player::spawn(server.clone(), device);
        let reporter = report_playback.then(|| {
            Reporter::spawn(
                server.clone(),
                player.status_handle(),
                crate::config::new_client_id().replacen("plext-", "plext-session-", 1),
            )
        });
        let mut app = App {
            _reporter: reporter,
            group_releases,
            server,
            library_key,
            header,
            player,
            tx,
            rx,
            artists: Loadable::Loading,
            albums: HashMap::new(),
            expanded: HashMap::new(),
            tracks: HashMap::new(),
            tree_state: ListState::default().with_selected(Some(0)),
            track_state: ListState::default().with_selected(Some(0)),
            focus: Focus::Tree,
            filter: String::new(),
            filtering: false,
            shown_album: None,
            play_when_loaded: None,
            quit: false,
        };
        app.load_artists();
        app
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(m) = self.rx.try_recv() {
                self.on_msg(m);
            }
            let status = self.player.status();
            terminal.draw(|f| self.draw(f, &status))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        self.on_key(k);
                    }
                }
            }
        }
        self.player.send(Cmd::Stop);
        Ok(())
    }

    // ---------- loading ----------

    fn load_artists(&mut self) {
        let (s, key, tx) = (self.server.clone(), self.library_key.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Artists(s.artists(&key).map_err(|e| e.to_string())));
        });
    }

    fn load_albums(&mut self, artist_key: &str) {
        if self.albums.contains_key(artist_key) {
            return;
        }
        self.albums.insert(artist_key.to_string(), Loadable::Loading);
        let (s, k, tx) = (self.server.clone(), artist_key.to_string(), self.tx.clone());
        let lib = self.library_key.clone();
        thread::spawn(move || {
            let r = s.albums(&lib, &k).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Albums(k, r));
        });
    }

    fn load_tracks(&mut self, album_key: &str) {
        if matches!(self.tracks.get(album_key), Some(Loadable::Ready(_)) | Some(Loadable::Loading)) {
            return;
        }
        self.tracks.insert(album_key.to_string(), Loadable::Loading);
        let (s, k, tx) = (self.server.clone(), album_key.to_string(), self.tx.clone());
        thread::spawn(move || {
            let r = s.tracks(&k).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Tracks(k, r));
        });
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Artists(r) => {
                self.artists = match r {
                    Ok(a) => Loadable::Ready(a),
                    Err(e) => Loadable::Failed(e),
                }
            }
            Msg::Albums(k, r) => {
                self.albums.insert(
                    k,
                    match r {
                        Ok(a) => Loadable::Ready(a),
                        Err(e) => Loadable::Failed(e),
                    },
                );
                self.sync_shown_album();
            }
            Msg::Tracks(k, r) => {
                let ready = r.is_ok();
                self.tracks.insert(
                    k.clone(),
                    match r {
                        Ok(t) => Loadable::Ready(t),
                        Err(e) => Loadable::Failed(e),
                    },
                );
                if ready {
                    if let Some((pk, idx)) = self.play_when_loaded.clone() {
                        if pk == k {
                            self.play_when_loaded = None;
                            self.play_album(&k, idx);
                        }
                    }
                }
            }
        }
    }

    // ---------- tree ----------

    fn visible_artists(&self) -> Vec<usize> {
        match &self.artists {
            Loadable::Ready(a) => {
                let f = self.filter.to_lowercase();
                a.iter()
                    .enumerate()
                    .filter(|(_, x)| f.is_empty() || x.title.to_lowercase().contains(&f))
                    .map(|(i, _)| i)
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let artists = match &self.artists {
            Loadable::Ready(a) => a,
            _ => return rows,
        };
        for i in self.visible_artists() {
            rows.push(Row::Artist(i));
            let key = &artists[i].key;
            if self.expanded.get(key).copied().unwrap_or(false) {
                if let Some(Loadable::Ready(albums)) = self.albums.get(key) {
                    if self.group_releases {
                        // Albums arrive sorted by release type: put a heading
                        // before the first release of each type.
                        let mut current: Option<ReleaseKind> = None;
                        for (j, al) in albums.iter().enumerate() {
                            if current != Some(al.kind) {
                                rows.push(Row::Heading(i, al.kind));
                                current = Some(al.kind);
                            }
                            rows.push(Row::Album(i, j));
                        }
                    } else {
                        // One list in release order, oldest first.
                        let mut order: Vec<usize> = (0..albums.len()).collect();
                        order.sort_by_key(|&j| (albums[j].year.unwrap_or(u32::MAX), albums[j].title.to_lowercase()));
                        rows.extend(order.into_iter().map(|j| Row::Album(i, j)));
                    }
                }
            }
        }
        rows
    }

    fn artist(&self, i: usize) -> Option<&Artist> {
        match &self.artists {
            Loadable::Ready(a) => a.get(i),
            _ => None,
        }
    }

    fn album(&self, i: usize, j: usize) -> Option<&Album> {
        let a = self.artist(i)?;
        match self.albums.get(&a.key) {
            Some(Loadable::Ready(al)) => al.get(j),
            _ => None,
        }
    }

    fn current_row(&self) -> Option<Row> {
        let rows = self.rows();
        self.tree_state.selected().and_then(|i| rows.get(i).copied())
    }

    /// Shows the tracks of the album under the cursor in the right pane.
    fn sync_shown_album(&mut self) {
        if let Some(Row::Album(i, j)) = self.current_row() {
            if let (Some(ar), Some(al)) = (self.artist(i).cloned(), self.album(i, j).cloned()) {
                let changed = self.shown_album.as_ref().map(|(_, a)| a.key != al.key).unwrap_or(true);
                if changed {
                    self.load_tracks(&al.key);
                    self.shown_album = Some((ar.title, al));
                    self.track_state.select(Some(0));
                }
            }
        }
    }

    fn set_expanded(&mut self, i: usize, open: bool) {
        if let Some(a) = self.artist(i).cloned() {
            if open {
                self.load_albums(&a.key);
            }
            self.expanded.insert(a.key, open);
        }
    }

    fn move_tree(&mut self, delta: i64) {
        let n = self.rows().len() as i64;
        if n == 0 {
            return;
        }
        let rows = self.rows();
        let cur = self.tree_state.selected().unwrap_or(0) as i64;
        let mut next = (cur + delta).clamp(0, n - 1);
        // Headings can't be selected: keep going the same way, or turn back at the ends.
        let step = if delta < 0 { -1 } else { 1 };
        while matches!(rows.get(next as usize), Some(Row::Heading(..))) {
            let further = next + step;
            if further < 0 || further >= n {
                next -= step;
            } else {
                next = further;
            }
        }
        self.tree_state.select(Some(next as usize));
        self.sync_shown_album();
    }

    fn shown_tracks(&self) -> Option<&Vec<Track>> {
        let (_, al) = self.shown_album.as_ref()?;
        match self.tracks.get(&al.key) {
            Some(Loadable::Ready(t)) => Some(t),
            _ => None,
        }
    }

    fn play_album(&mut self, album_key: &str, index: usize) {
        if let Some(Loadable::Ready(t)) = self.tracks.get(album_key) {
            if !t.is_empty() {
                self.player.send(Cmd::Play {
                    queue: t.clone(),
                    index: index.min(t.len() - 1),
                });
            }
        }
    }

    // ---------- input ----------

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.filtering {
            match k.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.tree_state.select(Some(0));
            self.sync_shown_album();
            return;
        }

        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('/') => {
                self.focus = Focus::Tree;
                self.filtering = true;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.tree_state.select(Some(0));
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Tree if self.shown_album.is_some() => Focus::Tracks,
                    _ => Focus::Tree,
                }
            }
            KeyCode::Char(' ') => self.player.send(Cmd::TogglePause),
            KeyCode::Char('n') => self.player.send(Cmd::Next),
            KeyCode::Char('p') => self.player.send(Cmd::Prev),
            KeyCode::Char('s') => self.player.send(Cmd::Stop),
            KeyCode::Char('.') | KeyCode::Char(']') => self.player.send(Cmd::SeekBy(10.0)),
            KeyCode::Char(',') | KeyCode::Char('[') => self.player.send(Cmd::SeekBy(-10.0)),
            _ => match self.focus {
                Focus::Tree => self.tree_key(k.code),
                Focus::Tracks => self.tracks_key(k.code),
            },
        }
    }

    fn tree_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_tree(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_tree(1),
            KeyCode::PageUp => self.move_tree(-15),
            KeyCode::PageDown => self.move_tree(15),
            KeyCode::Home | KeyCode::Char('g') => self.move_tree(-1_000_000),
            KeyCode::End | KeyCode::Char('G') => self.move_tree(1_000_000),
            KeyCode::Right | KeyCode::Char('l') => match self.current_row() {
                Some(Row::Artist(i)) => self.set_expanded(i, true),
                Some(Row::Album(..)) => {
                    self.sync_shown_album();
                    self.focus = Focus::Tracks;
                }
                None | Some(Row::Heading(..)) => {}
            },
            KeyCode::Left | KeyCode::Char('h') => match self.current_row() {
                Some(Row::Artist(i)) => self.set_expanded(i, false),
                Some(Row::Album(i, _)) => {
                    self.set_expanded(i, false);
                    let rows = self.rows();
                    let pos = rows.iter().position(|r| *r == Row::Artist(i));
                    self.tree_state.select(pos);
                }
                None | Some(Row::Heading(..)) => {}
            },
            KeyCode::Enter => match self.current_row() {
                Some(Row::Artist(i)) => {
                    let open = self
                        .artist(i)
                        .map(|a| self.expanded.get(&a.key).copied().unwrap_or(false))
                        .unwrap_or(false);
                    self.set_expanded(i, !open);
                }
                Some(Row::Album(i, j)) => {
                    if let Some(al) = self.album(i, j).cloned() {
                        self.sync_shown_album();
                        if matches!(self.tracks.get(&al.key), Some(Loadable::Ready(_))) {
                            self.play_album(&al.key, 0);
                        } else {
                            self.load_tracks(&al.key);
                            self.play_when_loaded = Some((al.key, 0));
                        }
                    }
                }
                None | Some(Row::Heading(..)) => {}
            },
            _ => {}
        }
    }

    fn tracks_key(&mut self, code: KeyCode) {
        let n = self.shown_tracks().map(|t| t.len()).unwrap_or(0);
        let cur = self.track_state.selected().unwrap_or(0);
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.track_state.select(Some(cur.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => {
                self.track_state.select(Some((cur + 1).min(n.saturating_sub(1))))
            }
            KeyCode::Home | KeyCode::Char('g') => self.track_state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => self.track_state.select(Some(n.saturating_sub(1))),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Tree,
            KeyCode::Enter => {
                if let Some((_, al)) = self.shown_album.clone() {
                    self.play_album(&al.key, cur);
                }
            }
            _ => {}
        }
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame, st: &Status) {
        let area = f.area();
        f.render_widget(Block::default().style(Style::default().fg(FG)), area);
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(5),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_header(f, outer[0]);
        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
            .split(outer[1]);
        self.draw_tree(f, main[0]);
        self.draw_tracks(f, main[1], st);
        draw_now_playing(f, outer[2], st);
        self.draw_keys(f, outer[3]);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let left = Line::from(vec![
            Span::styled(" ▶ plext", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled("   server: ", Style::default().fg(DIM)),
            Span::styled(&self.header.server_name, Style::default().fg(BRIGHT)),
            Span::styled("   library: ", Style::default().fg(DIM)),
            Span::styled(&self.header.library_title, Style::default().fg(BRIGHT)),
        ]);
        f.render_widget(Paragraph::new(left), area);
        let count = match &self.artists {
            Loadable::Ready(a) => format!("{} artists ", a.len()),
            Loadable::Loading => "loading… ".into(),
            Loadable::Failed(_) => "".into(),
        };
        let right = Line::from(vec![
            Span::styled(count, Style::default().fg(DIM)),
            Span::styled("● connected ", Style::default().fg(GOOD)),
        ])
        .alignment(Alignment::Right);
        f.render_widget(Paragraph::new(right), area);
    }

    fn pane(&self, title: Line<'static>, right: String, focused: bool) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { DIM } else { BORDER }))
            .title(title)
            .title(Line::from(Span::styled(right, Style::default().fg(DIM))).alignment(Alignment::Right))
    }

    fn draw_tree(&mut self, f: &mut Frame, area: Rect) {
        let filter_label = if self.filtering {
            format!(" /{}▏", self.filter)
        } else if !self.filter.is_empty() {
            format!(" /{} (esc clears) ", self.filter)
        } else {
            " / filter ".into()
        };
        let block = self.pane(
            Line::from(Span::styled(
                " Album Artists ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            filter_label,
            self.focus == Focus::Tree,
        );

        let playing_album = self.player.status().track.map(|t| t.album);
        let rows = self.rows();
        let grouped_artists: std::collections::HashSet<usize> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Heading(i, _) => Some(*i),
                _ => None,
            })
            .collect();
        let items: Vec<ListItem> = match &self.artists {
            Loadable::Loading => vec![ListItem::new(Span::styled(" Loading artists…", Style::default().fg(DIM)))],
            Loadable::Failed(e) => vec![ListItem::new(Span::styled(
                format!(" Couldn't load artists: {}", e),
                Style::default().fg(WARN),
            ))],
            Loadable::Ready(_) => rows
                .iter()
                .map(|r| match *r {
                    Row::Artist(i) => {
                        let a = self.artist(i).unwrap();
                        let open = self.expanded.get(&a.key).copied().unwrap_or(false);
                        let loading = open && matches!(self.albums.get(&a.key), Some(Loadable::Loading));
                        let marker = if open { "▾ " } else { "▸ " };
                        let style = if open {
                            Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(DIM)
                        };
                        let mut spans = vec![Span::styled(format!("{}{}", marker, a.title), style)];
                        if loading {
                            spans.push(Span::styled("  …", Style::default().fg(FAINT)));
                        }
                        if let Some(Loadable::Failed(_)) = self.albums.get(&a.key) {
                            spans.push(Span::styled("  (failed to load)", Style::default().fg(WARN)));
                        }
                        ListItem::new(Line::from(spans))
                    }
                    Row::Heading(_, kind) => ListItem::new(Line::from(Span::styled(
                        format!("    {}", kind.group_title()),
                        Style::default().fg(FAINT).add_modifier(Modifier::BOLD),
                    ))),
                    Row::Album(i, j) => {
                        let al = self.album(i, j).unwrap();
                        let playing = playing_album.as_deref() == Some(al.title.as_str());
                        let grouped = grouped_artists.contains(&i);
                        let mut spans = vec![Span::raw(if grouped { "      " } else { "    " })];
                        if playing {
                            spans.push(Span::styled("♪ ", Style::default().fg(ACCENT)));
                        }
                        spans.push(Span::styled(al.title.clone(), Style::default().fg(DIM)));
                        if let Some(y) = al.year {
                            spans.push(Span::styled(format!(" ({})", y), Style::default().fg(FAINT)));
                        }
                        ListItem::new(Line::from(spans))
                    }
                })
                .collect(),
        };
        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(SEL_BG)
                .fg(BRIGHT)
                .add_modifier(if self.focus == Focus::Tree { Modifier::BOLD } else { Modifier::empty() }),
        );
        f.render_stateful_widget(list, area, &mut self.tree_state);
    }

    fn draw_tracks(&mut self, f: &mut Frame, area: Rect, st: &Status) {
        let (title, right) = match &self.shown_album {
            Some((artist, al)) => {
                let mut t = vec![
                    Span::raw(" "),
                    Span::styled(al.title.clone(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" · {}", artist), Style::default().fg(DIM)),
                ];
                if let Some(y) = al.year {
                    t.push(Span::styled(format!(" · {}", y), Style::default().fg(DIM)));
                }
                if let Some(kind) = al.kind.label() {
                    t.push(Span::styled(format!(" · {}", kind), Style::default().fg(DIM)));
                }
                t.push(Span::raw(" "));
                let right = match self.tracks.get(&al.key) {
                    Some(Loadable::Ready(tr)) => {
                        let total: u64 = tr.iter().filter_map(|t| t.duration_ms).sum();
                        let fmt = tr.first().map(|t| t.format_label()).unwrap_or_default();
                        let codec = tr
                            .first()
                            .and_then(|t| t.codec.clone())
                            .map(|c| c.to_uppercase())
                            .unwrap_or_default();
                        format!(" {} {} · {} tracks · {} ", codec, fmt, tr.len(), mmss(total as f64 / 1000.0))
                    }
                    _ => String::new(),
                };
                (Line::from(t), right)
            }
            None => (
                Line::from(Span::styled(" Tracks ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
                String::new(),
            ),
        };
        let block = self.pane(title, right, self.focus == Focus::Tracks);
        let inner_w = area.width.saturating_sub(2) as usize;

        let items: Vec<ListItem> = match self.shown_album.as_ref().and_then(|(_, al)| self.tracks.get(&al.key)) {
            None => vec![ListItem::new(Span::styled(
                " Select an album on the left (→ to expand an artist).",
                Style::default().fg(DIM),
            ))],
            Some(Loadable::Loading) => vec![ListItem::new(Span::styled(" Loading tracks…", Style::default().fg(DIM)))],
            Some(Loadable::Failed(e)) => vec![ListItem::new(Span::styled(
                format!(" Couldn't load tracks: {}", e),
                Style::default().fg(WARN),
            ))],
            Some(Loadable::Ready(tracks)) => {
                let multi_disc = tracks.iter().any(|t| t.disc.unwrap_or(1) > 1);
                let playing_key = st.track.as_ref().map(|t| t.key.clone());
                let mut items = vec![ListItem::new(track_line(
                    "#", "Title", "Format", "Time", inner_w, Style::default().fg(FAINT),
                ))];
                for t in tracks {
                    let playing = playing_key.as_deref() == Some(t.key.as_str()) && st.state != PlayState::Stopped;
                    let num = if playing {
                        "▶".to_string()
                    } else if multi_disc {
                        format!("{}-{}", t.disc.unwrap_or(1), t.index.unwrap_or(0))
                    } else {
                        t.index.map(|i| i.to_string()).unwrap_or_default()
                    };
                    let style = if playing {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    };
                    let time = t.duration_ms.map(|d| mmss(d as f64 / 1000.0)).unwrap_or_default();
                    items.push(ListItem::new(track_line(
                        &num,
                        &t.title,
                        &t.format_label(),
                        &time,
                        inner_w,
                        style,
                    )));
                }
                items
            }
        };
        // Offset selection by one for the header row.
        let mut state = ListState::default()
            .with_selected(self.track_state.selected().map(|i| i + 1))
            .with_offset(self.track_state.offset());
        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(if self.focus == Focus::Tracks { SEL_BG } else { Color::Reset })
                .fg(BRIGHT),
        );
        f.render_stateful_widget(list, area, &mut state);
        *self.track_state.offset_mut() = state.offset();
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let keys = [
            ("↑↓", "move"),
            ("→/←", "expand"),
            ("enter", "play"),
            ("space", "pause"),
            ("n/p", "next/prev"),
            (",/.", "seek"),
            ("tab", "switch pane"),
            ("/", "search"),
            ("q", "quit"),
        ];
        let mut spans = vec![Span::raw(" ")];
        for (k, d) in keys {
            spans.push(Span::styled(k, Style::default().fg(FG)));
            spans.push(Span::styled(format!(" {}   ", d), Style::default().fg(FAINT)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn track_line(num: &str, title: &str, fmt: &str, time: &str, width: usize, style: Style) -> Line<'static> {
    let num_w = 5;
    let fmt_w = 12;
    let time_w = 7;
    let title_w = width.saturating_sub(num_w + fmt_w + time_w + 2).max(8);
    let title = truncate(title, title_w);
    Line::from(Span::styled(
        format!(
            " {:<num_w$}{:<title_w$}{:<fmt_w$}{:>time_w$}",
            truncate(num, num_w - 1),
            title,
            truncate(fmt, fmt_w - 1),
            time,
        ),
        style,
    ))
}

fn draw_now_playing(f: &mut Frame, area: Rect, st: &Status) {
    let active = matches!(st.state, PlayState::Playing | PlayState::Paused | PlayState::Loading);
    let edge = if st.state == PlayState::Playing { ACCENT } else if active { DIM } else { BORDER };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(edge))
        .title(Line::from(Span::styled(
            " Now Playing ",
            Style::default().fg(if active { ACCENT } else { DIM }).add_modifier(Modifier::BOLD),
        )));
    if active && st.queue_len > 0 {
        block = block.title(
            Line::from(Span::styled(
                format!(" track {} of {} ", st.queue_pos + 1, st.queue_len),
                Style::default().fg(DIM),
            ))
            .alignment(Alignment::Right),
        );
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    // A column of breathing room inside the frame.
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let (label, label_color) = match st.state {
        PlayState::Playing => ("▶ PLAYING", ACCENT),
        PlayState::Paused => ("❚❚ PAUSED", DIM),
        PlayState::Loading => ("… LOADING", DIM),
        PlayState::Stopped => ("■ STOPPED", FAINT),
    };

    // Line 1: state, title, artist — album, bit-perfect badge.
    let mut l1 = vec![Span::styled(
        format!(" {}  ", label),
        Style::default().fg(label_color).add_modifier(Modifier::BOLD),
    )];
    if let Some(t) = &st.track {
        if st.state != PlayState::Stopped {
            l1.push(Span::styled(t.title.clone(), Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD)));
            l1.push(Span::styled(format!("   {} — {}", t.artist, t.album), Style::default().fg(DIM)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(l1)), rows[0]);
    if st.output.is_some() {
        let badge = if st.bit_perfect() {
            Span::styled("◆ BIT-PERFECT ", Style::default().fg(GOOD).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("◇ NOT BIT-PERFECT ", Style::default().fg(WARN).add_modifier(Modifier::BOLD))
        };
        f.render_widget(Paragraph::new(Line::from(badge)).alignment(Alignment::Right), rows[0]);
    }

    // Line 2: progress.
    let dur = st.dur.unwrap_or(0.0);
    let ratio = if dur > 0.0 { (st.pos / dur).clamp(0.0, 1.0) } else { 0.0 };
    let bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(9), Constraint::Min(4), Constraint::Length(9)])
        .split(rows[1]);
    let playing = st.state != PlayState::Stopped;
    f.render_widget(
        Paragraph::new(Span::styled(
            if playing { format!(" {}", mmss(st.pos)) } else { String::new() },
            Style::default().fg(BRIGHT),
        )),
        bar[0],
    );
    let w = bar[1].width as usize;
    let filled = if playing { ((w as f64) * ratio).round() as usize } else { 0 }.min(w);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("━".repeat(filled), Style::default().fg(ACCENT)),
            Span::styled("─".repeat(w - filled), Style::default().fg(BORDER)),
        ])),
        bar[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            if playing && dur > 0.0 { format!("{} ", mmss(dur)) } else { String::new() },
            Style::default().fg(DIM),
        ))
        .alignment(Alignment::Right),
        bar[2],
    );

    // Line 3: source and output details, or the last error.
    if let Some(e) = &st.error {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ⚠ {}", e), Style::default().fg(WARN))),
            rows[2],
        );
        return;
    }
    // Split the line: source details on the left, output details on the right.
    let out_text = match (&st.output, playing) {
        (Some(o), true) => {
            let mode = if o.direct { "exclusive · no resample · vol: DAC" } else { "via ALSA plugins" };
            Some((o.label.clone(), format!(" · {} · {} ", o.fmt.name(), mode)))
        }
        _ => None,
    };
    let out_w = out_text
        .as_ref()
        .map(|(d, rest)| (5 + d.chars().count() + rest.chars().count()) as u16)
        .unwrap_or(0)
        .min(rows[2].width * 55 / 100);
    let line3 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(out_w)])
        .split(rows[2]);
    if let (Some(s), true) = (&st.source, playing) {
        let ch = match s.channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{} ch", n),
        };
        let mut src = format!(" {} · ", s.codec);
        if s.lossless {
            src.push_str(&format!("{}-bit / {} kHz · {}", s.bits, khz(s.rate), ch));
        } else {
            src.push_str(&format!("{} kHz · {} · lossy", khz(s.rate), ch));
        }
        if let Some(br) = st.track.as_ref().and_then(|t| t.bitrate_kbps) {
            src.push_str(&format!(" · {} kbps", br));
        }
        f.render_widget(Paragraph::new(Span::styled(src, Style::default().fg(DIM))), line3[0]);
    }
    if let Some((device, rest)) = out_text {
        let room = (out_w as usize).saturating_sub(5 + rest.chars().count());
        let out = Line::from(vec![
            Span::styled("out: ", Style::default().fg(DIM)),
            Span::styled(truncate(&device, room.max(6)), Style::default().fg(BRIGHT)),
            Span::styled(rest, Style::default().fg(DIM)),
        ])
        .alignment(Alignment::Right);
        f.render_widget(Paragraph::new(out), line3[1]);
    }
}

fn mmss(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}
