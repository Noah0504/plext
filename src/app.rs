//! Terminal user interface: artist/album tree, track list and now-playing bar.

use crate::covers::Covers;
use crate::mpris::Mpris;
use crate::output::Route;
use crate::pixelart::{self, PixelArt};
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
    /// A cover finished turning into pixel art (keyed by its Plex thumb path).
    /// The third value is the width in columns it was made for.
    Art(String, usize, Result<PixelArt, String>),
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

/// The playing album's cover, on its way to the screen.
struct ArtSlot {
    thumb: String,
    /// Width in columns of the art being made (or made).
    cols: usize,
    state: ArtState,
}

enum ArtState {
    /// Waiting for the download.
    Waiting,
    Decoding,
    Ready(PixelArt),
    Failed,
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
    _mpris: Option<Mpris>,
    covers: Option<Covers>,
    /// Show the cover as pixel art in a taller now-playing area.
    album_art: bool,
    truecolor: bool,
    art: Option<ArtSlot>,
    /// Width of the cover in columns, chosen so it looks square in this
    /// terminal's font.
    art_cols: usize,
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
        target: crate::output::Target,
        report_playback: bool,
        group_releases: bool,
        mpris: bool,
        album_art: bool,
    ) -> App {
        let (tx, rx) = mpsc::channel();
        let player = Player::spawn(server.clone(), target);
        let reporter = report_playback.then(|| {
            Reporter::spawn(
                server.clone(),
                player.status_handle(),
                crate::config::new_client_id().replacen("plext-", "plext-session-", 1),
            )
        });
        let covers = (mpris || album_art).then(|| Covers::open(server.clone())).flatten();
        let mpris = mpris.then(|| Mpris::spawn(player.status_handle(), player.commander(), covers.clone()));
        let rate_tx = spawn_rater(server.clone(), tx.clone());
        let mut app = App {
            _reporter: reporter,
            _mpris: mpris,
            covers,
            album_art,
            truecolor: pixelart::truecolor(),
            art: None,
            art_cols: art_columns(),
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
            // Cheap to ask, and the font size can change while we run.
            self.art_cols = art_columns();
            self.update_art(&status);
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

    /// "Album · 2019 · EP" for a track, from the album lists already loaded.
    fn album_line(&self, t: &Track) -> String {
        let album = t.album_key.as_deref().and_then(|key| {
            self.shown_album
                .as_ref()
                .map(|(_, a)| a)
                .filter(|a| a.key == key)
                .or_else(|| {
                    self.albums.values().find_map(|l| match l {
                        Loadable::Ready(v) => v.iter().find(|a| a.key == key),
                        _ => None,
                    })
                })
        });
        let mut parts = vec![t.album.clone()];
        if let Some(a) = album {
            if let Some(y) = a.year {
                parts.push(y.to_string());
            }
            if let Some(l) = a.kind.display_label() {
                parts.push(l.to_string());
            }
        }
        parts.join(" · ")
    }

    /// Keeps the cover for the playing track on its way: asks for the
    /// download, then turns the file into pixel art on a background thread.
    fn update_art(&mut self, st: &Status) {
        if !self.album_art {
            return;
        }
        let Some(covers) = self.covers.as_ref() else { return };
        let thumb = st
            .track
            .as_ref()
            .filter(|_| st.state != PlayState::Stopped)
            .and_then(|t| t.thumb.clone());
        let Some(thumb) = thumb else {
            self.art = None;
            return;
        };
        if self.art.as_ref().map(|a| a.thumb != thumb).unwrap_or(true) {
            covers.request(&thumb);
            self.art = Some(ArtSlot {
                thumb: thumb.clone(),
                cols: self.art_cols,
                state: ArtState::Waiting,
            });
        }
        let cols = self.art_cols;
        let slot = self.art.as_mut().unwrap();
        // Make it when the cover arrives, and again if the font or its size
        // changed and the art needs a different width to stay square.
        let resized = slot.cols != cols && matches!(slot.state, ArtState::Ready(_) | ArtState::Failed);
        if matches!(slot.state, ArtState::Waiting) || resized {
            if let Some(path) = covers.cached(&thumb) {
                slot.state = ArtState::Decoding;
                slot.cols = cols;
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let r = pixelart::from_file(&path, cols).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Art(thumb, cols, r));
                });
            }
        }
    }

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
            Msg::Art(thumb, cols, r) => {
                if let Some(slot) = self.art.as_mut().filter(|a| a.thumb == thumb && a.cols == cols) {
                    slot.state = match r {
                        Ok(art) => ArtState::Ready(art),
                        Err(e) => {
                            crate::log::write(&format!("cover: couldn't read {}: {}", thumb, e));
                            ArtState::Failed
                        }
                    };
                }
            }
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
        let tall = self.album_art && area.width >= TALL_MIN_W && area.height >= TALL_MIN_H;
        f.render_widget(Block::default().style(Style::default().fg(FG)), area);
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Length(1), // breathing room under the header
                Constraint::Min(5),
                Constraint::Length(if tall { NOW_PLAYING_TALL_H } else { NOW_PLAYING_H }),
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
        let art = tall.then(|| ArtView {
            art: match self.art.as_ref().map(|a| &a.state) {
                Some(ArtState::Ready(p)) => Some(p),
                _ => None,
            },
            cols: self.art_cols as u16,
            truecolor: self.truecolor,
            album_line: st.track.as_ref().map(|t| self.album_line(t)),
        });
        draw_now_playing(f, outer[3], st, error, rating, art);
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
                let name = Span::styled(al.title.clone(), title_style(self.focus == Focus::Tracks));
                let by = Span::styled(format!(" · {}", artist), Style::default().fg(DIM));
                let mut extra = Vec::new();
                if let Some(y) = al.year {
                    extra.push(Span::styled(format!(" · {}", y), Style::default().fg(DIM)));
                }
                if let Some(kind) = al.kind.display_label() {
                    extra.push(Span::styled(format!(" · {}", kind), Style::default().fg(DIM)));
                }
                let rights = match self.tracks.get(&al.key) {
                    Some(Loadable::Ready(tr)) => {
                        let total: u64 = tr.iter().filter_map(|t| t.duration_ms).sum();
                        let fmt = tr.first().map(|t| t.format_label()).unwrap_or_default();
                        let codec = tr
                            .first()
                            .and_then(|t| t.codec.clone())
                            .map(|c| c.to_uppercase())
                            .unwrap_or_default();
                        let summary = format!("{} tracks · {}", tr.len(), mmss(total as f64 / 1000.0));
                        vec![format!(" {} {} · {} ", codec, fmt, summary), format!(" {} ", summary)]
                    }
                    _ => Vec::new(),
                };
                fit_pane_titles(area.width, name, by, extra, rights)
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

