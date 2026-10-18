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
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const ACCENT: Color = Color::Rgb(0xe5, 0xa0, 0x0d);
const FG: Color = Color::Rgb(0xc9, 0xd1, 0xd9);
const BRIGHT: Color = Color::Rgb(0xe6, 0xed, 0xf3);
const DIM: Color = Color::Rgb(0x8b, 0x94, 0x9e);
const FAINT: Color = Color::Rgb(0x6e, 0x76, 0x81);
/// Unfocused pane edges. Light enough to stay visible on dark terminal
/// backgrounds (the old #30363d vanished against themes like Nord).
const BORDER: Color = Color::Rgb(0x54, 0x5c, 0x66);
const SEL_BG: Color = Color::Rgb(0x1f, 0x26, 0x30);
const GOOD: Color = Color::Rgb(0x7e, 0xe7, 0x87);
const WARN: Color = Color::Rgb(0xf0, 0x88, 0x3e);
/// A half star: the accent at about half strength, so 4½ stars reads as
/// "almost five" while still lining up in any terminal font.
const HALF_STAR: Color = Color::Rgb(0x8a, 0x6a, 0x2a);
/// Width of the rating column: five stars and a gap before the format.
const RATING_W: usize = 7;

enum Msg {
    Artists(Result<Vec<Artist>, String>),
    Albums(String, Result<Vec<Album>, String>),
    Tracks(String, Result<Vec<Track>, String>),
    /// A rating change finished saving to Plex (or failed to).
    Rated {
        key: String,
        wanted: Option<u8>,
        previous: Option<u8>,
        result: Result<(), String>,
    },
}

/// A rating change waiting to be saved to Plex.
struct RateJob {
    key: String,
    wanted: Option<u8>,
    previous: Option<u8>,
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
    /// Albums whose track list is being re-fetched in the background.
    refreshing: HashSet<String>,
    /// The latest rating we know of for each track, by rating key. This wins
    /// over the copy inside a `Track`, which may be older (e.g. the player's).
    ratings: HashMap<String, Option<u8>>,
    /// Rating changes still being saved, per track. While any are pending,
    /// a refreshed track list mustn't overwrite what you just chose.
    pending_ratings: HashMap<String, u32>,
    /// Rating changes go to one worker so they reach Plex in the order made.
    rate_tx: Sender<RateJob>,
    /// An error from the UI side (e.g. a rating that didn't save) and when it appeared.
    notice: Option<(String, Instant)>,

    tree_state: ListState,
    /// Rows visible in the artist pane at the last draw, for PageUp/PageDown.
    tree_page: usize,
    track_state: ListState,
    focus: Focus,
    filter: String,
    filtering: bool,
    /// Album shown in the right pane: (artist name, album).
    shown_album: Option<(String, Album)>,
    /// Album to start playing as soon as its tracks arrive.
    play_when_loaded: Option<(String, usize)>,
    quit: bool,
    /// The last error we've seen from the player, and when it appeared.
    seen_error_seq: u64,
    error_since: Option<Instant>,
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
        let rate_tx = spawn_rater(server.clone(), tx.clone());
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
            refreshing: HashSet::new(),
            ratings: HashMap::new(),
            pending_ratings: HashMap::new(),
            rate_tx,
            notice: None,
            tree_state: ListState::default().with_selected(Some(0)),
            tree_page: 15,
            track_state: ListState::default().with_selected(Some(0)),
            focus: Focus::Tree,
            filter: String::new(),
            filtering: false,
            shown_album: None,
            play_when_loaded: None,
            quit: false,
            seen_error_seq: 0,
            error_since: None,
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
            if status.error_seq != self.seen_error_seq {
                self.seen_error_seq = status.error_seq;
                self.error_since = Some(Instant::now());
            }
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

