//! The Discover view (stations, mixes, and the shelves the library offers),
//! and mixes that keep going: stations, track radio and artist mixes top the
//! queue up as it runs low, until something else is played.

use super::*;
use crate::plex::{Hub, HubItem};

/// Rows in the Discover list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DRow {
    Heading(&'static str),
    /// hubs[hub].items[item]
    Station(usize, usize),
    Mix(MixKind),
    /// hubs[hub]
    Shelf(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MixKind {
    TrackRadio,
    ArtistMix,
    Adventure,
}

/// Where a never-ending mix gets its next tracks.
#[derive(Clone, Debug)]
pub(super) enum Source {
    /// A Plex station, by path.
    Station(String),
    /// Tracks that sound like the last one added.
    Track { anchor: String },
    /// An artist and the artists that sound like them, a few at a time.
    Artist { key: String, similar: Vec<(String, String)>, next: usize },
}

/// A mix that tops the queue up until something else is played.
pub(super) struct Radio {
    pub(super) title: String,
    source: Source,
    /// Tracks already queued, so they aren't added twice.
    seen: HashSet<String>,
    fetching: bool,
    /// Top-ups in a row that found nothing new.
    dry: u32,
    gen: u64,
}

/// Tracks for a mix, fetched in the background.
pub(super) struct Batch {
    gen: u64,
    first: bool,
    result: Result<(Vec<Track>, Source), String>,
}

/// Add more when this few tracks are left to play.
const TOP_UP_AT: usize = 3;
/// Give up after this many top-ups in a row that find nothing new.
const MAX_DRY: u32 = 3;

impl App {
    // ---------- loading ----------

    pub(super) fn load_discover(&mut self) {
        if self.discover.is_none() {
            self.discover = Some(Loadable::Loading);
        }
        let (s, lib, tx) = (self.server.clone(), self.library_key.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Hubs(s.hubs(&lib).map_err(|e| e.to_string())));
        });
    }

    pub(super) fn on_hubs(&mut self, r: Result<Vec<Hub>, String>) {
        match r {
            Ok(h) => {
                // Shelves change (their contents and which there are), so
                // their full lists are fetched again.
                self.shelves.clear();
                self.discover = Some(Loadable::Ready(h));
                let rows = self.discover_rows();
                let sel = self.discover_state.selected().unwrap_or(0).min(rows.len().saturating_sub(1));
                self.discover_state.select(Some(first_selectable(&rows, sel, 1)));
                self.sync_shelf();
            }
            Err(e) => match self.discover {
                Some(Loadable::Ready(_)) => self.warn(format!("Couldn't refresh Discover: {}", e)),
                _ => self.discover = Some(Loadable::Failed(e)),
            },
        }
    }

    fn hubs_ref(&self) -> &[Hub] {
        match &self.discover {
            Some(Loadable::Ready(h)) => h,
            _ => &[],
        }
    }

    pub(super) fn discover_rows(&self) -> Vec<DRow> {
        let hubs = self.hubs_ref();
        let mut rows = Vec::new();
        let stations: Vec<(usize, usize)> = hubs
            .iter()
            .enumerate()
            .filter(|(_, h)| h.is_stations())
            .flat_map(|(i, h)| (0..h.items.len()).map(move |j| (i, j)))
            .collect();
        if !stations.is_empty() {
            rows.push(DRow::Heading("Stations"));
            rows.extend(stations.into_iter().map(|(i, j)| DRow::Station(i, j)));
        }
        rows.push(DRow::Heading("Mixes"));
        rows.extend([MixKind::TrackRadio, MixKind::ArtistMix, MixKind::Adventure].map(DRow::Mix));
        let shelves: Vec<usize> = hubs
            .iter()
            .enumerate()
            .filter(|(_, h)| !h.is_stations() && (!h.items.is_empty()) && h.kind != "clip")
            .map(|(i, _)| i)
            .collect();
        if !shelves.is_empty() {
            rows.push(DRow::Heading("Your library"));
            rows.extend(shelves.into_iter().map(DRow::Shelf));
        }
        rows
    }

    pub(super) fn discover_row(&self) -> Option<DRow> {
        self.discover_rows().get(self.discover_state.selected()?).copied()
    }

    /// The shelf open on the right, if any: its hub.
    fn shown_shelf(&self) -> Option<&Hub> {
        match self.discover_row()? {
            DRow::Shelf(i) => self.hubs_ref().get(i),
            _ => None,
        }
    }

    /// What the open shelf lists: the full list once fetched, else the hub's first items.
    pub(super) fn shelf_items(&self) -> Vec<HubItem> {
        let Some(h) = self.shown_shelf() else { return Vec::new() };
        match self.shelves.get(&h.id) {
            Some(Loadable::Ready(all)) => all.clone(),
            _ => h.items.clone(),
        }
    }

    /// Fetches the full list behind the open shelf.
    fn sync_shelf(&mut self) {
        let Some(h) = self.shown_shelf().cloned() else { return };
        self.shelf_state.select(Some(0));
        let Some(path) = h.more.clone() else { return };
        if self.shelves.contains_key(&h.id) {
            return;
        }
        self.shelves.insert(h.id.clone(), Loadable::Loading);
        let (s, tx) = (self.server.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Shelf(h.id, s.hub_items(&path).map_err(|e| e.to_string())));
        });
    }

    pub(super) fn on_shelf(&mut self, id: String, r: Result<Vec<HubItem>, String>) {
        match r {
            Ok(items) => {
                self.shelves.insert(id, Loadable::Ready(items));
            }
            // Keep showing the hub's first items.
            Err(_) => {
                self.shelves.remove(&id);
            }
        }
    }

    // ---------- keys ----------

    fn move_discover(&mut self, delta: i64) {
        let rows = self.discover_rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.discover_state.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, rows.len() as i64 - 1) as usize;
        self.discover_state.select(Some(first_selectable(&rows, next, if delta < 0 { -1 } else { 1 })));
        self.sync_shelf();
    }

    pub(super) fn discover_list_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_discover(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_discover(1),
            KeyCode::PageUp => self.move_discover(-(self.tree_page.max(1) as i64)),
            KeyCode::PageDown => self.move_discover(self.tree_page.max(1) as i64),
            KeyCode::Home | KeyCode::Char('g') => self.move_discover(-1_000_000),
            KeyCode::End | KeyCode::Char('G') => self.move_discover(1_000_000),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.discover_row() {
                Some(DRow::Station(i, j)) if code == KeyCode::Enter => {
                    if let Some(st) = self.hubs_ref().get(i).and_then(|h| h.items.get(j)).cloned() {
                        self.start_radio(st.title.clone(), Source::Station(st.path));
                    }
                }
                Some(DRow::Mix(kind)) if code == KeyCode::Enter => self.start_mix_from_playing(kind),
                Some(DRow::Shelf(_)) => {
                    self.sync_shelf();
                    self.right = RightView::Album;
                    self.focus = Focus::Tracks;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Keys in the right pane while it shows a shelf.
    pub(super) fn shelf_key(&mut self, code: KeyCode) {
        let items = self.shelf_items();
        let n = items.len();
        let cur = self.shelf_state.selected().unwrap_or(0).min(n.saturating_sub(1));
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.shelf_state.select(Some(cur.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => self.shelf_state.select(Some((cur + 1).min(n.saturating_sub(1)))),
            KeyCode::PageUp => self.shelf_state.select(Some(cur.saturating_sub(10))),
            KeyCode::PageDown => self.shelf_state.select(Some((cur + 10).min(n.saturating_sub(1)))),
            KeyCode::Home | KeyCode::Char('g') => self.shelf_state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => self.shelf_state.select(Some(n.saturating_sub(1))),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Tree,
            KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Char('P') => {
                let Some(item) = items.get(cur).cloned() else { return };
                let action = match code {
                    KeyCode::Enter => Action::Play(0),
                    KeyCode::Char('a') => Action::Append,
                    KeyCode::Char('A') => Action::PlayNext,
                    _ => Action::Pick,
                };
                match item.kind.as_str() {
                    "album" => self.album_key_action(&item.key, action),
                    "artist" if code == KeyCode::Enter => self.open_artist(&item.key),
                    "artist" => self.say(format!("Enter opens {}; m starts a mix of them", item.title)),
                    _ => self.say(format!("Plext can't play {} from here yet", item.title)),
                }
            }
            _ => {}
        }
    }

    /// Plays or queues an album by key, loading its tracks first if need be.
    pub(super) fn album_key_action(&mut self, key: &str, action: Action) {
        if matches!(self.tracks.get(key), Some(Loadable::Ready(_))) {
            self.album_action(key, action);
        } else {
            self.load_tracks(key);
            if matches!(action, Action::Play(_)) {
                self.when_loaded.clear();
            }
            self.when_loaded.push((key.to_string(), action));
        }
    }

    /// Shows an artist in the Artists view, opened.
    pub(super) fn open_artist(&mut self, key: &str) {
        let Some(i) = (match &self.artists {
            Loadable::Ready(a) => a.iter().position(|x| x.key == key),
            _ => None,
        }) else {
            self.say("That artist isn't in the artist list".into());
            return;
        };
        self.view = LeftView::Artists;
        self.filter.clear();
        self.focus = Focus::Tree;
        self.set_expanded(i, true);
        let pos = self.rows().iter().position(|r| *r == Row::Artist(i));
        self.tree_state.select(pos);
    }

    // ---------- starting mixes ----------

    /// `m`: a mix from whatever's under the cursor (a track: track radio;
    /// an artist: an artist mix), or from what's playing.
    pub(super) fn mix_from_here(&mut self) {
        // A track in the right pane or the queue.
        let track = match (self.focus, self.right, self.view) {
            (Focus::Tracks, RightView::Queue, _) => {
                self.queue_state.selected().and_then(|i| self.queue.items.get(i)).map(|e| e.track.clone())
            }
            (Focus::Tracks, RightView::Album, LeftView::Artists | LeftView::Playlists)
            | (Focus::Tracks, RightView::Album, LeftView::Search) => {
                self.track_state.selected().and_then(|i| self.shown_tracks()?.get(i).cloned())
            }
            (Focus::Tree, _, LeftView::Search) => self.search_track(),
            _ => None,
        };
        if let Some(t) = track {
            return self.start_track_radio(t);
        }
        // An artist in the artist list or a shelf.
        let artist = match (self.focus, self.view) {
            (Focus::Tree, LeftView::Artists) => match self.current_row() {
                Some(Row::Artist(i)) | Some(Row::Album(i, _)) | Some(Row::Heading(i, _)) => {
                    self.artist(i).map(|a| (a.key.clone(), a.title.clone()))
                }
                None => None,
            },
            (Focus::Tracks, LeftView::Discover) => self
                .shelf_state
                .selected()
                .and_then(|i| self.shelf_items().get(i).cloned())
                .filter(|it| it.kind == "artist")
                .map(|it| (it.key, it.title)),
            (Focus::Tree, LeftView::Search) => self.search_artist(),
            _ => None,
        };
        if let Some((key, name)) = artist {
            return self.start_artist_mix(key, name);
        }
        self.start_mix_from_playing(MixKind::TrackRadio);
    }

    /// A mix from the playing track (the Mixes rows in Discover).
    fn start_mix_from_playing(&mut self, kind: MixKind) {
        let st = self.player.status();
        let Some(t) = st.track.clone().filter(|_| st.state != PlayState::Stopped) else {
            self.say("Play something first, or press m on a track or artist to start a mix".into());
            return;
        };
        match kind {
            MixKind::TrackRadio => self.start_track_radio(t),
            MixKind::ArtistMix => {
                // The track listing doesn't carry its artist's key; ask.
                let (s, tx, gen) = (self.server.clone(), self.tx.clone(), self.next_radio_gen());
                self.radio = Some(Radio {
                    title: format!("{} mix", t.artist),
                    source: Source::Artist { key: String::new(), similar: Vec::new(), next: 0 },
                    seen: HashSet::new(),
                    fetching: true,
                    dry: 0,
                    gen,
                });
                self.say(format!("Starting a {} mix…", t.artist));
                thread::spawn(move || {
                    let r = s
                        .track_artist(&t.key)
                        .map_err(|e| e.to_string())
                        .and_then(|(key, _)| artist_batch(&s, &key, None, 0));
                    let _ = tx.send(Msg::Radio(Batch { gen, first: true, result: r }));
                });
            }
            MixKind::Adventure => self.say(
                "Highlight a track (in an album, a playlist or the queue) and press M to travel there from what's playing"
                    .into(),
            ),
        }
    }

    fn next_radio_gen(&mut self) -> u64 {
        self.next_load += 1;
        self.next_load
    }

    /// Track radio: the track, then tracks that sound like it, for as long as you like.
    pub(super) fn start_track_radio(&mut self, seed: Track) {
        let title = format!("{} radio", seed.title);
        let gen = self.next_radio_gen();
        self.radio = Some(Radio {
            title: title.clone(),
            source: Source::Track { anchor: seed.key.clone() },
            seen: HashSet::new(),
            fetching: true,
            dry: 0,
            gen,
        });
        self.say(format!("Starting {}…", title));
        let (s, tx) = (self.server.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = s.similar_tracks(&seed.key, 25, 0.25).map_err(|e| e.to_string()).map(|mut v| {
                v.retain(|t| t.key != seed.key);
                let anchor = v.last().map(|t| t.key.clone()).unwrap_or_else(|| seed.key.clone());
                v.insert(0, seed);
                (v, Source::Track { anchor })
            });
            let _ = tx.send(Msg::Radio(Batch { gen, first: true, result: r }));
        });
    }

    /// An artist mix: the artist and artists that sound like them.
    pub(super) fn start_artist_mix(&mut self, key: String, name: String) {
        let gen = self.next_radio_gen();
        self.radio = Some(Radio {
            title: format!("{} mix", name),
            source: Source::Artist { key: key.clone(), similar: Vec::new(), next: 0 },
            seen: HashSet::new(),
            fetching: true,
            dry: 0,
            gen,
        });
        self.say(format!("Starting a {} mix…", name));
        let (s, tx) = (self.server.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = artist_batch(&s, &key, None, 0);
            let _ = tx.send(Msg::Radio(Batch { gen, first: true, result: r }));
        });
    }

    /// A Plex station.
    pub(super) fn start_radio(&mut self, title: String, source: Source) {
        let gen = self.next_radio_gen();
        self.radio = Some(Radio { title: title.clone(), source: source.clone(), seen: HashSet::new(), fetching: true, dry: 0, gen });
        self.say(format!("Starting {}…", title));
        let (s, tx) = (self.server.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = fetch_batch(&s, &source);
            let _ = tx.send(Msg::Radio(Batch { gen, first: true, result: r }));
        });
    }

    /// `M`: a sonic adventure from the playing track to the highlighted one.
    pub(super) fn adventure_to_here(&mut self) {
        let target = match (self.focus, self.right) {
            (Focus::Tracks, RightView::Queue) => {
                self.queue_state.selected().and_then(|i| self.queue.items.get(i)).map(|e| e.track.clone())
            }
            (Focus::Tracks, RightView::Album) if self.view != LeftView::Discover => {
                self.track_state.selected().and_then(|i| self.shown_tracks()?.get(i).cloned())
            }
            (Focus::Tree, _) if self.view == LeftView::Search => self.search_track(),
            _ => None,
        };
        let Some(target) = target else {
            self.say("Highlight a track (in an album, a playlist or the queue), then press M".into());
            return;
        };
        let st = self.player.status();
        let Some(from) = st.track.clone().filter(|_| st.state != PlayState::Stopped) else {
            self.say("Play something first: a sonic adventure travels from what's playing".into());
            return;
        };
        if from.key == target.key {
            self.say("That's the track that's playing".into());
            return;
        }
        self.say(format!("Finding a way from {} to {}…", from.title, target.title));
        let (s, lib, tx) = (self.server.clone(), self.library_key.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = s.sonic_path(&lib, &from.key, &target.key).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Adventure { from: from.key, to: target.title, result: r });
        });
    }

    pub(super) fn on_adventure(&mut self, from: String, to: String, r: Result<Vec<Track>, String>) {
        let mut path = match r {
            Ok(p) => p,
            Err(e) => return self.mix_failed(&e),
        };
        if path.first().map(|t| t.key == from).unwrap_or(false) {
            path.remove(0);
        }
        if path.is_empty() {
            return self.say(format!("Plex found no way to {}", to));
        }
        let n = path.len();
        // Keep playing what's on, and travel from there.
        self.new_queue();
        let playing = self.player.status().state != PlayState::Stopped;
        if playing {
            self.player.send(Cmd::ClearUpcoming);
            self.player.send(Cmd::Append(path));
        } else {
            self.player.send(Cmd::Play { queue: path, index: 0 });
        }
        self.say(format!("Sonic adventure to {}: {} tracks queued", to, n));
    }

    fn mix_failed(&mut self, e: &str) {
        self.warn(format!(
            "Couldn't start the mix: {} (mixes need Plex Pass and sonic analysis on the library)",
            e
        ));
    }

    pub(super) fn on_radio(&mut self, b: Batch) {
        let Some(radio) = self.radio.as_mut().filter(|r| r.gen == b.gen) else { return };
        radio.fetching = false;
        let (tracks, source) = match b.result {
            Ok(x) => x,
            Err(e) => {
                let title = radio.title.clone();
                if b.first {
                    self.radio = None;
                    return self.mix_failed(&e);
                }
                // A top-up failed: try again next tick, a few times.
                radio.dry += 1;
                if radio.dry >= MAX_DRY {
                    self.radio = None;
                    self.warn(format!("{} stopped: {}", title, e));
                }
                return;
            }
        };
        radio.source = source;
        let fresh: Vec<Track> = tracks.into_iter().filter(|t| radio.seen.insert(t.key.clone())).collect();
        if fresh.is_empty() {
            radio.dry += 1;
            if b.first || radio.dry >= MAX_DRY {
                let title = radio.title.clone();
                self.radio = None;
                self.say(format!("{} has no {}tracks", title, if b.first { "" } else { "more new " }));
            }
            return;
        }
        radio.dry = 0;
        let title = radio.title.clone();
        if b.first {
            // A new queue; keep the radio (new_queue would end it).
            self.feed = None;
            self.when_loaded.clear();
            self.playlist_when_loaded.clear();
            self.player.send(Cmd::Play { queue: fresh, index: 0 });
            self.say(format!("Playing {}: it keeps going until you play something else", title));
        } else {
            self.player.send(Cmd::Append(fresh));
        }
    }

    /// Tops a running mix up when the queue is nearly out. True when it
    /// started fetching more.
    pub(super) fn radio_tick(&mut self, st: &Status) -> bool {
        let Some(radio) = self.radio.as_mut() else { return false };
        if radio.fetching || st.queue_len == 0 {
            return false;
        }
        let left = st.queue_len.saturating_sub(st.queue_pos + 1);
        if left > TOP_UP_AT {
            return false;
        }
        radio.fetching = true;
        let (gen, source) = (radio.gen, radio.source.clone());
        let (s, tx) = (self.server.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = fetch_batch(&s, &source);
            let _ = tx.send(Msg::Radio(Batch { gen, first: false, result: r }));
        });
        true
    }

    /// Something new is about to replace the queue: stop topping it up.
    pub(super) fn new_queue(&mut self) {
        self.feed = None;
        self.radio = None;
    }

    // ---------- drawing ----------

    pub(super) fn draw_discover_list(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Tree;
        let block = self.pane(self.view_switcher_for(focused, area.width), String::new(), focused);
        self.tree_page = area.height.saturating_sub(3) as usize;
        let rows = self.discover_rows();
        let hubs = self.hubs_ref();
        let playing = self.player.status();
        let now = playing.track.as_ref().filter(|_| playing.state != PlayState::Stopped);
        let inner_w = area.width.saturating_sub(2) as usize;
        let items: Vec<ListItem> = match &self.discover {
            None | Some(Loadable::Loading) => vec![ListItem::new(Span::styled(" Loading…", Style::default().fg(th().dim)))],
            Some(Loadable::Failed(e)) => {
                vec![ListItem::new(Span::styled(format!(" Couldn't load Discover: {}", e), Style::default().fg(th().warn)))]
            }
            Some(Loadable::Ready(_)) => rows
                .iter()
                .map(|r| match *r {
                    DRow::Heading(t) => ListItem::new(Span::styled(
                        format!(" {}", t),
                        Style::default().fg(th().faint).add_modifier(Modifier::BOLD),
                    )),
                    DRow::Station(i, j) => {
                        let name = &hubs[i].items[j].title;
                        let on = self.radio.as_ref().map(|r| &r.title == name).unwrap_or(false);
                        line_item(name, on, inner_w)
                    }
                    DRow::Mix(kind) => {
                        let label = match (kind, now) {
                            (MixKind::TrackRadio, Some(t)) => format!("Track radio · {}", t.title),
                            (MixKind::TrackRadio, None) => "Track radio".into(),
                            (MixKind::ArtistMix, Some(t)) => format!("Artist mix · {}", t.artist),
                            (MixKind::ArtistMix, None) => "Artist mix".into(),
                            (MixKind::Adventure, _) => "Sonic adventure".into(),
                        };
                        ListItem::new(Line::from(vec![
                            Span::raw("   "),
                            Span::styled(truncate(&label, inner_w.saturating_sub(4)), Style::default().fg(th().fg)),
                        ]))
                    }
                    DRow::Shelf(i) => line_item(&hubs[i].title, false, inner_w),
                })
                .collect(),
        };
        let list = List::new(items).block(block).scroll_padding(3).highlight_style(
            Style::default()
                .bg(th().sel)
                .fg(th().bright)
                .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
        );
        f.render_stateful_widget(list, area, &mut self.discover_state);
    }

    pub(super) fn draw_discover_right(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Tracks;
        let row = self.discover_row();
        let hubs = self.hubs_ref();
        let st = self.player.status();
        let now = st.track.clone().filter(|_| st.state != PlayState::Stopped);
        let (title, body): (String, Vec<String>) = match row {
            Some(DRow::Station(i, j)) => (
                hubs[i].items[j].title.clone(),
                vec![
                    "A station from your Plex server: it picks tracks from your library as it goes.".into(),
                    "Plext keeps adding tracks until you play something else.".into(),
                    String::new(),
                    "enter  play it".into(),
                ],
            ),
            Some(DRow::Mix(MixKind::TrackRadio)) => (
                "Track radio".into(),
                vec![
                    match &now {
                        Some(t) => format!("Starts with {} — {}, then tracks that sound like it.", t.title, t.artist),
                        None => "Starts with a track, then tracks that sound like it.".into(),
                    },
                    "It keeps going until you play something else.".into(),
                    String::new(),
                    if now.is_some() { "enter  start it from what's playing" } else { "Play something first" }.into(),
                    "m      on any track: start one from that track".into(),
                ],
            ),
            Some(DRow::Mix(MixKind::ArtistMix)) => (
                "Artist mix".into(),
                vec![
                    match &now {
                        Some(t) => format!("{} and artists that sound like them, a few tracks each.", t.artist),
                        None => "An artist and artists that sound like them, a few tracks each.".into(),
                    },
                    "It keeps going until you play something else.".into(),
                    String::new(),
                    if now.is_some() { "enter  start it from what's playing" } else { "Play something first" }.into(),
                    "m      on any artist: start one from that artist".into(),
                ],
            ),
            Some(DRow::Mix(MixKind::Adventure)) => (
                "Sonic adventure".into(),
                vec![
                    "A path from the track that's playing to another track, through tracks".into(),
                    "whose sound lies in between.".into(),
                    String::new(),
                    "M      on any track (in an album, a playlist or the queue):".into(),
                    "       travel there from what's playing".into(),
                ],
            ),
            Some(DRow::Shelf(_)) => return self.draw_shelf(f, area),
            _ => ("Discover".into(), Vec::new()),
        };
        let block = self
            .pane(Line::from(Span::styled(format!(" {} ", title), title_style(focused))), String::new(), focused)
            .padding(ratatui::widgets::Padding::horizontal(1));
        let lines: Vec<Line> = std::iter::once(Line::from(""))
            .chain(body.into_iter().map(|l| {
                let style = if l.starts_with("enter") || l.starts_with('m') || l.starts_with('M') || l.starts_with(' ') {
                    Style::default().fg(th().dim)
                } else {
                    Style::default().fg(th().fg)
                };
                Line::from(Span::styled(l, style))
            }))
            .collect();
        f.render_widget(Paragraph::new(lines).block(block).wrap(ratatui::widgets::Wrap { trim: false }), area);
    }

    fn draw_shelf(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Tracks;
        let hub = self.shown_shelf().cloned();
        let items = self.shelf_items();
        let title = hub.as_ref().map(|h| h.title.clone()).unwrap_or_default();
        let loading = hub.as_ref().map(|h| matches!(self.shelves.get(&h.id), Some(Loadable::Loading))).unwrap_or(false);
        let right = if loading { " loading… ".to_string() } else { format!(" {} ", items.len()) };
        let block = self.pane(Line::from(Span::styled(format!(" {} ", title), title_style(focused))), right, focused);
        let inner_w = area.width.saturating_sub(2) as usize;
        let selected = self.shelf_state.selected();
        let rows: Vec<ListItem> = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let style = if selected == Some(i) && focused { Style::default().fg(th().bright) } else { Style::default().fg(th().fg) };
                let mut detail = Vec::new();
                if !it.by.is_empty() {
                    detail.push(it.by.clone());
                }
                if let Some(y) = it.year {
                    detail.push(y.to_string());
                }
                let detail = if detail.is_empty() { String::new() } else { format!("  {}", detail.join(" · ")) };
                let room = inner_w.saturating_sub(2);
                let name = truncate(&it.title, room);
                let rest = truncate(&detail, room.saturating_sub(name.chars().count()));
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {}", name), style),
                    Span::styled(rest, Style::default().fg(th().dim)),
                ]))
            })
            .collect();
        let list = List::new(rows)
            .block(block)
            .scroll_padding(2)
            .highlight_style(Style::default().bg(if focused { th().sel } else { th().bg }));
        f.render_stateful_widget(list, area, &mut self.shelf_state);
    }
}

