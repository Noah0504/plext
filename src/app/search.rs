//! The Search view: type a few words, get matching artists, albums and
//! tracks from the library (Plex's own search), and play or queue them.
//! An album or track shows its album's tracks on the right; an artist
//! shows their releases.

use super::*;
use crate::plex::SearchResults;

/// Rows in the results list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SRow {
    /// A section heading and how many it lists.
    Heading(&'static str, usize),
    Artist(usize),
    Album(usize),
    Track(usize),
}

/// The Search view's state.
pub(super) struct Search {
    pub(super) query: String,
    /// Keys go into the query.
    pub(super) typing: bool,
    /// The query the results are for.
    pub(super) searched: String,
    pub(super) results: Option<SearchResults>,
    pub(super) searching: bool,
    pub(super) error: Option<String>,
    pub(super) state: ListState,
    /// The album shown on the right: (artist, album).
    pub(super) album: Option<(String, Album)>,
    /// A track to highlight once its album's tracks have loaded.
    select_track: Option<String>,
    /// (album, track): play the album from this track once its tracks load.
    play_when_loaded: Option<(String, String)>,
    gen: u64,
    edited_at: Option<Instant>,
}

impl Default for Search {
    fn default() -> Search {
        Search {
            query: String::new(),
            typing: false,
            searched: String::new(),
            results: None,
            searching: false,
            error: None,
            state: ListState::default().with_selected(Some(0)),
            album: None,
            select_track: None,
            play_when_loaded: None,
            gen: 0,
            edited_at: None,
        }
    }
}

impl Search {
    /// The album couldn't load: don't play it when it does.
    pub(super) fn forget_play(&mut self, album: &str) {
        if self.play_when_loaded.as_ref().map(|(a, _)| a == album).unwrap_or(false) {
            self.play_when_loaded = None;
        }
    }
}

/// A search answer from the background.
pub(super) struct Found {
    gen: u64,
    result: Result<SearchResults, String>,
}

/// Search this long after the last key while typing.
const TYPING_PAUSE: Duration = Duration::from_millis(300);
/// Shorter queries wait for Enter (one letter matches half the library).
const MIN_CHARS: usize = 2;

impl App {
    /// `f`: the Search view, ready to type.
    pub(super) fn open_search(&mut self) {
        if self.view != LeftView::Search {
            self.enter_view(LeftView::Search);
        }
        self.focus = Focus::Tree;
        self.search.typing = true;
    }

    // ---------- searching ----------

