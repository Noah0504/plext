//! The play queue: the tracks to play, in order, and where playback is.
//!
//! The player thread owns the queue and is the only one that changes it (the
//! UI and MPRIS ask through `Cmd`s); everyone else reads a shared copy.
//!
//! Tracks before the current one are history: they stay in the queue so
//! "previous" can go back to them, and the queue view shows them dimmed.
//! Shuffling only reorders what's still to come, and remembers the order it
//! had so turning shuffle off puts it back.

use crate::plex::Track;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    /// Start the queue over at the end.
    All,
    /// Play the current track again and again.
    One,
}

impl Repeat {
    /// Off → whole queue → this track → off.
    pub fn cycled(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

/// One track in the queue. The id tells apart two copies of the same track.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub track: Track,
}

#[derive(Debug, Clone, Default)]
pub struct Queue {
    items: Vec<Entry>,
    /// Index of the current track in `items`. Equal to `items.len()` only
    /// when the queue is empty.
    pos: usize,
    shuffle: bool,
    repeat: Repeat,
    /// The order before shuffling (entry ids), to restore when shuffle goes off.
    unshuffled: Vec<u64>,
    next_id: u64,
    /// Bumped on every change, so readers can tell when to look again.
    pub version: u64,
    rng: u64,
}

impl Queue {
    pub fn new() -> Queue {
        Queue { rng: seed(), ..Default::default() }
    }

    pub fn items(&self) -> &[Entry] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn current(&self) -> Option<&Entry> {
        self.items.get(self.pos)
    }

    fn changed(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    fn entries(&mut self, tracks: Vec<Track>) -> Vec<Entry> {
        tracks
            .into_iter()
            .map(|track| {
                self.next_id += 1;
                Entry { id: self.next_id, track }
            })
            .collect()
    }

    /// Replaces the queue with `tracks`, starting at `index`. With shuffle
    /// on, that track plays first and the rest follow in shuffled order.
    pub fn replace(&mut self, tracks: Vec<Track>, index: usize) {
        self.items = self.entries(tracks);
        self.pos = index.min(self.items.len().saturating_sub(1));
        if self.items.is_empty() {
            self.pos = 0;
        }
        self.unshuffled = self.ids();
        if self.shuffle && !self.items.is_empty() {
            // A fresh album with shuffle on: the chosen track, then the
            // others (including the ones before it) in random order.
            let first = self.items.remove(self.pos);
            self.items.insert(0, first);
            self.pos = 0;
            self.shuffle_upcoming();
        }
        self.changed();
    }

    /// Adds tracks to the end. Returns how many were added.
    pub fn append(&mut self, tracks: Vec<Track>) -> usize {
        let new = self.entries(tracks);
        let n = new.len();
        self.unshuffled.extend(new.iter().map(|e| e.id));
        self.items.extend(new);
        self.changed();
        n
    }

    /// Adds the rest of something already playing (a big playlist still
    /// loading). With shuffle on they're mixed in among the tracks still to
    /// come, so the whole thing is shuffled; otherwise they go at the end.
    pub fn append_mixed(&mut self, tracks: Vec<Track>) -> usize {
        if !self.shuffle || self.items.is_empty() {
            return self.append(tracks);
        }
        let new = self.entries(tracks);
        let n = new.len();
        self.unshuffled.extend(new.iter().map(|e| e.id));
        for e in new {
            let lo = self.pos + 1;
            let span = (self.items.len() + 1 - lo) as u64;
            let at = lo + (self.random() % span) as usize;
            self.items.insert(at, e);
        }
        self.changed();
        n
    }

    /// Adds tracks right after the current one (at the start if the queue is
    /// empty). Returns how many were added.
    pub fn play_next(&mut self, tracks: Vec<Track>) -> usize {
        let new = self.entries(tracks);
        let n = new.len();
        if self.items.is_empty() {
            self.unshuffled = new.iter().map(|e| e.id).collect();
            self.items = new;
            self.pos = 0;
        } else {
            let after = self.items[self.pos].id;
            let at = self.unshuffled.iter().position(|&i| i == after).map(|p| p + 1).unwrap_or(self.unshuffled.len());
            self.unshuffled.splice(at..at, new.iter().map(|e| e.id));
            self.items.splice(self.pos + 1..self.pos + 1, new);
        }
        self.changed();
        n
    }

    /// Removes a track. Returns true if it was the current one (the queue then
    /// points at the track that followed it).
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.index_of(id) else { return false };
        self.items.remove(i);
        self.unshuffled.retain(|&x| x != id);
        let was_current = i == self.pos;
        if i < self.pos {
            self.pos -= 1;
        }
        if self.pos >= self.items.len() {
            // Removed the last track while it was current: wrap or stop.
            self.pos = if self.repeat == Repeat::All { 0 } else { self.items.len() };
            if self.items.is_empty() {
                self.pos = 0;
            }
        }
        self.changed();
        was_current
    }