    /// Fetches an album's tracks again so ratings changed elsewhere (in Plexamp,
    /// or on another copy of the same recording) show up. The list on screen
    /// stays put until the fresh one arrives.
    fn refresh_tracks(&mut self, album_key: &str) {
        match self.tracks.get(album_key) {
            Some(Loadable::Ready(_)) => {}
            Some(Loadable::Loading) => return,
            _ => return self.load_tracks(album_key),
        }
        if !self.refreshing.insert(album_key.to_string()) {
            return;
        }
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
                let was_refresh = self.refreshing.remove(&k);
                if was_refresh && r.is_err() {
                    // Keep showing what we had; the next visit will try again.
                    return;
                }
                if let Ok(tracks) = &r {
                    for t in tracks {
                        if self.pending_ratings.get(&t.key).copied().unwrap_or(0) == 0 {
                            self.ratings.insert(t.key.clone(), t.rating);
                        }
                    }
                    let shown = self.shown_album.as_ref().map(|(_, a)| a.key == k).unwrap_or(false);
                    if shown {
                        let last = tracks.len().saturating_sub(1);
                        if let Some(sel) = self.track_state.selected() {
                            self.track_state.select(Some(sel.min(last)));
                        }
                    }
                }
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
            Msg::Rated { key, wanted, previous, result } => {
                if let Some(n) = self.pending_ratings.get_mut(&key) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.pending_ratings.remove(&key);
                    }
                }
                if let Err(e) = result {
                    crate::log::write(&format!("error: couldn't save rating for track {}: {}", key, e));
                    // Put the old rating back, unless you've changed it again since.
                    if self.ratings.get(&key).copied().flatten() == wanted {
                        self.ratings.insert(key, previous);
                    }
                    self.notice = Some((format!("Couldn't save rating: {}", e), Instant::now()));
                }
            }
        }
    }

    // ---------- ratings ----------

    /// The latest rating we know of for a track.
    fn rating_of(&self, t: &Track) -> Option<u8> {
        self.ratings.get(&t.key).copied().unwrap_or(t.rating)
    }

    /// The track a rating key applies to: the highlighted track when the
    /// track list has focus, otherwise the one playing.
    fn rating_target(&self) -> Option<(String, Option<u8>)> {
        match self.focus {
            Focus::Tracks => {
                let t = self.shown_tracks()?.get(self.track_state.selected()?)?;
                Some((t.key.clone(), self.rating_of(t)))
            }
            Focus::Tree => {
                let st = self.player.status();
                if st.state == PlayState::Stopped {
                    return None;
                }
                let t = st.track?;
                Some((t.key.clone(), self.rating_of(&t)))
            }
        }
    }

    fn rate(&mut self, code: KeyCode) {
        let Some((key, current)) = self.rating_target() else {
            return;
        };
        let wanted = match rating_for_key(code, current) {
            Some(w) => w,
            None => return,
        };
        if wanted == current {
            return;
        }
        self.ratings.insert(key.clone(), wanted);
        *self.pending_ratings.entry(key.clone()).or_insert(0) += 1;
        let _ = self.rate_tx.send(RateJob {
            key,
            wanted,
            previous: current,
        });
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
                    self.refresh_tracks(&al.key);
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
            KeyCode::Char('0'..='5') | KeyCode::Char('-') | KeyCode::Char('=') | KeyCode::Char('+') => {
                self.rate(k.code)
            }
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
            KeyCode::PageUp => self.move_tree(-(self.tree_page.max(1) as i64)),
            KeyCode::PageDown => self.move_tree(self.tree_page.max(1) as i64),
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
                Constraint::Length(1), // header
                Constraint::Length(1), // breathing room under the header
                Constraint::Min(5),
                Constraint::Length(5),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_header(f, outer[0]);
        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
            .split(outer[2]);
        self.draw_tree(f, main[0]);
        self.draw_tracks(f, main[1], st);
        // Errors stay up long enough to read, even if the next track has started.
        let recent = |t: Instant| t.elapsed() < Duration::from_secs(15);
        let player_error = st
            .error
            .as_deref()
            .zip(self.error_since)
            .filter(|(_, t)| recent(*t));
        let notice = self
            .notice
            .as_ref()
            .map(|(m, t)| (m.as_str(), *t))
            .filter(|(_, t)| recent(*t));
        // Show whichever error is newer.
        let error = match (player_error, notice) {
            (Some(p), Some(n)) => Some(if n.1 > p.1 { n.0 } else { p.0 }),
            (p, n) => p.or(n).map(|(m, _)| m),
        };
        let rating = st.track.as_ref().map(|t| self.rating_of(t));
        draw_now_playing(f, outer[3], st, error, rating);
        self.draw_keys(f, outer[4]);
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
        let mut block = self.pane(
            Line::from(Span::styled(" Album Artists ", title_style(self.focus == Focus::Tree))),
            filter_label,
            self.focus == Focus::Tree,
        );
        // Where you are in the (possibly filtered) artist list.
        let visible = self.visible_artists();
        if !visible.is_empty() {
            let cur_artist = match self.current_row() {
                Some(Row::Artist(i)) | Some(Row::Heading(i, _)) | Some(Row::Album(i, _)) => Some(i),
                None => None,
            };
            if let Some(pos) = cur_artist.and_then(|i| visible.iter().position(|&v| v == i)) {
                block = block.title_bottom(
                    Line::from(Span::styled(
                        format!(" {} of {} ", pos + 1, visible.len()),
                        Style::default().fg(FAINT),
                    ))
                    .alignment(Alignment::Right),
                );
            }
        }
        self.tree_page = area.height.saturating_sub(3) as usize;

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
                            Style::default().fg(FG)
                        };
                        let mut spans = vec![
                            Span::styled(marker, Style::default().fg(if open { ACCENT } else { FAINT })),
                            Span::styled(a.title.clone(), style),
                        ];
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
        let list = List::new(items)
            .block(block)
            // Keep a few rows of context visible above and below the cursor.
            .scroll_padding(3)
            .highlight_style(
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
                    Span::styled(al.title.clone(), title_style(self.focus == Focus::Tracks)),
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
                Line::from(Span::styled(" Tracks ", title_style(self.focus == Focus::Tracks))),
                String::new(),
            ),
        };
        let block = self.pane(title, right, self.focus == Focus::Tracks);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let inner_w = inner.width as usize;

        let message = |text: String, color: Color| List::new(vec![ListItem::new(Span::styled(text, Style::default().fg(color)))]);
        let tracks = match self.shown_album.as_ref().and_then(|(_, al)| self.tracks.get(&al.key)) {
            None => {
                f.render_widget(message(" Select an album on the left (→ to expand an artist).".into(), DIM), inner);
                return;
            }
            Some(Loadable::Loading) => {
                f.render_widget(message(" Loading tracks…".into(), DIM), inner);
                return;
            }
            Some(Loadable::Failed(e)) => {
                f.render_widget(message(format!(" Couldn't load tracks: {}", e), WARN), inner);
                return;
            }
            Some(Loadable::Ready(t)) => t,
        };

        // Column headings stay put while the track list scrolls beneath them.
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        let head = Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD);
        let rating_head = vec![Span::styled("Rating", head)];
        f.render_widget(
            Paragraph::new(track_line("#", "Title", rating_head, "Format", "Time", inner_w, [head; 4])),
            parts[0],
        );
        f.render_widget(
            Paragraph::new(Span::styled("─".repeat(inner_w), Style::default().fg(BORDER))),
            parts[1],
        );

        let multi_disc = tracks.iter().any(|t| t.disc.unwrap_or(1) > 1);
        let playing_key = st.track.as_ref().map(|t| t.key.clone());
        let focused = self.focus == Focus::Tracks;
        let selected = self.track_state.selected();
        let items: Vec<ListItem> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let playing = playing_key.as_deref() == Some(t.key.as_str()) && st.state != PlayState::Stopped;
                let num = if playing {
                    "▶".to_string()
                } else if multi_disc {
                    format!("{}-{}", t.disc.unwrap_or(1), t.index.unwrap_or(0))
                } else {
                    t.index.map(|i| i.to_string()).unwrap_or_default()
                };
                let styles = if playing {
                    [Style::default().fg(ACCENT).add_modifier(Modifier::BOLD); 4]
                } else if selected == Some(i) {
                    // The highlight only sets the background (so the stars keep
                    // their colours); brighten the text here instead.
                    [Style::default().fg(BRIGHT); 4]
                } else {
                    [
                        Style::default().fg(FAINT),
                        Style::default().fg(FG),
                        Style::default().fg(DIM),
                        Style::default().fg(DIM),
                    ]
                };
                let time = t.duration_ms.map(|d| mmss(d as f64 / 1000.0)).unwrap_or_default();
                // Unrated tracks leave the column blank to keep the list calm.
                let stars = match self.rating_of(t) {
                    Some(r) => star_spans(r, if playing { DIM } else { FAINT }),
                    None => Vec::new(),
                };
                ListItem::new(track_line(&num, &t.title, stars, &t.format_label(), &time, inner_w, styles))
            })
            .collect();
        let list = List::new(items)
            .scroll_padding(2)
            .highlight_style(Style::default().bg(if focused { SEL_BG } else { Color::Reset }));
        f.render_stateful_widget(list, parts[2], &mut self.track_state);
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let keys = [
            ("↑↓", "move"),
            ("→/←", "expand"),
            ("enter", "play"),
            ("space", "pause"),
            ("n/p", "next/prev"),
            (",/.", "seek"),
            ("0-5", "rate"),
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

/// Pane titles light up in the accent colour only on the focused pane.
fn title_style(focused: bool) -> Style {
    Style::default().fg(if focused { ACCENT } else { BRIGHT }).add_modifier(Modifier::BOLD)
}

/// One row of the track table. `styles` colours the number, title, format and
/// time columns separately so the title stands out from the details; the
/// rating column brings its own colours.
fn track_line(
    num: &str,
    title: &str,
    rating: Vec<Span<'static>>,
    fmt: &str,
    time: &str,
    width: usize,
    styles: [Style; 4],
) -> Line<'static> {
    let num_w = 5;
    let fmt_w = 12;
    let time_w = 7;
    let title_w = width.saturating_sub(num_w + RATING_W + fmt_w + time_w + 2).max(8);
    let rating_used: usize = rating.iter().map(|s| s.content.chars().count()).sum();
    let mut spans = vec![
        Span::styled(format!(" {:<num_w$}", truncate(num, num_w - 1)), styles[0]),
        // Leave a gap so a long title never runs into the stars.
        Span::styled(format!("{:<title_w$}", truncate(title, title_w.saturating_sub(2))), styles[1]),
    ];
    spans.extend(rating);
    spans.push(Span::raw(" ".repeat(RATING_W.saturating_sub(rating_used))));
    spans.push(Span::styled(format!("{:<fmt_w$}", truncate(fmt, fmt_w - 1)), styles[2]));
    spans.push(Span::styled(format!("{:>time_w$}", time), styles[3]));
    Line::from(spans)
}