/// A list row, marked ♪ when it's the mix playing.
fn line_item(name: &str, on: bool, width: usize) -> ListItem<'static> {
    let mark = if on { " ♪ " } else { "   " };
    ListItem::new(Line::from(vec![
        Span::styled(mark, Style::default().fg(th().accent)),
        Span::styled(truncate(name, width.saturating_sub(4)), Style::default().fg(th().fg)),
    ]))
}

/// The nearest row at or after `i` (going `step`) that isn't a heading.
fn first_selectable(rows: &[DRow], i: usize, step: i64) -> usize {
    let n = rows.len() as i64;
    let mut at = i as i64;
    while at >= 0 && at < n && matches!(rows[at as usize], DRow::Heading(_)) {
        at += step;
    }
    if at < 0 || at >= n {
        // Ran off the end: go the other way.
        at = i as i64;
        while at >= 0 && at < n && matches!(rows[at as usize], DRow::Heading(_)) {
            at -= step;
        }
    }
    at.clamp(0, (n - 1).max(0)) as usize
}

/// The next tracks from a mix's source, and where to carry on from.
fn fetch_batch(s: &Server, source: &Source) -> Result<(Vec<Track>, Source), String> {
    match source {
        Source::Station(path) => s.station_tracks(path).map(|t| (t, source.clone())).map_err(|e| e.to_string()),
        Source::Track { anchor } => {
            let v = s.similar_tracks(anchor, 25, 0.25).map_err(|e| e.to_string())?;
            // Carry on from somewhere new each time, so the mix drifts
            // instead of circling one sound.
            let next = v.get(v.len() / 2).or(v.last()).map(|t| t.key.clone()).unwrap_or_else(|| anchor.clone());
            Ok((v, Source::Track { anchor: next }))
        }
        Source::Artist { key, similar, next } => artist_batch(s, key, Some(similar.clone()), *next),
    }
}