    /// Moves a track up (negative) or down by `by` places among the tracks
    /// still to come. History and the current track stay put.
    pub fn move_by(&mut self, id: u64, by: i64) {
        let Some(i) = self.index_of(id) else { return };
        if i <= self.pos {
            return;
        }
        let lo = self.pos + 1;
        let hi = self.items.len() - 1;
        let to = (i as i64 + by).clamp(lo as i64, hi as i64) as usize;
        if to == i {
            return;
        }
        let e = self.items.remove(i);
        self.items.insert(to, e);
        if !self.shuffle {
            self.unshuffled = self.ids();
        }
        self.changed();
    }

    /// Drops every track after the current one.
    pub fn clear_upcoming(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let keep: Vec<u64> = self.items[..=self.pos].iter().map(|e| e.id).collect();
        self.items.truncate(self.pos + 1);
        self.unshuffled.retain(|i| keep.contains(i));
        self.changed();
    }

    /// Makes `id` the current track. Returns false if it isn't in the queue.
    pub fn jump(&mut self, id: u64) -> bool {
        match self.index_of(id) {
            Some(i) => {
                self.pos = i;
                self.changed();
                true
            }
            None => false,
        }
    }

    pub fn set_shuffle(&mut self, on: bool) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        if on {
            self.unshuffled = self.ids();
            self.shuffle_upcoming();
        } else if let Some(cur) = self.current().map(|e| e.id) {
            // Back to the original order, carrying on from the current track.
            let mut by_id: Vec<Option<Entry>> = Vec::new();
            let mut order = self.unshuffled.clone();
            // Anything not in the remembered order goes at the end.
            for e in &self.items {
                if !order.contains(&e.id) {
                    order.push(e.id);
                }
            }
            for id in &order {
                by_id.push(self.items.iter().find(|e| e.id == *id).cloned());
            }
            self.items = by_id.into_iter().flatten().collect();
            self.pos = self.index_of(cur).unwrap_or(0);
        }
        self.changed();
    }

    pub fn set_repeat(&mut self, r: Repeat) {
        if r != self.repeat {
            self.repeat = r;
            self.changed();
        }
    }

    /// Where playback goes when a track finishes on its own. Returns false at
    /// the end of the queue (the queue then points back at its first track).
    pub fn advance_finished(&mut self) -> bool {
        if self.repeat == Repeat::One && !self.items.is_empty() {
            return true;
        }
        self.step_forward()
    }

    /// Next track on request: skips even with repeat-one on.
    pub fn next(&mut self) -> bool {
        self.step_forward()
    }

    fn step_forward(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        if self.pos + 1 < self.items.len() {
            self.pos += 1;
            self.changed();
            return true;
        }
        self.pos = 0;
        if self.repeat == Repeat::All {
            if self.shuffle {
                // A new lap gets a new order.
                self.shuffle_from(0);
            }
            self.changed();
            return true;
        }
        self.changed();
        false
    }

    /// Previous track. At the start, stays on the first track.
    pub fn prev(&mut self) {
        if self.pos > 0 {
            self.pos -= 1;
            self.changed();
        }
    }

    /// The track that plays after the current one finishes, if any.
    pub fn peek_finished(&self) -> Option<&Entry> {
        if self.repeat == Repeat::One {
            return self.current();
        }
        self.upcoming(1).into_iter().next()
    }

