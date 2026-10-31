//! The Playlists view, and editing playlists: the add-to-playlist picker,
//! naming, renaming and deleting.
//!
//! Edits change what's on screen straight away and are saved to Plex in the
//! background, one at a time in the order made. Once the last pending edit
//! to a playlist is saved, the playlist is fetched again so the screen shows
//! exactly what the server has (and new tracks get their playlist item ids).

use super::*;
use crate::plex::{Playlist, PlaylistTracks};

/// Tracks in the first page of a playlist (shown quickly) and in each page after.
const FIRST_PAGE: usize = 200;
const PAGE: usize = 1000;

/// A playlist's tracks on their way, page by page.
pub(super) struct Load {
    gen: u64,
    loaded: usize,
    total: usize,
    /// The first load: pages are shown as they arrive. Otherwise it's a
    /// refresh, collected here and swapped in once complete, so the list on
    /// screen doesn't shrink and regrow.
    progressive: bool,
    buf: PlaylistTracks,
    /// Fetch again once this load finishes (an edit was saved meanwhile).
    reload_after: bool,
}

/// Where the rest of a still-loading playlist goes as it arrives.
pub(super) struct Feed {
    key: String,
    gen: u64,
    /// It's playing: mix the rest in when shuffled. Otherwise it was added
    /// to the queue, and the rest follows at the end.
    playing: bool,
}

/// A playlist change waiting to be saved to Plex.
pub(super) enum PlaylistJob {
    Create { title: String, keys: Vec<String> },
    Add { key: String, keys: Vec<String> },
    Remove { key: String, item: u64 },
    Move { key: String, item: u64, after: Option<u64> },
    Rename { key: String, title: String },
    Delete { key: String },
}

impl PlaylistJob {
    /// The playlist it changes (None for a new one).
    fn key(&self) -> Option<&str> {
        match self {
            PlaylistJob::Create { .. } => None,
            PlaylistJob::Add { key, .. }
            | PlaylistJob::Remove { key, .. }
            | PlaylistJob::Move { key, .. }
            | PlaylistJob::Rename { key, .. }
            | PlaylistJob::Delete { key } => Some(key),
        }
    }

    /// What failed, for the error message: "Couldn't {what}: …".
    fn what(&self) -> &'static str {
        match self {
            PlaylistJob::Create { .. } => "create the playlist",
            PlaylistJob::Add { .. } => "add to the playlist",
            PlaylistJob::Remove { .. } => "remove from the playlist",
            PlaylistJob::Move { .. } => "move the track",
            PlaylistJob::Rename { .. } => "rename the playlist",
            PlaylistJob::Delete { .. } => "delete the playlist",
        }
    }
}

/// A playlist change finished saving (or failed to).
pub(super) struct PlaylistDone {
    key: Option<String>,
    what: &'static str,
    /// The new playlist, for a create.
    result: Result<Option<Playlist>, String>,
}

/// Saves playlist changes to Plex one at a time, in the order made.
pub(super) fn spawn_playlist_worker(server: Server, tx: Sender<Msg>) -> Sender<PlaylistJob> {
    let (job_tx, job_rx) = mpsc::channel::<PlaylistJob>();
    thread::spawn(move || {
        for job in job_rx {
            let key = job.key().map(String::from);
            let what = job.what();
            let result = match job {
                PlaylistJob::Create { title, keys } => server.create_playlist(&title, &keys).map(Some),
                PlaylistJob::Add { key, keys } => server.add_to_playlist(&key, &keys).map(|_| None),
                PlaylistJob::Remove { key, item } => server.remove_from_playlist(&key, item).map(|_| None),
                PlaylistJob::Move { key, item, after } => server.move_in_playlist(&key, item, after).map(|_| None),
                PlaylistJob::Rename { key, title } => server.rename_playlist(&key, &title).map(|_| None),
                PlaylistJob::Delete { key } => server.delete_playlist(&key).map(|_| None),
            }
            .map_err(|e| e.to_string());
            if tx.send(Msg::PlaylistDone(PlaylistDone { key, what, result })).is_err() {
                break;
            }
        }
    });
    job_tx
}

/// A small window over the screen.
pub(super) enum Modal {
    /// "Add to playlist": a new playlist, or one of yours.
    Picker { tracks: Vec<Track>, what: String, sel: usize },
    /// Typing a playlist's name.
    Input { purpose: InputFor, text: String },
    /// "Delete playlist …? y / n"
    Confirm { key: String, title: String },
    /// Every shortcut (see help.rs).
    Help { scroll: u16 },
}

pub(super) enum InputFor {
    NewPlaylist { tracks: Vec<Track>, what: String },
    Rename { key: String },
}

impl App {
    // ---------- loading ----------