/// Height of the now-playing box: compact, or tall with the cover beside it.
const NOW_PLAYING_H: u16 = 5;
const NOW_PLAYING_TALL_H: u16 = 2 + (pixelart::SIZE / 2) as u16;
/// The tall layout only when the terminal has room for it.
const TALL_MIN_W: u16 = 90;
const TALL_MIN_H: u16 = 30;

/// What the cover area shows in the tall layout.
pub struct ArtView<'a> {
    pub art: Option<&'a PixelArt>,
    /// Columns to give the cover.
    pub cols: u16,
    pub truecolor: bool,
    /// "Album · 2019 · EP", when the album's details are known.
    pub album_line: Option<String>,
}

/// The "up next" column, and the text width it needs before it appears
/// (about a 100-column terminal).
const UP_NEXT_W: u16 = 34;
const UP_NEXT_GAP: u16 = 4;
const UP_NEXT_MIN_W: u16 = UP_NEXT_W + UP_NEXT_GAP + 40;

/// `rating` is the playing track's latest rating (`Some(None)` when unrated).
/// With `art`, draws the tall layout: the cover on the left, then the title,
/// artist and album on their own lines.
fn draw_now_playing(
    f: &mut Frame,
    area: Rect,
    st: &Status,
    error: Option<&str>,
    rating: Option<Option<u8>>,
    art: Option<ArtView>,
) {
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
    let playing = st.state != PlayState::Stopped;
    let track = st.track.as_ref().filter(|_| playing);

    let Some(art) = art else {
        // Compact: title line, progress, details.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
            .split(inner);
        let mut l1 = state_and_title(st, rating);
        if let Some(t) = track {
            l1.push(Span::styled(format!("   {} — {}", t.artist, t.album), Style::default().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(l1)), rows[0]);
        draw_badge(f, rows[0], st);
        draw_progress(f, rows[1], st, true);
        draw_details(f, rows[2], st, error);
        return;
    };

    // Tall: cover | gap | text. The text has a title block (with "up next"
    // beside it when there's room), the progress bar, and a details line.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(art.cols),
            Constraint::Length(2),
            Constraint::Min(10),
        ])
        .split(inner);
    draw_cover(f, cols[0], &art);
    let text = cols[2];
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 8])
        .split(text);
    let show_next = text.width >= UP_NEXT_MIN_W;
    let left_w = if show_next { text.width - UP_NEXT_W - UP_NEXT_GAP } else { text.width };
    let left = |i: usize| Rect { width: left_w, ..rows[i] };
    let right = |i: usize| Rect {
        x: text.x + text.width - UP_NEXT_W,
        width: UP_NEXT_W,
        ..rows[i]
    };
    let lw = left_w as usize;

    // Title block: state icon and title, then artist and album lined up under the title.
    let (icon, icon_color) = match st.state {
        PlayState::Playing => ("▶ ", ACCENT),
        PlayState::Paused => ("‖ ", DIM),
        PlayState::Loading => ("… ", DIM),
        PlayState::Stopped => ("■ ", FAINT),
    };
    let mut l0 = vec![Span::styled(icon, Style::default().fg(icon_color).add_modifier(Modifier::BOLD))];
    match track {
        Some(t) => {
            l0.push(Span::styled(
                truncate(&t.title, lw.saturating_sub(2 + 7).max(4)),
                Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
            ));
            l0.push(Span::raw("  "));
            l0.extend(star_spans(rating.flatten().unwrap_or(0), BORDER));
        }
        None => l0.push(Span::styled("Stopped", Style::default().fg(FAINT))),
    }
    f.render_widget(Paragraph::new(Line::from(l0)), left(0));
    if let Some(t) = track {
        let room = lw.saturating_sub(2).max(4);
        f.render_widget(
            Paragraph::new(Span::styled(format!("  {}", truncate(&t.artist, room)), Style::default().fg(BRIGHT))),
            left(1),
        );
        let album = art.album_line.clone().unwrap_or_else(|| t.album.clone());
        f.render_widget(
            Paragraph::new(Span::styled(format!("  {}", truncate(&album, room)), Style::default().fg(DIM))),
            left(2),
        );
    }

    // Up next: the following two tracks, or the end of the album.
    if show_next && track.is_some() {
        f.render_widget(Paragraph::new(Span::styled("up next", Style::default().fg(FAINT))), right(0));
        if st.next.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("end of album", Style::default().fg(FAINT))), right(1));
        }
        for (i, n) in st.next.iter().take(2).enumerate() {
            let num = n.index.map(|x| x.to_string()).unwrap_or_else(|| (st.queue_pos + 2 + i).to_string());
            let len = n.duration_ms.map(|d| mmss(d as f64 / 1000.0)).unwrap_or_default();
            let title_room = (UP_NEXT_W as usize).saturating_sub(num.chars().count() + 2 + len.chars().count() + 2);
            let title_color = if i == 0 { FG } else { DIM };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{}  ", num), Style::default().fg(FAINT)),
                    Span::styled(truncate(&n.title, title_room.max(4)), Style::default().fg(title_color)),
                ])),
                right(1 + i),
            );
            f.render_widget(
                Paragraph::new(Span::styled(len, Style::default().fg(FAINT))).alignment(Alignment::Right),
                right(1 + i),
            );
        }
    }

    // Row 3 and 6 stay blank around the progress bar (rows 4–5, which the
    // waveform will fill; the bar sits on the lower one).
    draw_progress(f, rows[5], st, false);
    draw_details_tall(f, rows[7], st, error);
}