    /// The next `n` tracks in playing order, wrapping round with repeat-all.
    pub fn upcoming(&self, n: usize) -> Vec<&Entry> {
        let len = self.items.len();
        if len == 0 {
            return Vec::new();
        }
        let after = &self.items[self.pos + 1..];
        if self.repeat != Repeat::All {
            return after.iter().take(n).collect();
        }
        // Wrapping round: everything but the current track, once.
        after.iter().chain(self.items[..self.pos].iter()).take(n).collect()
    }

    fn ids(&self) -> Vec<u64> {
        self.items.iter().map(|e| e.id).collect()
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|e| e.id == id)
    }

    fn shuffle_upcoming(&mut self) {
        let from = (self.pos + 1).min(self.items.len());
        self.shuffle_from(from);
    }

    /// Fisher–Yates over `items[from..]`.
    fn shuffle_from(&mut self, from: usize) {
        let n = self.items.len();
        if n < from + 2 {
            return;
        }
        for i in (from + 1..n).rev() {
            let j = from + (self.random() % (i - from + 1) as u64) as usize;
            self.items.swap(i, j);
        }
    }

    /// xorshift64*: plenty for shuffling a playlist.
    fn random(&mut self) -> u64 {
        if self.rng == 0 {
            self.rng = seed();
        }
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    // ---------- saving between runs ----------

    pub fn to_saved(&self, machine_id: &str, position: f64) -> Saved {
        Saved {
            machine_id: machine_id.to_string(),
            items: self.items.clone(),
            pos: self.pos,
            position,
            shuffle: self.shuffle,
            repeat: self.repeat,
            unshuffled: self.unshuffled.clone(),
        }
    }

    pub fn from_saved(s: Saved) -> Queue {
        let mut q = Queue::new();
        q.next_id = s.items.iter().map(|e| e.id).max().unwrap_or(0);
        q.pos = s.pos.min(s.items.len().saturating_sub(1));
        q.items = s.items;
        q.shuffle = s.shuffle;
        q.repeat = s.repeat;
        q.unshuffled = s.unshuffled;
        q.changed();
        q
    }
}

fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    (t ^ ((std::process::id() as u64) << 32)) | 1
}

/// The queue as saved to disk when Plext quits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    /// The server the tracks belong to; a queue from another server is ignored.
    pub machine_id: String,
    pub items: Vec<Entry>,
    pub pos: usize,
    /// Seconds into the current track.
    pub position: f64,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub unshuffled: Vec<u64>,
}

/// ~/.local/state/plext/queue.json
pub fn saved_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::cache_dir)?;
    Some(base.join("plext").join("queue.json"))
}

pub fn save(s: &Saved) {
    let Some(p) = saved_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let result = serde_json::to_vec(s)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            // Write then rename, so a crash mid-write can't leave half a file.
            let tmp = p.with_extension("json.tmp");
            fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, &p)).map_err(|e| e.to_string())
        });
    if let Err(e) = result {
        crate::log::write(&format!("couldn't save the queue: {}", e));
    }
}

/// Removes the saved queue (the queue was emptied).
pub fn forget() {
    if let Some(p) = saved_path() {
        let _ = fs::remove_file(p);
    }
}