    fn run_search(&mut self) {
        let query = self.search.query.trim().to_string();
        self.search.edited_at = None;
        if query.is_empty() {
            self.search.results = None;
            self.search.searched.clear();
            self.search.album = None;
            return;
        }
        self.search.gen += 1;
        self.search.searching = true;
        self.search.searched = query.clone();
        let (s, lib, tx, gen) = (self.server.clone(), self.library_key.clone(), self.tx.clone(), self.search.gen);
        thread::spawn(move || {
            let result = s.search(&lib, &query, SEARCH_LIMIT).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Found(Found { gen, result }));
        });
    }

    /// Searches once you stop typing for a moment.
    pub(super) fn search_tick(&mut self) {
        let Some(at) = self.search.edited_at else { return };
        if at.elapsed() < TYPING_PAUSE {
            return;
        }
        let q = self.search.query.trim();
        if q == self.search.searched {
            self.search.edited_at = None;
        } else if q.is_empty() || q.chars().count() >= MIN_CHARS {
            self.run_search();
        }
    }

    pub(super) fn on_found(&mut self, f: Found) {
        if f.gen != self.search.gen {
            return; // an older search, overtaken by a newer one
        }
        self.search.searching = false;
        match f.result {
            Ok(r) => {
                self.search.error = None;
                self.search.results = Some(r);
                let rows = self.search_rows();
                self.search.state.select(Some(first_result(&rows, 0, 1)));
                self.search.album = None;
                self.sync_search();
            }
            Err(e) => self.search.error = Some(e),
        }
    }

    // ---------- rows ----------

    pub(super) fn search_rows(&self) -> Vec<SRow> {
        let Some(r) = &self.search.results else { return Vec::new() };
        let mut rows = Vec::new();
        if !r.artists.is_empty() {
            rows.push(SRow::Heading("Artists", r.artists.len()));
            rows.extend((0..r.artists.len()).map(SRow::Artist));
        }
        if !r.albums.is_empty() {
            rows.push(SRow::Heading("Albums", r.albums.len()));
            rows.extend((0..r.albums.len()).map(SRow::Album));
        }
        if !r.tracks.is_empty() {
            rows.push(SRow::Heading("Tracks", r.tracks.len()));
            rows.extend((0..r.tracks.len()).map(SRow::Track));
        }
        rows
    }

    pub(super) fn search_row(&self) -> Option<SRow> {
        self.search_rows().get(self.search.state.selected()?).copied()
    }

    /// The track on the highlighted row, if it's a track.
    pub(super) fn search_track(&self) -> Option<Track> {
        match self.search_row()? {
            SRow::Track(i) => self.search.results.as_ref()?.tracks.get(i).cloned(),
            _ => None,
        }
    }

    /// The artist on the highlighted row, if it's an artist: (key, name).
    pub(super) fn search_artist(&self) -> Option<(String, String)> {
        match self.search_row()? {
            SRow::Artist(i) => self.search.results.as_ref()?.artists.get(i).map(|a| (a.key.clone(), a.title.clone())),
            _ => None,
        }
    }

    /// Shows the highlighted album (or the highlighted track's album) on the
    /// right, or loads the highlighted artist's releases.
    pub(super) fn sync_search(&mut self) {
        let Some(r) = self.search.results.clone() else { return };
        let (artist, album, track) = match self.search_row() {
            Some(SRow::Album(i)) => {
                let a = &r.albums[i];
                (a.artist.clone(), a.album.clone(), None)
            }
            Some(SRow::Track(i)) => {
                let t = &r.tracks[i];
                let Some(key) = t.album_key.clone() else {
                    self.search.album = None;
                    return;
                };
                let album = Album { key, title: t.album.clone(), year: None, kind: ReleaseKind::Other };
                (t.artist.clone(), album, Some(t.key.clone()))
            }
            Some(SRow::Artist(i)) => {
                self.search.album = None;
                let key = r.artists[i].key.clone();
                self.load_albums(&key);
                return;
            }
            _ => {
                self.search.album = None;
                return;
            }
        };
        let changed = self.search.album.as_ref().map(|(_, a)| a.key != album.key).unwrap_or(true);
        if changed {
            self.refresh_tracks(&album.key);
            self.search.album = Some((artist, album));
        }
        self.track_state.select(Some(0));
        self.search.select_track = track;
        self.search_tracks_loaded();
    }

    /// Once the album on the right has its tracks: highlight the track you
    /// found, and play it if Enter was pressed before they arrived.
    pub(super) fn search_tracks_loaded(&mut self) {
        if let Some((album, track)) = self.search.play_when_loaded.clone() {
            if let Some(Loadable::Ready(tracks)) = self.tracks.get(&album) {
                self.search.play_when_loaded = None;
                match tracks.iter().position(|t| t.key == track) {
                    Some(i) => self.play_album(&album, i),
                    None => self.say("That track isn't on its album any more".into()),
                }
            }
        }
        let Some((_, al)) = self.search.album.as_ref() else { return };
        let Some(Loadable::Ready(tracks)) = self.tracks.get(&al.key) else { return };
        if let Some(i) = self.search.select_track.as_deref().and_then(|k| tracks.iter().position(|t| t.key == k)) {
            self.track_state.select(Some(i));
            self.search.select_track = None;
        }
    }

    // ---------- keys ----------

    /// Keys while typing the query.
    pub(super) fn search_type_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => {
                self.search.typing = false;
                if self.search.query.trim() != self.search.searched || self.search.results.is_none() {
                    self.run_search();
                }
            }
            KeyCode::Esc => {
                self.search.typing = false;
                // Put back what the results are for.
                if self.search.results.is_some() {
                    self.search.query = self.search.searched.clone();
                    self.search.edited_at = None;
                }
            }
            KeyCode::Down | KeyCode::Up | KeyCode::PageDown | KeyCode::PageUp => {
                self.search.typing = false;
                self.search_list_key(k.code);
            }
            KeyCode::Backspace => {
                self.search.query.pop();
                self.search.edited_at = Some(Instant::now());
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.query.clear();
                self.search.edited_at = Some(Instant::now());
            }
            KeyCode::Char(c) => {
                self.search.query.push(c);
                self.search.edited_at = Some(Instant::now());
            }
            _ => {}
        }
    }

    fn move_search(&mut self, delta: i64) {
        let rows = self.search_rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.search.state.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, rows.len() as i64 - 1) as usize;
        self.search.state.select(Some(first_result(&rows, next, if delta < 0 { -1 } else { 1 })));
        self.sync_search();
    }

    /// Keys in the results list.
    pub(super) fn search_list_key(&mut self, code: KeyCode) {
        let Some(r) = self.search.results.clone() else {
            if matches!(code, KeyCode::Enter | KeyCode::Char('/') | KeyCode::Right | KeyCode::Char('l')) {
                self.search.typing = true;
            }
            return;
        };
        let row = self.search_row();
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_search(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_search(1),
            KeyCode::PageUp => self.move_search(-(self.tree_page.max(1) as i64)),
            KeyCode::PageDown => self.move_search(self.tree_page.max(1) as i64),
            KeyCode::Home | KeyCode::Char('g') => self.move_search(-1_000_000),
            KeyCode::End | KeyCode::Char('G') => self.move_search(1_000_000),
            KeyCode::Char('/') => self.search.typing = true,
            KeyCode::Right | KeyCode::Char('l') => match row {
                Some(SRow::Artist(i)) => self.open_artist(&r.artists[i].key),
                Some(SRow::Album(_)) | Some(SRow::Track(_)) if self.search.album.is_some() => {
                    self.right = RightView::Album;
                    self.focus = Focus::Tracks;
                }
                _ => {}
            },
            KeyCode::Enter => match row {
                Some(SRow::Artist(i)) => self.open_artist(&r.artists[i].key),
                Some(SRow::Album(i)) => self.album_key_action(&r.albums[i].album.key, Action::Play(0)),
                Some(SRow::Track(i)) => self.play_found_track(r.tracks[i].clone()),
                _ => {}
            },
            KeyCode::Char(c @ ('a' | 'A' | 'P')) => {
                let action = match c {
                    'a' => Action::Append,
                    'A' => Action::PlayNext,
                    _ => Action::Pick,
                };
                match row {
                    Some(SRow::Album(i)) => self.album_key_action(&r.albums[i].album.key, action),
                    Some(SRow::Track(i)) => {
                        let t = r.tracks[i].clone();
                        let title = t.title.clone();
                        if action == Action::Pick {
                            self.open_picker(vec![t], title);
                        } else {
                            self.queue_tracks(vec![t], action, &title);
                        }
                    }
                    Some(SRow::Artist(i)) => self.say(format!(
                        "Pick one of {}'s albums (→ opens the artist), then press {} again",
                        r.artists[i].title, c
                    )),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Enter on a track: play its album from that track, like Enter in an
    /// album's track list.
    fn play_found_track(&mut self, t: Track) {
        let Some(album) = t.album_key.clone() else {
            self.new_queue();
            self.player.send(Cmd::Play { queue: vec![t], index: 0 });
            return;
        };
        if let Some(Loadable::Ready(tracks)) = self.tracks.get(&album) {
            if let Some(i) = tracks.iter().position(|x| x.key == t.key) {
                return self.play_album(&album, i);
            }
            // Not on the album as listed: just the track.
            self.new_queue();
            self.player.send(Cmd::Play { queue: vec![t], index: 0 });
            return;
        }
        self.load_tracks(&album);
        self.when_loaded.clear();
        self.search.play_when_loaded = Some((album, t.key));
    }

    // ---------- drawing ----------

    pub(super) fn draw_search_list(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Tree;
        let status = if self.search.searching {
            "searching… ".to_string()
        } else if let Some(r) = &self.search.results {
            let n = r.artists.len() + r.albums.len() + r.tracks.len();
            format!("{} found ", n)
        } else {
            String::new()
        };
        let block = self.pane(self.view_switcher_for(focused, area.width), String::new(), focused);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        self.tree_page = parts[2].height.saturating_sub(1) as usize;
        let inner_w = inner.width as usize;

        // The query line.
        let s = &self.search;
        let room = inner_w.saturating_sub(4 + status.chars().count());
        let query: String = {
            // Keep the end of a long query in view while typing.
            let n = s.query.chars().count();
            s.query.chars().skip(n.saturating_sub(room)).collect()
        };
        let line = if s.typing {
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(ACCENT)),
                Span::styled(query, Style::default().fg(BRIGHT)),
                Span::styled("▏", Style::default().fg(ACCENT)),
            ])
        } else if s.query.is_empty() {
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(FAINT)),
                Span::styled("f or enter to search", Style::default().fg(FAINT)),
            ])
        } else {
            // The hint only when the whole query fits beside it.
            let hint = if query.chars().count() + 9 <= room { "  f edits" } else { "" };
            let q = truncate(&query, room.saturating_sub(hint.len()));
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(DIM)),
                Span::styled(q, Style::default().fg(FG)),
                Span::styled(hint, Style::default().fg(FAINT)),
            ])
        };
        f.render_widget(Paragraph::new(line), parts[0]);
        f.render_widget(
            Paragraph::new(Span::styled(status, Style::default().fg(FAINT))).alignment(Alignment::Right),
            parts[0],
        );

        let message = |text: String, color: Color| List::new(vec![ListItem::new(Span::styled(text, Style::default().fg(color)))]);
        if let Some(e) = &self.search.error {
            f.render_widget(message(format!(" Search failed: {}", e), WARN), parts[2]);
            return;
        }
        let Some(r) = &self.search.results else {
            let text = if self.search.searching {
                " Searching…"
            } else {
                " Artists, albums and tracks: type a few words."
            };
            f.render_widget(message(text.into(), DIM), parts[2]);
            return;
        };
        if r.is_empty() {
            f.render_widget(message(format!(" Nothing matches \"{}\".", self.search.searched), DIM), parts[2]);
            return;
        }
        let rows = self.search_rows();
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let (name, detail) = match *row {
                    SRow::Heading(t, n) => {
                        return ListItem::new(Line::from(vec![
                            Span::styled(format!(" {}", t), Style::default().fg(FAINT).add_modifier(Modifier::BOLD)),
                            Span::styled(format!("  {}", n), Style::default().fg(FAINT)),
                        ]))
                    }
                    SRow::Artist(i) => (r.artists[i].title.clone(), String::new()),
                    SRow::Album(i) => {
                        let a = &r.albums[i];
                        let mut d = vec![a.artist.clone()];
                        if let Some(y) = a.album.year {
                            d.push(y.to_string());
                        }
                        (a.album.title.clone(), d.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · "))
                    }
                    SRow::Track(i) => {
                        let t = &r.tracks[i];
                        let d: Vec<String> = [t.artist.clone(), t.album.clone()].into_iter().filter(|x| !x.is_empty()).collect();
                        (t.title.clone(), d.join(" · "))
                    }
                };
                let room = inner_w.saturating_sub(4);
                let name = truncate(&name, room);
                let detail = if detail.is_empty() { String::new() } else { format!("  {}", detail) };
                let rest = truncate(&detail, room.saturating_sub(name.chars().count()));
                ListItem::new(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(name, Style::default().fg(FG)),
                    Span::styled(rest, Style::default().fg(DIM)),
                ]))
            })
            .collect();
        let list = List::new(items).scroll_padding(3).highlight_style(
            Style::default()
                .bg(if focused && !self.search.typing { SEL_BG } else { Color::Reset })
                .fg(BRIGHT)
                .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
        );
        f.render_stateful_widget(list, parts[2], &mut self.search.state);
    }

    pub(super) fn draw_search_right(&mut self, f: &mut Frame, area: Rect, st: &Status) {
        let Some((key, name)) = self.search_artist() else {
            return self.draw_tracks(f, area, st);
        };
        // An artist: their releases, oldest first.
        let block = self
            .pane(Line::from(Span::styled(format!(" {} ", name), title_style(false))), String::new(), false)
            .padding(ratatui::widgets::Padding::horizontal(1));
        let mut lines = vec![Line::from("")];
        match self.albums.get(&key) {
            Some(Loadable::Ready(albums)) => {
                let mut order: Vec<&Album> = albums.iter().collect();
                order.sort_by_key(|a| (a.year.unwrap_or(u32::MAX), a.title.to_lowercase()));
                for a in order {
                    let mut detail = Vec::new();
                    if let Some(y) = a.year {
                        detail.push(y.to_string());
                    }
                    if let Some(k) = a.kind.label() {
                        detail.push(k.to_string());
                    }
                    lines.push(Line::from(vec![
                        Span::styled(a.title.clone(), Style::default().fg(FG)),
                        Span::styled(
                            if detail.is_empty() { String::new() } else { format!("  {}", detail.join(" · ")) },
                            Style::default().fg(DIM),
                        ),
                    ]));
                }
            }
            Some(Loadable::Failed(e)) => lines.push(Line::from(Span::styled(format!("Couldn't load releases: {}", e), Style::default().fg(WARN)))),
            _ => lines.push(Line::from(Span::styled("Loading releases…", Style::default().fg(DIM)))),
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("enter  open in Artists    m  artist mix", Style::default().fg(DIM))));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

/// The nearest row at or after `i` (going `step`) that isn't a heading.
fn first_result(rows: &[SRow], i: usize, step: i64) -> usize {
    let n = rows.len() as i64;
    let ok = |at: i64| at >= 0 && at < n && !matches!(rows[at as usize], SRow::Heading(..));
    let mut at = i as i64;
    while at >= 0 && at < n && !ok(at) {
        at += step;
    }
    if !ok(at) {
        at = i as i64;
        while at >= 0 && at < n && !ok(at) {
            at -= step;
        }
    }
    at.clamp(0, (n - 1).max(0)) as usize
}

#[cfg(test)]
impl Search {
    pub(super) fn play_when_loaded_is_empty(&self) -> bool {
        self.play_when_loaded.is_none()
    }
}

#[cfg(test)]
impl Found {
    pub(super) fn stale_for_test(gen: u64) -> Found {
        Found { gen, result: Ok(SearchResults::default()) }
    }
}

#[cfg(test)]
impl App {
    pub(super) fn search_gen(&self) -> u64 {
        self.search.gen
    }
}