/// The tall layout's details line: the source on the left, and on the
/// right the bit-perfect verdict, where the sound goes and how. When the
/// line is too narrow, the least important parts go first (bitrate,
/// channels, the output mode) before anything is cut short.
fn draw_details_tall(f: &mut Frame, area: Rect, st: &Status, error: Option<&str>) {
    if let Some(e) = error {
        f.render_widget(Paragraph::new(Span::styled(format!("⚠ {}", e), Style::default().fg(WARN))), area);
        return;
    }
    if st.state == PlayState::Stopped {
        return;
    }
    // Right: verdict → device · mode, with or without the mode.
    let rights: Vec<Vec<Span>> = match &st.output {
        Some(o) => {
            // Through the desktop's sound by choice: no verdict to give.
            let verdict = if let Route::SoundServer(_) = o.route {
                Span::raw("")
            } else if st.bit_perfect() {
                Span::styled("◆ BIT-PERFECT", Style::default().fg(GOOD).add_modifier(Modifier::BOLD))
            } else {
                Span::styled("◇ NOT BIT-PERFECT", Style::default().fg(WARN).add_modifier(Modifier::BOLD))
            };
            let mode = match o.route {
                Route::Direct => " · exclusive".to_string(),
                Route::AlsaPlugins => " · via ALSA plugins".to_string(),
                Route::SoundServer(kind) => format!(" · via {}", kind),
            };
            let arrow = Span::styled("  →  ", Style::default().fg(DIM));
            let device = Span::styled(o.label.clone(), Style::default().fg(BRIGHT));
            vec![
                vec![verdict.clone(), arrow.clone(), device.clone(), Span::styled(mode, Style::default().fg(DIM))],
                vec![verdict, arrow, device],
            ]
        }
        None => vec![Vec::new()],
    };
    draw_split_line(f, area, &source_variants(st), rights, 2);
}