/// The saved queue for this server, if there is one.
pub fn load(machine_id: &str) -> Option<Saved> {
    let bytes = fs::read(saved_path()?).ok()?;
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(s) if s.machine_id == machine_id && !s.items.is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            crate::log::write(&format!("ignoring the saved queue: {}", e));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str) -> Track {
        Track {
            key: key.into(),
            title: key.into(),
            album_key: None,
            artist: String::new(),
            album: String::new(),
            index: None,
            disc: None,
            duration_ms: None,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            channels: None,
            bitrate_kbps: None,
            part_key: None,
            container: None,
            rating: None,
            thumb: None,
        }
    }

    fn tracks(keys: &str) -> Vec<Track> {
        keys.split_whitespace().map(t).collect()
    }

    fn keys(q: &Queue) -> String {
        q.items().iter().map(|e| e.track.key.as_str()).collect::<Vec<_>>().join(" ")
    }

    fn cur(q: &Queue) -> &str {
        q.current().map(|e| e.track.key.as_str()).unwrap_or("-")
    }

    fn id_of(q: &Queue, key: &str) -> u64 {
        q.items().iter().find(|e| e.track.key == key).unwrap().id
    }

    #[test]
    fn append_and_play_next() {
        let mut q = Queue::new();
        q.replace(tracks("a b c"), 1);
        assert_eq!(cur(&q), "b");
        q.append(tracks("x y"));
        assert_eq!(keys(&q), "a b c x y");
        q.play_next(tracks("n1 n2"));
        assert_eq!(keys(&q), "a b n1 n2 c x y");
        assert_eq!(cur(&q), "b");
        // Adding to an empty queue makes the first added track current.
        let mut e = Queue::new();
        e.play_next(tracks("p q"));
        assert_eq!((keys(&e).as_str(), cur(&e)), ("p q", "p"));
    }

    #[test]
    fn the_rest_of_a_big_playlist_is_mixed_in_when_shuffled() {
        let mut q = Queue::new();
        q.replace(tracks("a b c"), 0);
        q.append_mixed(tracks("d e"));
        assert_eq!(keys(&q), "a b c d e", "in order without shuffle");
        let mut s = Queue::new();
        s.set_shuffle(true);
        s.replace(tracks("p0 p1 p2 p3 p4 p5 p6 p7"), 0);
        let later: Vec<Track> = (0..40).map(|i| t(&format!("x{}", i))).collect();
        s.append_mixed(later);
        assert_eq!(s.len(), 48);
        assert_eq!(cur(&s), "p0", "the playing track stays first");
        // Spread through the queue, not all at the end.
        let first_x = s.items().iter().position(|e| e.track.key.starts_with('x')).unwrap();
        assert!(first_x < 10, "first late track at {}", first_x);
        s.set_shuffle(false);
        assert!(keys(&s).ends_with("x38 x39") && keys(&s).starts_with("p0 p1"));
    }

    #[test]
    fn the_same_track_twice_is_two_entries() {
        let mut q = Queue::new();
        q.replace(tracks("a"), 0);
        q.append(tracks("a"));
        assert_eq!(q.len(), 2);
        assert_ne!(q.items()[0].id, q.items()[1].id);
        let second = q.items()[1].id;
        q.remove(second);
        assert_eq!(keys(&q), "a");
    }

    #[test]
    fn finishing_moves_on_and_stops_at_the_end() {
        let mut q = Queue::new();
        q.replace(tracks("a b"), 0);
        assert!(q.advance_finished());
        assert_eq!(cur(&q), "b");
        assert!(!q.advance_finished());
        // Back at the start, ready to play again.
        assert_eq!(cur(&q), "a");
    }

    #[test]
    fn repeat_all_wraps_and_repeat_one_stays() {
        let mut q = Queue::new();
        q.replace(tracks("a b"), 1);
        q.set_repeat(Repeat::All);
        assert_eq!(q.upcoming(3).iter().map(|e| e.track.key.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(q.advance_finished());
        assert_eq!(cur(&q), "a");
        q.set_repeat(Repeat::One);
        assert!(q.advance_finished());
        assert_eq!(cur(&q), "a");
        assert_eq!(q.peek_finished().unwrap().track.key, "a");
        // Next still skips with repeat-one on.
        assert!(q.next());
        assert_eq!(cur(&q), "b");
        assert_eq!(Repeat::Off.cycled().cycled().cycled(), Repeat::Off);
    }

    #[test]
    fn shuffle_keeps_history_and_current_and_unshuffle_restores_order() {
        let all = "a b c d e f g h i j k l m n o p";
        let mut q = Queue::new();
        q.replace(tracks(all), 2);
        q.set_shuffle(true);
        assert_eq!(&keys(&q)[..5], "a b c");
        assert_eq!(cur(&q), "c");
        let mut sorted: Vec<String> = keys(&q).split(' ').map(String::from).collect();
        sorted.sort();
        assert_eq!(sorted.join(" "), all, "shuffle must not lose or duplicate tracks");
        assert_ne!(keys(&q), all, "16 tracks staying in order is vanishingly unlikely");
        // Play a couple, then turn shuffle off: original order, still on the same track.
        q.next();
        q.next();
        let now = cur(&q).to_string();
        q.set_shuffle(false);
        assert_eq!(keys(&q), all);
        assert_eq!(cur(&q), now);
    }

    #[test]
    fn replacing_with_shuffle_on_starts_with_the_chosen_track() {
        let mut q = Queue::new();
        q.set_shuffle(true);
        q.replace(tracks("a b c d e f g h"), 5);
        assert_eq!(q.pos(), 0);
        assert_eq!(cur(&q), "f");
        q.set_shuffle(false);
        assert_eq!(keys(&q), "a b c d e f g h");
        assert_eq!(cur(&q), "f");
    }

    #[test]
    fn play_next_while_shuffled_lands_after_current_in_both_orders() {
        let mut q = Queue::new();
        q.replace(tracks("a b c d"), 0);
        q.set_shuffle(true);
        q.play_next(tracks("x"));
        assert_eq!(q.items()[1].track.key, "x");
        q.set_shuffle(false);
        assert_eq!(keys(&q), "a x b c d");
    }

    #[test]
    fn removing_and_moving() {
        let mut q = Queue::new();
        q.replace(tracks("a b c d e"), 1);
        // Removing history keeps the current track current.
        assert!(!q.remove(id_of(&q, "a")));
        assert_eq!(cur(&q), "b");
        // Removing the current track moves on to the next.
        assert!(q.remove(id_of(&q, "b")));
        assert_eq!(cur(&q), "c");
        // Moving only reorders what's still to come.
        q.move_by(id_of(&q, "e"), -5);
        assert_eq!(keys(&q), "c e d");
        q.move_by(id_of(&q, "c"), 1); // the current track stays put
        assert_eq!(keys(&q), "c e d");
        q.clear_upcoming();
        assert_eq!(keys(&q), "c");
        // Removing the last track leaves an empty queue.
        q.remove(id_of(&q, "c"));
        assert!(q.is_empty() && q.current().is_none());
    }

    #[test]
    fn jumping() {
        let mut q = Queue::new();
        q.replace(tracks("a b c"), 0);
        assert!(q.jump(id_of(&q, "c")));
        assert_eq!(cur(&q), "c");
        assert!(!q.jump(999));
        q.prev();
        assert_eq!(cur(&q), "b");
    }

    #[test]
    fn survives_a_round_trip_through_json() {
        let mut q = Queue::new();
        q.replace(tracks("a b c"), 1);
        q.set_repeat(Repeat::All);
        q.set_shuffle(true);
        let saved = q.to_saved("srv", 42.5);
        let text = serde_json::to_string(&saved).unwrap();
        let mut back = Queue::from_saved(serde_json::from_str(&text).unwrap());
        assert_eq!(keys(&back), keys(&q));
        assert_eq!(cur(&back), "b");
        assert!(back.shuffle());
        assert_eq!(back.repeat(), Repeat::All);
        // New entries don't reuse saved ids.
        back.append(tracks("d"));
        let ids: Vec<u64> = back.items().iter().map(|e| e.id).collect();
        let mut dedup = ids.clone();
        dedup.dedup();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), ids.len());
        back.set_shuffle(false);
        assert_eq!(keys(&back), "a b c d");
    }

    #[test]
    fn saves_to_disk_and_ignores_other_servers() {
        // The only test that touches the state directory, so it can point it elsewhere.
        let dir = std::env::temp_dir().join(format!("plext-state-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let mut q = Queue::new();
        q.replace(tracks("a b c"), 2);
        save(&q.to_saved("srv", 61.0));
        let back = load("srv").expect("saved queue");
        assert_eq!((back.pos, back.position, back.items.len()), (2, 61.0, 3));
        assert!(load("another-server").is_none());
        forget();
        assert!(load("srv").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