/// Tracks for an artist mix: on the first call, the artist's own tracks
/// and three similar artists'; after that, the next similar artists'.
fn artist_batch(
    s: &Server,
    key: &str,
    similar: Option<Vec<(String, String)>>,
    next: usize,
) -> Result<(Vec<Track>, Source), String> {
    let similar = match similar.filter(|v| !v.is_empty()) {
        Some(v) => v,
        None => s.similar_artists(key, 12).map_err(|e| e.to_string())?,
    };
    let mut rng = Rng::new();
    let mut out = Vec::new();
    let first = next == 0;
    if first {
        let mut own = s.artist_tracks(key).map_err(|e| e.to_string())?;
        rng.shuffle(&mut own);
        out.extend(own.into_iter().take(6));
    }
    let take = 3;
    let mut at = next;
    for _ in 0..take.min(similar.len()) {
        let (k, _) = &similar[at % similar.len()];
        at += 1;
        if let Ok(mut t) = s.artist_tracks(k) {
            rng.shuffle(&mut t);
            out.extend(t.into_iter().take(3));
        }
    }
    // Every few rounds, a few more by the artist the mix is about.
    if !first && at % (similar.len().max(1) * 2) < take {
        if let Ok(mut own) = s.artist_tracks(key) {
            rng.shuffle(&mut own);
            out.extend(own.into_iter().take(2));
        }
    }
    rng.shuffle(&mut out);
    Ok((out, Source::Artist { key: key.to_string(), similar, next: at }))
}

/// A small random source for shuffling mixes.
struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        Rng(t | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}