/// "▶ PLAYING  Title  ★★★★☆" (the title and stars only while something plays).
fn state_and_title(st: &Status, rating: Option<Option<u8>>) -> Vec<Span<'static>> {
    let (label, label_color) = match st.state {
        PlayState::Playing => ("▶ PLAYING", ACCENT),
        PlayState::Paused => ("❚❚ PAUSED", DIM),
        PlayState::Loading => ("… LOADING", DIM),
        PlayState::Stopped => ("■ STOPPED", FAINT),
    };
    let mut l1 = vec![Span::styled(
        format!(" {}  ", label),
        Style::default().fg(label_color).add_modifier(Modifier::BOLD),
    )];
    if let Some(t) = st.track.as_ref().filter(|_| st.state != PlayState::Stopped) {
        l1.push(Span::styled(t.title.clone(), Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD)));
        // Faint outlines when unrated, so you can see there's a rating to set.
        l1.push(Span::raw("  "));
        l1.extend(star_spans(rating.flatten().unwrap_or(0), BORDER));
    }
    l1
}

/// The bit-perfect badge at the right end of `area`.
fn draw_badge(f: &mut Frame, area: Rect, st: &Status) {
    let chosen_shared = matches!(st.output.as_ref().map(|o| o.route), Some(Route::SoundServer(_)));
    if st.output.is_some() && !chosen_shared {
        let badge = if st.bit_perfect() {
            Span::styled("◆ BIT-PERFECT ", Style::default().fg(GOOD).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("◇ NOT BIT-PERFECT ", Style::default().fg(WARN).add_modifier(Modifier::BOLD))
        };
        f.render_widget(Paragraph::new(Line::from(badge)).alignment(Alignment::Right), area);
    }
}

/// Elapsed time, progress bar, duration.
/// With `indent`, a space before the elapsed time and after the duration
/// (the compact bar's spacing).
fn draw_progress(f: &mut Frame, area: Rect, st: &Status, indent: bool) {
    let dur = st.dur.unwrap_or(0.0);
    let ratio = if dur > 0.0 { (st.pos / dur).clamp(0.0, 1.0) } else { 0.0 };
    let bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(9), Constraint::Min(4), Constraint::Length(9)])
        .split(area);
    let playing = st.state != PlayState::Stopped;
    f.render_widget(
        Paragraph::new(Span::styled(
            if playing { format!("{}{}", if indent { " " } else { "" }, mmss(st.pos)) } else { String::new() },
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
            if playing && dur > 0.0 { format!("{}{}", mmss(dur), if indent { " " } else { "" }) } else { String::new() },
            Style::default().fg(DIM),
        ))
        .alignment(Alignment::Right),
        bar[2],
    );
}

/// Source and output details, or the latest error in their place.
/// Fits the track list's border headings into a pane `width` wide: the
/// album heading on the left (name, artist, then extras like year and
/// type) and the summary on the right (fullest form first). Pieces go
/// whole, least important first: the format, then the whole summary, then
/// the extras; only then are the artist and finally the name shortened.
fn fit_pane_titles(
    width: u16,
    name: Span<'static>,
    by: Span<'static>,
    extra: Vec<Span<'static>>,
    rights: Vec<String>,
) -> (Line<'static>, String) {
    // Two corners, the spaces around the heading, and a gap of at least one.
    let avail = (width as usize).saturating_sub(5);
    let n = |s: &Span| s.content.chars().count();
    let line = |spans: Vec<Span<'static>>| {
        let mut v = vec![Span::raw(" ")];
        v.extend(spans);
        v.push(Span::raw(" "));
        Line::from(v)
    };
    let full_left = n(&name) + n(&by) + extra.iter().map(n).sum::<usize>();
    for r in &rights {
        if full_left + r.chars().count() <= avail {
            let mut spans = vec![name, by];
            spans.extend(extra);
            return (line(spans), r.clone());
        }
    }
    if full_left <= avail {
        let mut spans = vec![name, by];
        spans.extend(extra);
        return (line(spans), String::new());
    }
    if n(&name) + n(&by) <= avail {
        return (line(vec![name, by]), String::new());
    }
    // Shorten the artist, keeping at least " · X…"; then the name.
    let by_room = avail.saturating_sub(n(&name));
    if by_room >= 5 {
        let short = truncate(&by.content, by_room);
        return (line(vec![name, Span::styled(short, by.style)]), String::new());
    }
    let short = truncate(&name.content, avail.max(1));
    (line(vec![Span::styled(short, name.style)]), String::new())
}