    /// Fetches the playlist list (again).
    pub(super) fn load_playlists(&mut self) {
        if self.playlists.is_none() {
            self.playlists = Some(Loadable::Loading);
        }
        let (s, lib, tx) = (self.server.clone(), self.library_key.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Playlists(s.playlists(&lib).map_err(|e| e.to_string())));
        });
    }

    /// Fetches a playlist's tracks a page at a time. The first time, pages
    /// show as they arrive; after that, the old list stays on screen until
    /// the whole new one is in.
    pub(super) fn load_playlist_tracks(&mut self, key: &str) {
        if let Some(l) = self.playlist_loads.get_mut(key) {
            if l.progressive {
                // Let the first load finish (it may be feeding the queue), then fetch again.
                l.reload_after = true;
                return;
            }
        }
        let progressive = !matches!(self.playlist_tracks.get(key), Some(Loadable::Ready(_)));
        if progressive {
            self.playlist_tracks.insert(key.to_string(), Loadable::Loading);
        }
        self.next_load += 1;
        let gen = self.next_load;
        self.playlist_loads.insert(
            key.to_string(),
            Load { gen, loaded: 0, total: 0, progressive, buf: PlaylistTracks::default(), reload_after: false },
        );
        self.live_loads.lock().unwrap().insert(key.to_string(), gen);
        let (s, k, tx, live) = (self.server.clone(), key.to_string(), self.tx.clone(), self.live_loads.clone());
        thread::spawn(move || {
            let mut start = 0;
            loop {
                let size = if start == 0 { FIRST_PAGE } else { PAGE };
                let (page, total, last) = match s.playlist_page(&k, start, size) {
                    Ok((pt, total)) => {
                        let n = pt.tracks.len();
                        start += n;
                        (Ok(pt), total, n == 0 || start >= total)
                    }
                    Err(e) => (Err(e.to_string()), 0, true),
                };
                let msg = Msg::PlaylistPage { key: k.clone(), gen, total, page, last };
                if tx.send(msg).is_err() || last {
                    break;
                }
                // Stop if a newer load of this playlist has started.
                if live.lock().unwrap().get(&k) != Some(&gen) {
                    break;
                }
            }
        });
    }

    /// (loaded, total) while a playlist is still arriving for the first time.
    pub(super) fn still_loading(&self, key: &str) -> Option<(usize, usize)> {
        self.playlist_loads
            .get(key)
            .filter(|l| l.progressive)
            .map(|l| (l.loaded, l.total.max(l.loaded)))
    }

    fn pending_edits(&self) -> u32 {
        self.playlist_pending.values().sum()
    }

    pub(super) fn on_playlists(&mut self, r: Result<Vec<Playlist>, String>) {
        // A list fetched while edits were still saving may be out of date;
        // another fetch follows once they're done.
        if self.pending_edits() > 0 {
            return;
        }
        match r {
            Ok(list) => {
                let selected = self.selected_playlist().map(|p| p.key.clone());
                self.playlists = Some(Loadable::Ready(list));
                // Keep the cursor on the same playlist.
                let vis = self.visible_playlists();
                let pos = selected
                    .and_then(|k| vis.iter().position(|p| p.key == k))
                    .unwrap_or_else(|| self.playlist_state.selected().unwrap_or(0).min(vis.len().saturating_sub(1)));
                self.playlist_state.select(Some(pos));
                self.sync_shown_playlist();
            }
            Err(e) => match self.playlists {
                // Keep what we had; say why it didn't update.
                Some(Loadable::Ready(_)) => self.warn(format!("Couldn't refresh playlists: {}", e)),
                _ => self.playlists = Some(Loadable::Failed(e)),
            },
        }
    }

    pub(super) fn on_playlist_page(
        &mut self,
        key: String,
        gen: u64,
        total: usize,
        page: Result<PlaylistTracks, String>,
        last: bool,
    ) {
        let Some(load) = self.playlist_loads.get_mut(&key).filter(|l| l.gen == gen) else { return };
        let progressive = load.progressive;
        // A refresh fetched while edits were saving may be out of date;
        // another starts once they're done.
        if !progressive && self.playlist_pending.get(&key).copied().unwrap_or(0) > 0 {
            self.playlist_loads.remove(&key);
            return;
        }
        let pt = match page {
            Ok(pt) => pt,
            Err(e) => {
                let reload = load.reload_after;
                self.playlist_loads.remove(&key);
                self.playlist_when_loaded.retain(|(k, _)| *k != key);
                if self.feed.as_ref().map(|f| f.key == key).unwrap_or(false) {
                    self.feed = None;
                }
                match self.playlist_tracks.get(&key) {
                    Some(Loadable::Ready(have)) if !have.tracks.is_empty() => {
                        self.warn(format!("Couldn't load all of the playlist: {}", e))
                    }
                    _ => {
                        self.playlist_tracks.insert(key.clone(), Loadable::Failed(e));
                    }
                }
                if reload {
                    self.load_playlist_tracks(&key);
                }
                return;
            }
        };
        for t in &pt.tracks {
            if self.pending_ratings.get(&t.key).copied().unwrap_or(0) == 0 {
                self.ratings.insert(t.key.clone(), t.rating);
            }
        }
        load.loaded += pt.tracks.len();
        load.total = total;
        if progressive {
            // The rest of a playlist that's already playing or queued.
            if let Some(f) = self.feed.as_ref().filter(|f| f.key == key && f.gen == gen) {
                if !pt.tracks.is_empty() {
                    let tracks = pt.tracks.clone();
                    self.player.send(if f.playing { Cmd::AppendMixed(tracks) } else { Cmd::Append(tracks) });
                }
            }
            match self.playlist_tracks.get_mut(&key) {
                Some(Loadable::Ready(have)) => {
                    have.tracks.extend(pt.tracks);
                    have.item_ids.extend(pt.item_ids);
                }
                _ => {
                    self.playlist_tracks.insert(key.clone(), Loadable::Ready(pt));
                }
            }
        } else {
            load.buf.tracks.extend(pt.tracks);
            load.buf.item_ids.extend(pt.item_ids);
        }
        if !last {
            return;
        }
        // All in.
        let load = self.playlist_loads.remove(&key).unwrap();
        self.playlist_fetched.insert(key.clone(), Instant::now());
        if !progressive {
            self.playlist_tracks.insert(key.clone(), Loadable::Ready(load.buf));
        }
        if self.feed.as_ref().map(|f| f.key == key).unwrap_or(false) {
            self.feed = None;
        }
        if self.shown_playlist.as_deref() == Some(key.as_str()) {
            let n = self.shown_playlist_tracks().map(|pt| pt.tracks.len()).unwrap_or(0);
            if let Some(sel) = self.track_state.selected() {
                self.track_state.select(Some(sel.min(n.saturating_sub(1))));
            }
        }
        let (now, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.playlist_when_loaded).into_iter().partition(|(k, _)| *k == key);
        self.playlist_when_loaded = later;
        for (_, action) in now {
            self.playlist_action(&key, action);
        }
        if load.reload_after {
            self.load_playlist_tracks(&key);
        }
    }

    pub(super) fn on_playlist_done(&mut self, d: PlaylistDone) {
        let pkey = d.key.clone().unwrap_or_default();
        if let Some(n) = self.playlist_pending.get_mut(&pkey) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.playlist_pending.remove(&pkey);
            }
        }
        match d.result {
            Err(e) => {
                crate::log::write(&format!("error: couldn't {}: {}", d.what, e));
                self.warn(format!("Couldn't {}: {}", d.what, e));
            }
            Ok(Some(created)) => {
                self.say(format!(
                    "Created playlist {} ({} track{})",
                    created.title,
                    created.count,
                    if created.count == 1 { "" } else { "s" }
                ));
            }
            Ok(None) => {}
        }
        // Everything saved (or something failed): show what the server has.
        if self.playlist_pending.get(&pkey).copied().unwrap_or(0) == 0 {
            if d.key.is_some() && self.playlist_tracks.contains_key(&pkey) {
                self.load_playlist_tracks(&pkey);
            }
        }
        if self.pending_edits() == 0 && self.playlists.is_some() {
            self.load_playlists();
        }
    }

    fn send_job(&mut self, job: PlaylistJob) {
        let key = job.key().unwrap_or_default().to_string();
        *self.playlist_pending.entry(key).or_insert(0) += 1;
        let _ = self.playlist_tx.send(job);
    }

    pub(super) fn warn(&mut self, text: String) {
        self.notice = Some(Notice { text, at: Instant::now(), error: true });
    }

    // ---------- the list ----------

    /// The playlists matching the filter, as shown.
    pub(super) fn visible_playlists(&self) -> Vec<&Playlist> {
        match &self.playlists {
            Some(Loadable::Ready(list)) => {
                let f = self.filter.to_lowercase();
                list.iter().filter(|p| f.is_empty() || p.title.to_lowercase().contains(&f)).collect()
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn selected_playlist(&self) -> Option<&Playlist> {
        self.visible_playlists().get(self.playlist_state.selected()?).copied()
    }

    pub(super) fn playlist(&self, key: &str) -> Option<&Playlist> {
        match &self.playlists {
            Some(Loadable::Ready(list)) => list.iter().find(|p| p.key == key),
            _ => None,
        }
    }

    fn playlist_mut(&mut self, key: &str) -> Option<&mut Playlist> {
        match &mut self.playlists {
            Some(Loadable::Ready(list)) => list.iter_mut().find(|p| p.key == key),
            _ => None,
        }
    }

    /// The tracks of the playlist shown on the right, when loaded.
    pub(super) fn shown_playlist_tracks(&self) -> Option<&PlaylistTracks> {
        match self.playlist_tracks.get(self.shown_playlist.as_deref()?) {
            Some(Loadable::Ready(pt)) => Some(pt),
            _ => None,
        }
    }

    /// Shows the playlist under the cursor on the right.
    pub(super) fn sync_shown_playlist(&mut self) {
        let Some(key) = self.selected_playlist().map(|p| p.key.clone()) else { return };
        if self.shown_playlist.as_deref() != Some(key.as_str()) {
            if let Some(old) = self.shown_playlist.replace(key.clone()) {
                self.drop_unneeded_load(&old);
            }
            self.track_state.select(Some(0));
            self.revisit_playlist(&key);
        }
    }

    /// Fetches a playlist again on a visit, so changes made elsewhere show
    /// up; not if it's already on its way or was fetched in the last minute
    /// (big playlists take a while).
    fn revisit_playlist(&mut self, key: &str) {
        if self.playlist_loads.contains_key(key) {
            return;
        }
        let fresh = self.playlist_fetched.get(key).map(|t| t.elapsed() < Duration::from_secs(60)).unwrap_or(false);
        if fresh && matches!(self.playlist_tracks.get(key), Some(Loadable::Ready(_))) {
            return;
        }
        self.load_playlist_tracks(key);
    }

    /// Stops loading a playlist you've moved past, unless it's still needed
    /// (playing or queued as it loads, or waiting to be). A half-loaded one
    /// is forgotten, to load afresh next time.
    fn drop_unneeded_load(&mut self, key: &str) {
        let Some(load) = self.playlist_loads.get(key) else { return };
        let needed = self.feed.as_ref().map(|f| f.key == key).unwrap_or(false)
            || self.playlist_when_loaded.iter().any(|(k, _)| k == key)
            || load.reload_after;
        if needed {
            return;
        }
        let progressive = load.progressive;
        self.playlist_loads.remove(key);
        self.live_loads.lock().unwrap().remove(key);
        if progressive {
            self.playlist_tracks.remove(key);
        }
    }

    /// `v`: the next view in the left pane.
    pub(super) fn toggle_view(&mut self) {
        let next = match self.view {
            LeftView::Artists => LeftView::Playlists,
            LeftView::Playlists => LeftView::Discover,
            LeftView::Discover => LeftView::Search,
            LeftView::Search => LeftView::Artists,
        };
        // Search doesn't start typing here, so v keeps cycling (f does).
        self.enter_view(next);
    }

    /// Shows `view` in the left pane.
    pub(super) fn enter_view(&mut self, view: LeftView) {
        self.view = view;
        self.filter.clear();
        self.filtering = false;
        self.focus = Focus::Tree;
        if self.right != RightView::Queue {
            self.right = RightView::Album;
        }
        self.track_state.select(Some(0));
        if self.view == LeftView::Search {
            self.sync_search();
        }
        if self.view == LeftView::Discover {
            // Fetched again each time: the shelves change as you listen.
            self.load_discover();
        }
        if self.view == LeftView::Playlists {
            // Fetched again each time, so playlists made elsewhere show up.
            self.load_playlists();
            if let Some(k) = self.shown_playlist.clone() {
                self.revisit_playlist(&k);
            } else {
                self.sync_shown_playlist();
            }
        }
    }

    fn move_playlist_cursor(&mut self, delta: i64) {
        let n = self.visible_playlists().len() as i64;
        if n == 0 {
            return;
        }
        let cur = self.playlist_state.selected().unwrap_or(0) as i64;
        self.playlist_state.select(Some((cur + delta).clamp(0, n - 1) as usize));
        self.sync_shown_playlist();
    }

    pub(super) fn playlist_list_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_playlist_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_playlist_cursor(1),
            KeyCode::PageUp => self.move_playlist_cursor(-(self.tree_page.max(1) as i64)),
            KeyCode::PageDown => self.move_playlist_cursor(self.tree_page.max(1) as i64),
            KeyCode::Home | KeyCode::Char('g') => self.move_playlist_cursor(-1_000_000),
            KeyCode::End | KeyCode::Char('G') => self.move_playlist_cursor(1_000_000),
            KeyCode::Right | KeyCode::Char('l') => {
                if self.selected_playlist().is_some() {
                    self.sync_shown_playlist();
                    self.right = RightView::Album;
                    self.focus = Focus::Tracks;
                }
            }
            KeyCode::Enter => self.act_on_playlist(Action::Play(0)),
            KeyCode::Char('a') => self.act_on_playlist(Action::Append),
            KeyCode::Char('A') => self.act_on_playlist(Action::PlayNext),
            KeyCode::Char('P') => self.act_on_playlist(Action::Pick),
            KeyCode::Char('R') => {
                if let Some(p) = self.selected_playlist().cloned() {
                    self.modal = Some(Modal::Input { purpose: InputFor::Rename { key: p.key }, text: p.title });
                }
            }
            KeyCode::Char('D') | KeyCode::Delete => {
                if let Some(p) = self.selected_playlist().cloned() {
                    self.modal = Some(Modal::Confirm { key: p.key, title: p.title });
                }
            }
            _ => {}
        }
    }

    /// Plays, queues or picks the playlist under the cursor, loading it first if need be.
    fn act_on_playlist(&mut self, action: Action) {
        let Some(p) = self.selected_playlist().cloned() else { return };
        self.sync_shown_playlist();
        // Usable as soon as its first page is in (see playlist_action).
        if matches!(self.playlist_tracks.get(&p.key), Some(Loadable::Ready(_))) {
            self.playlist_action(&p.key, action);
        } else {
            if !self.playlist_loads.contains_key(&p.key) {
                self.load_playlist_tracks(&p.key);
            }
            if matches!(action, Action::Play(_)) {
                self.when_loaded.clear();
                self.playlist_when_loaded.clear();
            }
            self.playlist_when_loaded.push((p.key, action));
        }
    }

    /// Plays or queues a playlist whose tracks have (at least partly)
    /// loaded. A big playlist still arriving starts playing (or is queued)
    /// with what's in, and the rest follows as it loads; playing next and
    /// adding to another playlist wait until it's all in.
    pub(super) fn playlist_action(&mut self, key: &str, action: Action) {
        let Some(Loadable::Ready(pt)) = self.playlist_tracks.get(key) else { return };
        let tracks = pt.tracks.clone();
        let title = self.playlist(key).map(|p| p.title.clone()).unwrap_or_else(|| "the playlist".into());
        let loading = self.still_loading(key);
        if tracks.is_empty() && loading.is_none() {
            self.say(format!("{} is empty", title));
            return;
        }
        if let Some((_, total)) = loading {
            if matches!(action, Action::PlayNext | Action::Pick) || tracks.is_empty() {
                self.playlist_when_loaded.push((key.to_string(), action));
                self.say(format!("Loading all {} tracks of {}…", total, title));
                return;
            }
        }
        let gen = self.playlist_loads.get(key).map(|l| l.gen).unwrap_or(0);
        match action {
            Action::Play(i) => {
                let index = i.min(tracks.len() - 1);
                self.when_loaded.clear();
                self.radio = None;
                self.feed = loading.map(|_| Feed { key: key.to_string(), gen, playing: true });
                self.player.send(Cmd::Play { queue: tracks, index });
                if let Some((_, total)) = loading {
                    self.say(format!("Playing {}: the rest of its {} tracks join the queue as they load", title, total));
                }
            }
            Action::Append => {
                if let Some((_, total)) = loading {
                    self.feed = Some(Feed { key: key.to_string(), gen, playing: false });
                    self.player.send(Cmd::Append(tracks));
                    self.say(format!("Adding {} ({} tracks) to the queue as it loads", title, total));
                } else {
                    self.queue_tracks(tracks, action, &title);
                }
            }
            Action::PlayNext => self.queue_tracks(tracks, action, &title),
            Action::Pick => self.open_picker(tracks, title),
        }
    }

    // ---------- the playlist's tracks ----------

    /// Keys in the right pane while it shows a playlist. Returns false for
    /// keys it doesn't handle.
    pub(super) fn playlist_tracks_key(&mut self, code: KeyCode) -> bool {
        let Some(key) = self.shown_playlist.clone() else { return false };
        let smart = self.playlist(&key).map(|p| p.smart).unwrap_or(false);
        let cur = self.track_state.selected().unwrap_or(0);
        match code {
            KeyCode::Enter => self.playlist_action(&key, Action::Play(cur)),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('J') | KeyCode::Char('K') if smart => {
                self.say("Smart playlists are built by Plex from rules, so their tracks can't be edited here".into());
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => self.remove_from_shown(cur),
            KeyCode::Char('J') => self.move_in_shown(cur, 1),
            KeyCode::Char('K') => self.move_in_shown(cur, -1),
            _ => return false,
        }
        true
    }

    fn remove_from_shown(&mut self, i: usize) {
        let Some(key) = self.shown_playlist.clone() else { return };
        let Some(Loadable::Ready(pt)) = self.playlist_tracks.get_mut(&key) else { return };
        let Some(&item) = pt.item_ids.get(i) else { return };
        if item == 0 {
            self.say("Still saving that track to the playlist; try again in a moment".into());
            return;
        }
        let t = pt.tracks.remove(i);
        pt.item_ids.remove(i);
        let left = pt.tracks.len();
        self.track_state.select(Some(i.min(left.saturating_sub(1))));
        if let Some(p) = self.playlist_mut(&key) {
            p.count = p.count.saturating_sub(1);
            p.duration_ms = p.duration_ms.saturating_sub(t.duration_ms.unwrap_or(0));
        }
        let name = self.playlist(&key).map(|p| p.title.clone()).unwrap_or_default();
        self.send_job(PlaylistJob::Remove { key, item });
        self.say(format!("Removed {} from {}", t.title, name));
    }

    /// Moves the track at `i` down (1) or up (-1) one place.
    fn move_in_shown(&mut self, i: usize, by: i64) {
        let Some(key) = self.shown_playlist.clone() else { return };
        let Some(Loadable::Ready(pt)) = self.playlist_tracks.get_mut(&key) else { return };
        let n = pt.tracks.len();
        let to = i as i64 + by;
        if i >= n || to < 0 || to >= n as i64 {
            return;
        }
        let to = to as usize;
        // Plex moves an item to just after another (or to the top).
        let item = pt.item_ids[i];
        let after = if by > 0 {
            Some(pt.item_ids[to])
        } else if to == 0 {
            None
        } else {
            Some(pt.item_ids[to - 1])
        };
        if item == 0 || after == Some(0) {
            self.say("Still saving the playlist; try again in a moment".into());
            return;
        }
        pt.tracks.swap(i, to);
        pt.item_ids.swap(i, to);
        self.track_state.select(Some(to));
        self.send_job(PlaylistJob::Move { key, item, after });
    }

    // ---------- adding to a playlist ----------

    /// Opens "Add to playlist" for `tracks`, named `what` in messages.
    pub(super) fn open_picker(&mut self, tracks: Vec<Track>, what: String) {
        if tracks.is_empty() {
            return;
        }
        if !matches!(self.playlists, Some(Loadable::Ready(_))) {
            self.load_playlists();
        }
        self.modal = Some(Modal::Picker { tracks, what, sel: 0 });
    }

    /// The playlists the picker offers: yours that aren't smart.
    pub(super) fn editable_playlists(&self) -> Vec<&Playlist> {
        match &self.playlists {
            Some(Loadable::Ready(list)) => list.iter().filter(|p| !p.smart).collect(),
            _ => Vec::new(),
        }
    }

    /// Adds `tracks` to an existing playlist.
    fn add_to_playlist(&mut self, key: &str, tracks: Vec<Track>, what: &str) {
        let keys: Vec<String> = tracks.iter().map(|t| t.key.clone()).collect();
        let n = tracks.len();
        let added_ms: u64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
        if let Some(Loadable::Ready(pt)) = self.playlist_tracks.get_mut(key) {
            // Shown at once; they get their item ids when the playlist is fetched again.
            pt.item_ids.extend(std::iter::repeat(0).take(n));
            pt.tracks.extend(tracks);
        }
        let name = match self.playlist_mut(key) {
            Some(p) => {
                p.count += n as u32;
                p.duration_ms += added_ms;
                p.title.clone()
            }
            None => "the playlist".into(),
        };
        self.send_job(PlaylistJob::Add { key: key.to_string(), keys });
        let count = if n == 1 { String::new() } else { format!(" ({} tracks)", n) };
        self.say(format!("Added {}{} to {}", what, count, name));
    }

    // ---------- the windows ----------

    /// Keys while a window is open.
    pub(super) fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        self.modal = match modal {
            Modal::Picker { tracks, what, sel } => {
                let options = 1 + self.editable_playlists().len();
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => None,
                    KeyCode::Up | KeyCode::Char('k') => Some(Modal::Picker { tracks, what, sel: sel.saturating_sub(1) }),
                    KeyCode::Down | KeyCode::Char('j') => {
                        Some(Modal::Picker { tracks, what, sel: (sel + 1).min(options - 1) })
                    }
                    KeyCode::Home | KeyCode::Char('g') => Some(Modal::Picker { tracks, what, sel: 0 }),
                    KeyCode::End | KeyCode::Char('G') => Some(Modal::Picker { tracks, what, sel: options - 1 }),
                    KeyCode::Enter if sel == 0 => Some(Modal::Input {
                        purpose: InputFor::NewPlaylist { tracks, what },
                        text: String::new(),
                    }),
                    KeyCode::Enter => {
                        if let Some(key) = self.editable_playlists().get(sel - 1).map(|p| p.key.clone()) {
                            self.add_to_playlist(&key, tracks, &what);
                        }
                        None
                    }
                    _ => Some(Modal::Picker { tracks, what, sel }),
                }
            }
            Modal::Input { purpose, mut text } => match k.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let name = text.trim().to_string();
                    if name.is_empty() {
                        Some(Modal::Input { purpose, text })
                    } else {
                        self.submit_name(purpose, name);
                        None
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    Some(Modal::Input { purpose, text })
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if text.chars().count() < 200 {
                        text.push(c);
                    }
                    Some(Modal::Input { purpose, text })
                }
                _ => Some(Modal::Input { purpose, text }),
            },
            Modal::Confirm { key, title } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.delete_playlist(&key, &title);
                    None
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
                _ => Some(Modal::Confirm { key, title }),
            },
            Modal::Help { scroll } => self.help_key(k.code, scroll),
        };
    }

    fn submit_name(&mut self, purpose: InputFor, name: String) {
        match purpose {
            InputFor::NewPlaylist { tracks, what } => {
                let keys: Vec<String> = tracks.iter().map(|t| t.key.clone()).collect();
                self.send_job(PlaylistJob::Create { title: name.clone(), keys });
                let _ = what;
                self.say(format!("Creating playlist {}…", name));
            }
            InputFor::Rename { key } => {
                let old = self.playlist(&key).map(|p| p.title.clone()).unwrap_or_default();
                if old == name {
                    return;
                }
                if let Some(p) = self.playlist_mut(&key) {
                    p.title = name.clone();
                }
                self.send_job(PlaylistJob::Rename { key, title: name.clone() });
                self.say(format!("Renamed {} to {}", old, name));
            }
        }
    }

    fn delete_playlist(&mut self, key: &str, title: &str) {
        if let Some(Loadable::Ready(list)) = &mut self.playlists {
            list.retain(|p| p.key != key);
        }
        self.playlist_tracks.remove(key);
        if self.shown_playlist.as_deref() == Some(key) {
            self.shown_playlist = None;
        }
        let n = self.visible_playlists().len();
        let sel = self.playlist_state.selected().unwrap_or(0).min(n.saturating_sub(1));
        self.playlist_state.select(Some(sel));
        self.sync_shown_playlist();
        if self.shown_playlist.is_none() && self.focus == Focus::Tracks && self.right == RightView::Album {
            self.focus = Focus::Tree;
        }
        self.send_job(PlaylistJob::Delete { key: key.to_string() });
        self.say(format!("Deleted playlist {}", title));
    }

    // ---------- drawing ----------

    pub(super) fn draw_playlist_list(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Tree;
        let filter_label = self.filter_label(area.width);
        let title = if filter_label.chars().count() + 25 > area.width as usize && !filter_label.is_empty() {
            Line::from("")
        } else {
            self.view_switcher_for(focused, area.width)
        };
        let mut block = self.pane(title, filter_label, focused);
        let vis_len = self.visible_playlists().len();
        if let Some(sel) = self.playlist_state.selected().filter(|_| vis_len > 0) {
            block = block.title_bottom(
                Line::from(Span::styled(format!(" {} of {} ", sel + 1, vis_len), Style::default().fg(FAINT)))
                    .alignment(Alignment::Right),
            );
        }
        self.tree_page = area.height.saturating_sub(3) as usize;
        let inner_w = area.width.saturating_sub(2) as usize;
        let items: Vec<ListItem> = match &self.playlists {
            None | Some(Loadable::Loading) => {
                vec![ListItem::new(Span::styled(" Loading playlists…", Style::default().fg(DIM)))]
            }
            Some(Loadable::Failed(e)) => vec![ListItem::new(Span::styled(
                format!(" Couldn't load playlists: {}", e),
                Style::default().fg(WARN),
            ))],
            Some(Loadable::Ready(list)) if list.is_empty() => vec![
                ListItem::new(Span::styled(" No playlists in this library yet.", Style::default().fg(DIM))),
                ListItem::new(""),
                ListItem::new(Span::styled(" Press P on an album or track", Style::default().fg(FAINT))),
                ListItem::new(Span::styled(" to start one.", Style::default().fg(FAINT))),
            ],
            Some(Loadable::Ready(_)) => self
                .visible_playlists()
                .into_iter()
                .map(|p| {
                    // Smart playlists say so, dimly, after the title.
                    let tag = if p.smart { " · smart" } else { "" };
                    let count = format!("{}", p.count);
                    let room = inner_w.saturating_sub(2 + tag.len() + count.len() + 2);
                    let title = truncate(&p.title, room);
                    let used = 1 + title.chars().count() + tag.chars().count() + count.len() + 1;
                    let pad = inner_w.saturating_sub(used);
                    ListItem::new(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(title, Style::default().fg(FG)),
                        Span::styled(tag, Style::default().fg(FAINT)),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(count, Style::default().fg(FAINT)),
                        Span::raw(" "),
                    ]))
                })
                .collect(),
        };
        let list = List::new(items).block(block).scroll_padding(3).highlight_style(
            Style::default()
                .bg(SEL_BG)
                .fg(BRIGHT)
                .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
        );
        let mut state = self.playlist_state.clone();
        if !matches!(&self.playlists, Some(Loadable::Ready(l)) if !l.is_empty()) {
            state.select(None);
        }
        f.render_stateful_widget(list, area, &mut state);
        *self.playlist_state.offset_mut() = state.offset();
    }

    pub(super) fn draw_playlist_tracks(&mut self, f: &mut Frame, area: Rect, st: &Status) {
        let focused = self.focus == Focus::Tracks;
        let playlist = self.shown_playlist.as_deref().and_then(|k| self.playlist(k)).cloned();
        let (title, right) = match &playlist {
            Some(p) => {
                let name = Span::styled(p.title.clone(), title_style(focused));
                let kind = Span::styled(
                    if p.smart { " · smart playlist" } else { " · playlist" }.to_string(),
                    Style::default().fg(DIM),
                );
                let summary = format!(
                    "{} track{} · {}",
                    p.count,
                    if p.count == 1 { "" } else { "s" },
                    mmss(p.duration_ms as f64 / 1000.0)
                );
                let rights = match self.still_loading(&p.key) {
                    Some((n, total)) if n > 0 => vec![
                        format!(" loading {} of {} · {} ", n, total, summary),
                        format!(" loading {} of {} ", n, total),
                    ],
                    _ => vec![format!(" {} ", summary)],
                };
                fit_pane_titles(area.width, name, kind, Vec::new(), rights)
            }
            None => (Line::from(Span::styled(" Tracks ", title_style(focused))), String::new()),
        };
        let block = self.pane(title, right, focused);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let inner_w = inner.width as usize;
        let message = |text: String, color: Color| Paragraph::new(Span::styled(text, Style::default().fg(color)));

        let Some(p) = playlist else {
            f.render_widget(message(" Select a playlist on the left.".into(), DIM), inner);
            return;
        };
        let pt = match self.playlist_tracks.get(&p.key) {
            Some(Loadable::Ready(pt)) => pt,
            Some(Loadable::Failed(e)) => {
                f.render_widget(message(format!(" Couldn't load the playlist: {}", e), WARN), inner);
                return;
            }
            _ => {
                f.render_widget(message(" Loading tracks…".into(), DIM), inner);
                return;
            }
        };
        if pt.tracks.is_empty() {
            f.render_widget(message(" This playlist is empty. Press P on an album or track to add to it.".into(), DIM), inner);
            return;
        }

        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        let head = Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(playlist_line("#", "Title", "Artist", vec![Span::styled("Rating", head)], "Time", inner_w, [head; 4])),
            parts[0],
        );
        f.render_widget(Paragraph::new(Span::styled("─".repeat(inner_w), Style::default().fg(BORDER))), parts[1]);

        let playing_key = st.track.as_ref().filter(|_| st.state != PlayState::Stopped).map(|t| t.key.clone());
        let selected = self.track_state.selected();
        let items: Vec<ListItem> = pt
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let playing = playing_key.as_deref() == Some(t.key.as_str());
                let num = if playing { "▶".to_string() } else { (i + 1).to_string() };
                let styles = if playing {
                    [Style::default().fg(ACCENT).add_modifier(Modifier::BOLD); 4]
                } else if selected == Some(i) {
                    [Style::default().fg(BRIGHT); 4]
                } else {
                    [Style::default().fg(FAINT), Style::default().fg(FG), Style::default().fg(DIM), Style::default().fg(DIM)]
                };
                let stars = match self.rating_of(t) {
                    Some(r) => star_spans(r, if playing { DIM } else { FAINT }),
                    None => Vec::new(),
                };
                let time = t.duration_ms.map(|d| mmss(d as f64 / 1000.0)).unwrap_or_default();
                ListItem::new(playlist_line(&num, &t.title, &t.artist, stars, &time, inner_w, styles))
            })
            .collect();
        let list = List::new(items)
            .scroll_padding(2)
            .highlight_style(Style::default().bg(if focused { SEL_BG } else { Color::Reset }));
        f.render_stateful_widget(list, parts[2], &mut self.track_state);
    }

    /// The window over the screen, if one is open.
    pub(super) fn draw_modal(&mut self, f: &mut Frame) {
        let area = f.area();
        match &self.modal {
            None => {}
            Some(Modal::Picker { tracks, what, sel }) => {
                let count = if tracks.len() == 1 { String::new() } else { format!(" ({} tracks)", tracks.len()) };
                let mut lines: Vec<Line> = vec![
                    Line::from(Span::styled(
                        truncate(&format!(" Adding {}{}", what, count), area.width.saturating_sub(8) as usize),
                        Style::default().fg(DIM),
                    )),
                    Line::from(""),
                ];
                let mut options = vec![("+ New playlist…".to_string(), String::new())];
                options.extend(self.editable_playlists().iter().map(|p| (p.title.clone(), p.count.to_string())));
                let loading = !matches!(self.playlists, Some(Loadable::Ready(_)));
                let w: u16 = 54.min(area.width.saturating_sub(4));
                let inner_w = w.saturating_sub(4) as usize;
                // Keep the highlighted row in view in a long list.
                let rows = (area.height.saturating_sub(10) as usize).max(3);
                let first = sel.saturating_sub(rows.saturating_sub(1));
                for (i, (name, n)) in options.iter().enumerate().skip(first).take(rows) {
                    let on = i == *sel;
                    let name = truncate(name, inner_w.saturating_sub(n.len() + 3));
                    let pad = inner_w.saturating_sub(name.chars().count() + n.len() + 2);
                    let style = if on { Style::default().fg(BRIGHT).bg(SEL_BG).add_modifier(Modifier::BOLD) } else { Style::default().fg(FG) };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {}", name), style),
                        Span::styled(" ".repeat(pad), style),
                        Span::styled(format!("{} ", n), if on { style } else { Style::default().fg(FAINT) }),
                    ]));
                }
                if loading {
                    lines.push(Line::from(Span::styled(" Loading your playlists…", Style::default().fg(FAINT))));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(" ↑↓ choose   enter add   esc cancel", Style::default().fg(FAINT))));
                draw_window(f, " Add to playlist ", lines, w);
            }
            Some(Modal::Input { purpose, text }) => {
                let (title, prompt) = match purpose {
                    InputFor::NewPlaylist { tracks, what } => {
                        let count = if tracks.len() == 1 { String::new() } else { format!(" ({} tracks)", tracks.len()) };
                        (" New playlist ", format!(" A new playlist with {}{}. Name it:", what, count))
                    }
                    InputFor::Rename { .. } => (" Rename playlist ", " New name:".to_string()),
                };
                let w: u16 = 60.min(area.width.saturating_sub(4));
                let room = w.saturating_sub(6) as usize;
                // Show the end of a long name, where you're typing.
                let shown: String = {
                    let n = text.chars().count();
                    text.chars().skip(n.saturating_sub(room)).collect()
                };
                let lines = vec![
                    Line::from(Span::styled(truncate(&prompt, w.saturating_sub(4) as usize), Style::default().fg(DIM))),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(" › ", Style::default().fg(ACCENT)),
                        Span::styled(shown, Style::default().fg(BRIGHT)),
                        Span::styled("▏", Style::default().fg(ACCENT)),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(" enter save   esc cancel", Style::default().fg(FAINT))),
                ];
                draw_window(f, title, lines, w);
            }
            Some(Modal::Confirm { title, .. }) => {
                let w: u16 = 56.min(area.width.saturating_sub(4));
                let lines = vec![
                    Line::from(Span::styled(
                        truncate(&format!(" Delete {}?", title), w.saturating_sub(4) as usize),
                        Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(" It's deleted from your Plex server. This can't be undone.", Style::default().fg(DIM))),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(" y", Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
                        Span::styled(" delete   ", Style::default().fg(FAINT)),
                        Span::styled("n", Style::default().fg(FG).add_modifier(Modifier::BOLD)),
                        Span::styled(" keep it", Style::default().fg(FAINT)),
                    ]),
                ];
                draw_window(f, " Delete playlist ", lines, w);
            }
            Some(Modal::Help { scroll }) => {
                let scroll = *scroll;
                self.draw_help(f, scroll);
            }
        }
    }
}