/// Five stars for a rating in Plex's units (1-10, two per star): full stars in
/// the accent, a half star as a dimmer accent star, the rest as outlines in `empty`.
fn star_spans(rating: u8, empty: Color) -> Vec<Span<'static>> {
    let r = rating.min(10) as usize;
    let (full, half) = (r / 2, r % 2);
    let mut spans = Vec::new();
    if full > 0 {
        spans.push(Span::styled("★".repeat(full), Style::default().fg(ACCENT)));
    }
    if half > 0 {
        spans.push(Span::styled("★", Style::default().fg(HALF_STAR)));
    }
    let rest = 5 - full - half;
    if rest > 0 {
        spans.push(Span::styled("☆".repeat(rest), Style::default().fg(empty)));
    }
    spans
}

/// The rating a key sets, given the track's current rating. `None` means the
/// key doesn't change anything; `Some(None)` clears the rating.
fn rating_for_key(code: KeyCode, current: Option<u8>) -> Option<Option<u8>> {
    match code {
        KeyCode::Char('0') => Some(None),
        KeyCode::Char(c @ '1'..='5') => Some(Some((c as u8 - b'0') * 2)),
        // Half a star down; going below half a star clears the rating.
        KeyCode::Char('-') => current.map(|r| Some(r - 1).filter(|&r| r > 0)),
        // Half a star up, starting from nothing on an unrated track.
        KeyCode::Char('=') | KeyCode::Char('+') => Some(Some((current.unwrap_or(0) + 1).min(10))),
        _ => None,
    }
}