/// The compact bar's details line: source on the left, output on the
/// right. Like the tall layout's, it drops pieces whole when narrow.
fn draw_details(f: &mut Frame, area: Rect, st: &Status, error: Option<&str>) {
    if let Some(e) = error {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ⚠ {}", e), Style::default().fg(WARN))),
            area,
        );
        return;
    }
    if st.state == PlayState::Stopped {
        return;
    }
    let sources: Vec<String> = source_variants(st).into_iter().map(|s| format!(" {}", s)).collect();
    let rights: Vec<Vec<Span>> = match &st.output {
        Some(o) => {
            let out = Span::styled("out: ", Style::default().fg(DIM));
            let device = Span::styled(o.label.clone(), Style::default().fg(BRIGHT));
            let dim = |t: String| Span::styled(t, Style::default().fg(DIM));
            let modes: Vec<String> = match o.route {
                Route::Direct => vec![
                    format!(" · {} · exclusive · no resample · vol: DAC ", o.fmt.name()),
                    format!(" · {} · exclusive ", o.fmt.name()),
                    " ".into(),
                ],
                Route::AlsaPlugins => {
                    vec![format!(" · {} · via ALSA plugins ", o.fmt.name()), " · via ALSA plugins ".into(), " ".into()]
                }
                Route::SoundServer(kind) => {
                    vec![format!(" · via {} · vol: system ", kind), format!(" · via {} ", kind), " ".into()]
                }
            };
            modes.into_iter().map(|m| vec![out.clone(), device.clone(), dim(m)]).collect()
        }
        None => vec![Vec::new()],
    };
    draw_split_line(f, area, &sources, rights, 1);
}

/// The source description, from most to least detailed.
fn source_variants(st: &Status) -> Vec<String> {
    let Some(s) = &st.source else { return vec![String::new()] };
    let ch = match s.channels {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{} ch", n),
    };
    let base = if s.lossless {
        format!("{} · {}-bit / {} kHz", s.codec, s.bits, khz(s.rate))
    } else {
        format!("{} · {} kHz · lossy", s.codec, khz(s.rate))
    };
    let mut v = Vec::new();
    if let Some(br) = st.track.as_ref().and_then(|t| t.bitrate_kbps) {
        v.push(format!("{} · {} · {} kbps", base, ch, br));
    }
    v.push(format!("{} · {}", base, ch));
    v.push(base);
    v
}

/// Draws `sources[i]` on the left and `rights[j]` right-aligned, choosing
/// the fullest pair that fits with a two-column gap (the right side's
/// detail is kept longest). If even the shortest pair doesn't fit, the
/// span at `shrink` in the right side (the device name) is shortened.
fn draw_split_line(f: &mut Frame, area: Rect, sources: &[String], rights: Vec<Vec<Span<'static>>>, shrink: usize) {
    let width = area.width as usize;
    let len = |spans: &[Span]| spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
    let mut pick = (sources.len() - 1, rights.len() - 1);
    'search: for (ri, r) in rights.iter().enumerate() {
        for (si, src) in sources.iter().enumerate() {
            if src.chars().count() + 2 + len(r) <= width {
                pick = (si, ri);
                break 'search;
            }
        }
    }
    let src = &sources[pick.0];
    let mut right = rights.into_iter().nth(pick.1).unwrap_or_default();
    let min_src = src.chars().count().min(12);
    let max_right = width.saturating_sub(min_src + 2);
    if len(&right) > max_right {
        let over = len(&right) - max_right;
        if let Some(dev) = right.get_mut(shrink) {
            let keep = dev.content.chars().count().saturating_sub(over).max(4);
            *dev = Span::styled(truncate(&dev.content, keep), dev.style);
        }
    }
    let right_w = len(&right).min(width) as u16;
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_w)])
        .split(area);
    let room = (parts[0].width as usize).saturating_sub(2);
    if room >= 4 && !src.trim().is_empty() {
        f.render_widget(Paragraph::new(Span::styled(truncate(src, room), Style::default().fg(DIM))), parts[0]);
    }
    if !right.is_empty() {
        f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), parts[1]);
    }
}

/// How many columns the cover needs to look square, from the size of a
/// character cell in pixels. Most terminals report it; when one doesn't,
/// a typical font's proportions are assumed.
fn art_columns() -> usize {
    let ratio = ratatui::crossterm::terminal::window_size()
        .ok()
        .filter(|w| w.width > 0 && w.height > 0 && w.columns > 0 && w.rows > 0)
        .map(|w| (w.height as f64 / w.rows as f64) / (w.width as f64 / w.columns as f64))
        .unwrap_or(pixelart::DEFAULT_CELL_RATIO);
    pixelart::columns_for(ratio)
}

fn art_color(rgb: [u8; 3], truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb(rgb[0], rgb[1], rgb[2])
    } else {
        Color::Indexed(pixelart::to_256(rgb))
    }
}