/// A centred window `width` wide, tall enough for `lines`.
pub(super) fn draw_window(f: &mut Frame, title: &str, lines: Vec<Line>, width: u16) {
    let area = f.area();
    let h = (lines.len() as u16 + 2).min(area.height);
    let w = width.min(area.width);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(Span::styled(title.to_string(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))))
        .style(Style::default().bg(Color::Reset));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// One row of a playlist: number, title, artist, rating, time.
fn playlist_line(
    num: &str,
    title: &str,
    artist: &str,
    rating: Vec<Span<'static>>,
    time: &str,
    width: usize,
    styles: [Style; 4],
) -> Line<'static> {
    let num_w = 5;
    let time_w = 7;
    let rest = width.saturating_sub(num_w + RATING_W + time_w + 2).max(12);
    let title_w = rest * 58 / 100;
    let artist_w = rest - title_w;
    let rating_used: usize = rating.iter().map(|s| s.content.chars().count()).sum();
    let col = |text: &str, w: usize| format!("{:<w$}", truncate(text, w.saturating_sub(2)));
    let mut spans = vec![
        Span::styled(format!(" {:<num_w$}", truncate(num, num_w - 1)), styles[0]),
        Span::styled(col(title, title_w), styles[1]),
        Span::styled(col(artist, artist_w), styles[2]),
    ];
    spans.extend(rating);
    spans.push(Span::raw(" ".repeat(RATING_W.saturating_sub(rating_used))));
    spans.push(Span::styled(format!("{:>time_w$}", time), styles[3]));
    Line::from(spans)
}