/// Saves rating changes to Plex one at a time, in the order you make them,
/// so quick changes in a row can't land out of order.
fn spawn_rater(server: Server, tx: Sender<Msg>) -> Sender<RateJob> {
    let (job_tx, job_rx) = mpsc::channel::<RateJob>();
    thread::spawn(move || {
        for job in job_rx {
            let result = server.rate(&job.key, job.wanted).map_err(|e| e.to_string());
            let msg = Msg::Rated {
                key: job.key,
                wanted: job.wanted,
                previous: job.previous,
                result,
            };
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    job_tx
}

/// `rating` is the playing track's latest rating (`Some(None)` when unrated).
fn draw_now_playing(f: &mut Frame, area: Rect, st: &Status, error: Option<&str>, rating: Option<Option<u8>>) {
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
            // Faint outlines when unrated, so you can see there's a rating to set.
            l1.push(Span::raw("  "));
            l1.extend(star_spans(rating.flatten().unwrap_or(0), BORDER));
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
    if let Some(e) = error {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex::Plex;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn number_keys_set_whole_stars_and_zero_clears() {
        assert_eq!(rating_for_key(KeyCode::Char('1'), None), Some(Some(2)));
        assert_eq!(rating_for_key(KeyCode::Char('4'), Some(9)), Some(Some(8)));
        assert_eq!(rating_for_key(KeyCode::Char('5'), Some(3)), Some(Some(10)));
        assert_eq!(rating_for_key(KeyCode::Char('0'), Some(7)), Some(None));
    }

    #[test]
    fn plus_and_minus_move_by_half_a_star() {
        assert_eq!(rating_for_key(KeyCode::Char('='), Some(8)), Some(Some(9)));
        assert_eq!(rating_for_key(KeyCode::Char('+'), Some(8)), Some(Some(9)));
        assert_eq!(rating_for_key(KeyCode::Char('='), None), Some(Some(1)));
        assert_eq!(rating_for_key(KeyCode::Char('='), Some(10)), Some(Some(10)));
        assert_eq!(rating_for_key(KeyCode::Char('-'), Some(9)), Some(Some(8)));
        // Half a star down from half a star clears it; nothing to lower when unrated.
        assert_eq!(rating_for_key(KeyCode::Char('-'), Some(1)), Some(None));
        assert_eq!(rating_for_key(KeyCode::Char('-'), None), None);
        assert_eq!(rating_for_key(KeyCode::Char('x'), Some(4)), None);
    }

    fn stars_text(r: u8) -> (String, Vec<Color>) {
        let spans = star_spans(r, FAINT);
        let text = spans.iter().map(|s| s.content.as_ref()).collect();
        let colors = spans
            .iter()
            .flat_map(|s| std::iter::repeat(s.style.fg.unwrap()).take(s.content.chars().count()))
            .collect();
        (text, colors)
    }

    #[test]
    fn stars_are_always_five_wide_with_a_dim_half_star() {
        for r in 0..=10 {
            assert_eq!(stars_text(r).0.chars().count(), 5, "rating {}", r);
        }
        assert_eq!(stars_text(10).0, "★★★★★");
        assert_eq!(stars_text(6).0, "★★★☆☆");
        let (text, colors) = stars_text(7);
        assert_eq!(text, "★★★★☆");
        assert_eq!(colors, vec![ACCENT, ACCENT, ACCENT, HALF_STAR, FAINT]);
    }

    fn track(key: &str, index: u32, title: &str, rating: Option<u8>) -> Track {
        Track {
            key: key.into(),
            title: title.into(),
            album_key: Some("100".into()),
            artist: "Cat Power".into(),
            album: "You Are Free".into(),
            index: Some(index),
            disc: Some(1),
            duration_ms: Some(185_000),
            codec: Some("flac".into()),
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            channels: Some(2),
            bitrate_kbps: Some(641),
            part_key: None,
            container: None,
            rating,
        }
    }

    /// An app with one album open in the track list, talking to a server
    /// address nothing listens on.
    fn app_with_album(tracks: Vec<Track>) -> App {
        let server = Server::new(Plex::new("plext-test"), "http://127.0.0.1:9", "token", "machine");
        let header = HeaderInfo {
            server_name: "Home".into(),
            library_title: "Music".into(),
        };
        let mut app = App::new(server, "1".into(), header, "null".into(), false, true);
        let album = Album {
            key: "100".into(),
            title: "You Are Free".into(),
            year: Some(2003),
            kind: ReleaseKind::Album,
        };
        for t in &tracks {
            app.ratings.insert(t.key.clone(), t.rating);
        }
        app.tracks.insert(album.key.clone(), Loadable::Ready(tracks));
        app.shown_album = Some(("Cat Power".into(), album));
        app.focus = Focus::Tracks;
        app
    }

    fn render(app: &mut App, st: &Status) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(140, 30)).unwrap();
        term.draw(|f| app.draw(f, st)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    #[test]
    fn track_list_and_now_playing_show_ratings() {
        let tracks = vec![
            track("1", 1, "I Don't Blame You", Some(8)),
            track("2", 2, "Free", None),
            track("3", 3, "Good Woman", Some(9)),
        ];
        let mut app = app_with_album(tracks.clone());
        let st = Status {
            state: PlayState::Playing,
            track: Some(tracks[0].clone()),
            dur: Some(185.0),
            queue_len: 3,
            ..Status::default()
        };
        let screen = render(&mut app, &st);
        let row = |needle: &str| screen.iter().find(|l| l.contains(needle) && !l.contains("PLAYING")).unwrap().clone();
        assert!(screen.iter().any(|l| l.contains("Rating") && l.contains("Format")));
        assert!(row("Good Woman").contains("★★★★★"));
        assert!(row("I Don't Blame You").contains("★★★★☆"));
        assert!(!row("Free").contains('★') && !row("Free").contains('☆'));
        let playing = screen.iter().find(|l| l.contains("PLAYING")).unwrap();
        assert!(playing.contains("I Don't Blame You  ★★★★☆"));
    }

    #[test]
    fn rating_keys_target_the_highlighted_track_in_the_track_list() {
        let mut app = app_with_album(vec![track("1", 1, "One", None), track("2", 2, "Two", Some(4))]);
        app.track_state.select(Some(1));
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        assert_eq!(app.ratings.get("2"), Some(&Some(8)));
        assert_eq!(app.pending_ratings.get("2"), Some(&1));
        app.on_key(KeyEvent::from(KeyCode::Char('=')));
        assert_eq!(app.ratings.get("2"), Some(&Some(9)));
        assert_eq!(app.ratings.get("1"), Some(&None));
    }

    #[test]
    fn a_rating_that_fails_to_save_is_put_back() {
        let mut app = app_with_album(vec![track("1", 1, "One", Some(6))]);
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        assert_eq!(app.ratings.get("1"), Some(&Some(4)));
        // Nothing listens on the test server, so the save fails.
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.pending_ratings.contains_key("1") && Instant::now() < deadline {
            if let Ok(m) = app.rx.recv_timeout(Duration::from_millis(100)) {
                app.on_msg(m);
            }
        }
        assert_eq!(app.ratings.get("1"), Some(&Some(6)));
        assert!(app.notice.as_ref().unwrap().0.starts_with("Couldn't save rating"));
    }

    #[test]
    fn a_refreshed_track_list_brings_in_new_ratings_but_keeps_unsaved_ones() {
        let mut app = app_with_album(vec![track("1", 1, "One", None), track("2", 2, "Two", Some(4))]);
        app.pending_ratings.insert("2".into(), 1);
        app.ratings.insert("2".into(), Some(10));
        app.refreshing.insert("100".into());
        let fresh = vec![track("1", 1, "One", Some(7)), track("2", 2, "Two", Some(4))];
        app.on_msg(Msg::Tracks("100".into(), Ok(fresh)));
        assert_eq!(app.ratings.get("1"), Some(&Some(7)));
        assert_eq!(app.ratings.get("2"), Some(&Some(10)));
        // A refresh that fails keeps the list we already had.
        app.refreshing.insert("100".into());
        app.on_msg(Msg::Tracks("100".into(), Err("offline".into())));
        assert!(matches!(app.tracks.get("100"), Some(Loadable::Ready(t)) if t.len() == 2));
    }
}