/// The cover as pixel art: each cell is "▀", its foreground the upper pixel
/// and its background the lower one. Without a cover, a quiet placeholder.
fn draw_cover(f: &mut Frame, area: Rect, view: &ArtView) {
    let buf = f.buffer_mut();
    let w = view.art.map(|a| a.width as u16).unwrap_or(view.cols).min(view.cols).min(area.width);
    let h = ((pixelart::SIZE / 2) as u16).min(area.height);
    for cy in 0..h {
        for cx in 0..w {
            let cell = &mut buf[(area.x + cx, area.y + cy)];
            match view.art {
                Some(art) => {
                    let (x, y) = (cx as usize, cy as usize * 2);
                    cell.set_symbol("▀")
                        .set_fg(art_color(art.at(x, y), view.truecolor))
                        .set_bg(art_color(art.at(x, y + 1), view.truecolor));
                }
                None => {
                    cell.set_symbol(" ").set_bg(SEL_BG);
                }
            }
        }
    }
    if view.art.is_none() && w >= 2 && h >= 1 {
        let cell = &mut buf[(area.x + w / 2 - 1, area.y + h / 2)];
        cell.set_symbol("♪").set_fg(FAINT);
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
            thumb: None,
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
        let mut app = App::new(server, "1".into(), header, crate::output::Target::Alsa("null".into()), false, true, false, false);
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

    fn playing_status(t: &Track) -> Status {
        Status {
            state: PlayState::Playing,
            track: Some(t.clone()),
            pos: 25.0,
            dur: Some(185.0),
            queue_len: 1,
            ..Status::default()
        }
    }

    fn render_sized(app: &mut App, st: &Status, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f, st)).unwrap();
        term.backend().buffer().clone()
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect()
    }

    /// Top-left cell of the cover: inside the border and the breathing column.
    fn cover_origin(buf: &ratatui::buffer::Buffer) -> (u16, u16) {
        let h = buf.area.height;
        (2, h - 1 - NOW_PLAYING_TALL_H + 1)
    }

    #[test]
    fn tall_layout_draws_the_cover_two_pixels_per_cell() {
        let t = track("1", 1, "Northbound", Some(8));
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = true;
        app.truecolor = true;
        app.art_cols = 16;
        // Top half of each pixel pair red, bottom half blue: rows alternate.
        let pixels = (0..pixelart::SIZE * pixelart::SIZE)
            .map(|i| if (i / pixelart::SIZE) % 2 == 0 { [200, 0, 0] } else { [0, 0, 200] })
            .collect();
        app.art = Some(ArtSlot { thumb: "/thumb".into(), cols: 16, state: ArtState::Ready(PixelArt { width: 16, pixels }) });
        let buf = render_sized(&mut app, &playing_status(&t), 140, 40);
        let (x0, y0) = cover_origin(&buf);
        for cy in 0..(pixelart::SIZE / 2) as u16 {
            for cx in 0..pixelart::SIZE as u16 {
                let c = &buf[(x0 + cx, y0 + cy)];
                assert_eq!(c.symbol(), "▀", "cell {},{}", cx, cy);
                assert_eq!(c.fg, Color::Rgb(200, 0, 0));
                assert_eq!(c.bg, Color::Rgb(0, 0, 200));
            }
        }
        // Title, artist and album each get a line beside the cover, the
        // album with its year; the progress bar sits on row 5.
        let text_rows: Vec<String> = (y0..y0 + 8).map(|y| row_text(&buf, y)).collect();
        assert!(text_rows[0].contains("▶ Northbound  ★★★★☆"), "{}", text_rows[0]);
        assert!(text_rows[1].contains("  Cat Power"));
        assert!(text_rows[2].contains("  You Are Free · 2003"));
        assert!(text_rows[5].contains("0:25") && text_rows[5].contains("3:05"));
    }

    /// A row's text without the box's right edge.
    fn inside(row: &str) -> &str {
        row.trim_end().trim_end_matches('│').trim_end()
    }

    fn tall_rows(app: &mut App, st: &Status, w: u16) -> Vec<String> {
        let buf = render_sized(app, st, w, 40);
        let (_, y0) = cover_origin(&buf);
        (y0..y0 + 8).map(|y| row_text(&buf, y)).collect()
    }

    #[test]
    fn up_next_lists_the_next_two_tracks_when_there_is_room() {
        let tracks = vec![
            track("1", 1, "One", None),
            track("2", 2, "Two", None),
            track("3", 3, "Three", None),
            track("4", 4, "Four", None),
        ];
        let mut app = app_with_album(tracks.clone());
        app.album_art = true;
        let mut st = playing_status(&tracks[0]);
        st.queue_len = 4;
        st.next = vec![tracks[1].clone(), tracks[2].clone()];
        let rows = tall_rows(&mut app, &st, 140);
        assert!(rows[0].contains("up next"));
        assert!(rows[1].contains("2  Two") && inside(&rows[1]).ends_with("3:05"));
        assert!(rows[2].contains("3  Three"));
        assert!(!rows.iter().any(|r| r.contains("Four")));

        // On the last track it says so; in a narrower terminal the column goes.
        st.next.clear();
        let rows = tall_rows(&mut app, &st, 140);
        assert!(rows[1].contains("end of album"));
        let rows = tall_rows(&mut app, &st, 96);
        assert!(!rows.iter().any(|r| r.contains("up next") || r.contains("end of album")));
        assert!(rows[0].contains("▶ One"));
    }

    #[test]
    fn tall_details_line_shows_source_and_where_it_plays() {
        use crate::decode::SourceInfo;
        use crate::output::{OutputInfo, Route, SampleFmt};
        let t = track("1", 1, "One", None);
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = true;
        let mut st = playing_status(&t);
        st.source = Some(SourceInfo {
            codec: "FLAC".into(),
            lossless: true,
            rate: 44_100,
            channels: 2,
            bits: 16,
            duration_secs: Some(185.0),
        });
        st.output = Some(OutputInfo { label: "Schiit Modi+".into(), rate: 44_100, fmt: SampleFmt::S16, route: Route::Direct });
        let rows = tall_rows(&mut app, &st, 140);
        assert!(rows[7].contains("FLAC · 16-bit / 44.1 kHz · stereo · 641 kbps"), "{}", rows[7]);
        assert!(inside(&rows[7]).ends_with("◆ BIT-PERFECT  →  Schiit Modi+ · exclusive"), "{}", rows[7]);
        // Narrower: details drop out whole, nothing is cut mid-word.
        let rows = tall_rows(&mut app, &st, 96);
        assert!(!rows[7].contains('…'), "{}", rows[7]);
        assert!(rows[7].contains("FLAC · 16-bit / 44.1 kHz") && rows[7].contains("◆ BIT-PERFECT  →  Schiit Modi+"), "{}", rows[7]);
    }

    #[test]
    fn desktop_sound_shows_where_it_plays_without_a_verdict() {
        use crate::decode::SourceInfo;
        use crate::output::{OutputInfo, Route, SampleFmt};
        let t = track("1", 1, "One", None);
        let mut st = playing_status(&t);
        st.source = Some(SourceInfo {
            codec: "FLAC".into(),
            lossless: true,
            rate: 44_100,
            channels: 2,
            bits: 16,
            duration_secs: Some(185.0),
        });
        st.output = Some(OutputInfo {
            label: "Built-in Audio".into(),
            rate: 44_100,
            fmt: SampleFmt::S16,
            route: Route::SoundServer("PipeWire"),
        });
        // Tall layout.
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = true;
        let rows = tall_rows(&mut app, &st, 140);
        assert!(inside(&rows[7]).ends_with("→  Built-in Audio · via PipeWire"), "{}", rows[7]);
        assert!(!rows.iter().any(|r| r.contains("BIT-PERFECT")));
        // Compact layout.
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = false;
        let buf = render_sized(&mut app, &st, 140, 40);
        let screen: Vec<String> = (0..40).map(|y| row_text(&buf, y)).collect();
        assert!(screen.iter().any(|l| l.contains("out: Built-in Audio · via PipeWire · vol: system")), "{:#?}", screen);
        assert!(!screen.iter().any(|l| l.contains("BIT-PERFECT")));
    }

    #[test]
    fn the_cover_takes_as_many_columns_as_the_font_needs() {
        let t = track("1", 1, "Northbound", None);
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = true;
        app.art_cols = 20;
        let pixels = vec![[9, 9, 9]; 20 * pixelart::SIZE];
        app.art = Some(ArtSlot { thumb: "/thumb".into(), cols: 20, state: ArtState::Ready(PixelArt { width: 20, pixels }) });
        let buf = render_sized(&mut app, &playing_status(&t), 140, 40);
        let (x0, y0) = cover_origin(&buf);
        assert_eq!(buf[(x0 + 19, y0)].symbol(), "▀");
        assert_ne!(buf[(x0 + 20, y0)].symbol(), "▀");
        // The text starts two columns after the cover.
        assert!(row_text(&buf, y0)[..].contains("▶ Northbound"));
        assert_eq!(buf[(x0 + 22, y0)].symbol(), "▶");
    }

    #[test]
    fn without_truecolor_the_cover_uses_the_256_color_palette() {
        let t = track("1", 1, "Northbound", None);
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = true;
        app.truecolor = false;
        app.art_cols = 16;
        let pixels = vec![[255, 0, 0]; pixelart::SIZE * pixelart::SIZE];
        app.art = Some(ArtSlot { thumb: "/thumb".into(), cols: 16, state: ArtState::Ready(PixelArt { width: 16, pixels }) });
        let buf = render_sized(&mut app, &playing_status(&t), 140, 40);
        let (x0, y0) = cover_origin(&buf);
        assert_eq!(buf[(x0, y0)].fg, Color::Indexed(196));
    }

    #[test]
    fn a_cover_still_on_its_way_shows_a_placeholder() {
        let t = track("1", 1, "Northbound", None);
        let mut app = app_with_album(vec![t.clone()]);
        app.album_art = true;
        let buf = render_sized(&mut app, &playing_status(&t), 140, 40);
        let (x0, y0) = cover_origin(&buf);
        assert_eq!(buf[(x0, y0)].bg, SEL_BG);
        let rows: String = (y0..y0 + 8).map(|y| row_text(&buf, y)).collect();
        assert!(rows.contains('♪'));
    }

    #[test]
    fn compact_layout_when_art_is_off_or_the_terminal_is_small() {
        let t = track("1", 1, "Northbound", None);
        for (album_art, w, h) in [(false, 140, 40), (true, 80, 40), (true, 140, 24)] {
            let mut app = app_with_album(vec![t.clone()]);
            app.album_art = album_art;
            let buf = render_sized(&mut app, &playing_status(&t), w, h);
            // Compact: the title line carries "artist — album", and no cover cells.
            let screen: Vec<String> = (0..h).map(|y| row_text(&buf, y)).collect();
            let title = screen.iter().find(|l| l.contains("PLAYING")).unwrap();
            assert!(title.contains("Cat Power — You Are Free"), "{} {}x{}", album_art, w, h);
            assert!(!screen.iter().any(|l| l.contains('▀') || l.contains('♪')));
        }
    }

    #[test]
    fn at_80x24_headings_and_details_fit_without_overlapping() {
        use crate::decode::SourceInfo;
        use crate::output::{OutputInfo, Route, SampleFmt};
        let tracks: Vec<Track> = (1..=5).map(|i| track(&i.to_string(), i, &format!("Song {}", i), None)).collect();
        let mut app = app_with_album(tracks.clone());
        if let Some((_, a)) = app.shown_album.as_mut() {
            a.kind = ReleaseKind::Ep;
        }
        let mut st = playing_status(&tracks[2]);
        st.source = Some(SourceInfo {
            codec: "FLAC".into(),
            lossless: true,
            rate: 44_100,
            channels: 2,
            bits: 16,
            duration_secs: Some(185.0),
        });
        st.output = Some(OutputInfo { label: "Schiit Modi+".into(), rate: 44_100, fmt: SampleFmt::S16, route: Route::Direct });
        let buf = render_sized(&mut app, &st, 80, 24);
        let screen: Vec<String> = (0..24).map(|y| row_text(&buf, y)).collect();

        // The track list heading keeps the album, artist, year and type whole;
        // the summary on the right is either whole or left out.
        let heading = screen.iter().find(|l| l.contains("You Are Free")).unwrap();
        assert!(heading.contains("You Are Free · Cat Power · 2003 · EP"), "{}", heading);
        if heading.contains("tracks") {
            assert!(heading.contains("5 tracks · 15:25"), "{}", heading);
        }

        // The compact details line: a real gap before "out:", nothing cut off.
        let details = screen.iter().find(|l| l.contains("FLAC ·")).unwrap();
        assert!(details.contains("  out: Schiit Modi+"), "{}", details);
        assert!(!details.contains('…'), "{}", details);
        assert!(inside(details).ends_with("exclusive") || inside(details).ends_with("vol: DAC") || inside(details).ends_with("Modi+"), "{}", details);
    }

    #[test]
    fn release_type_shows_for_albums_too_but_not_when_unknown() {
        let t = track("1", 1, "One", None);
        for (kind, want) in [(ReleaseKind::Album, Some("Album")), (ReleaseKind::Ep, Some("EP")), (ReleaseKind::Other, None)] {
            let mut app = app_with_album(vec![t.clone()]);
            app.album_art = true;
            if let Some((_, a)) = app.shown_album.as_mut() {
                a.kind = kind;
            }
            let buf = render_sized(&mut app, &playing_status(&t), 140, 40);
            let screen: Vec<String> = (0..40).map(|y| row_text(&buf, y)).collect();
            let heading = screen.iter().find(|l| l.contains("You Are Free · Cat Power")).unwrap();
            let album_line = &tall_rows(&mut app, &playing_status(&t), 140)[2];
            match want {
                Some(label) => {
                    assert!(heading.contains(&format!("· 2003 · {} ", label)), "{}", heading);
                    assert!(album_line.contains(&format!("You Are Free · 2003 · {}", label)), "{}", album_line);
                }
                None => {
                    assert!(heading.contains("· 2003 ") && !heading.contains("Other"), "{}", heading);
                    assert!(inside(album_line).ends_with("You Are Free · 2003"), "{}", album_line);
                }
            }
        }
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
