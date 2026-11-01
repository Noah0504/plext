//! Playback engine: runs on its own thread, owns the DAC, and plays the queue
//! (see `queue.rs`). Consecutive tracks with the same format are played
//! gaplessly.

use crate::decode::{Decoder, SourceInfo};
use crate::download::Download;
use crate::output::{Output, OutputInfo, StreamSpec, Target};
use crate::plex::{Server, Track};
use crate::queue::{Entry, Queue, Repeat, Saved};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub enum Cmd {
    /// Replace the queue with these tracks and play from `index`.
    Play { queue: Vec<Track>, index: usize },
    /// Add tracks to the end of the queue.
    Append(Vec<Track>),
    /// Add tracks right after the current one.
    PlayNext(Vec<Track>),
    /// Add the rest of what's playing (a big playlist still loading): mixed
    /// in among the upcoming tracks when shuffled, else at the end.
    AppendMixed(Vec<Track>),
    /// Remove a queue entry (by id); removing the current one skips it.
    Remove(u64),
    /// Move a queue entry up (negative) or down.
    Move { id: u64, by: i64 },
    /// Play this queue entry now.
    Jump(u64),
    /// Drop everything after the current track.
    ClearUpcoming,
    ToggleShuffle,
    SetShuffle(bool),
    CycleRepeat,
    SetRepeat(Repeat),
    /// Bring back the queue saved last time, paused where it was left.
    Restore(Saved),
    TogglePause,
    /// Pause if playing (does nothing when already paused).
    Pause,
    /// Resume if paused (does nothing when already playing).
    Resume,
    Next,
    Prev,
    Stop,
    SeekBy(f64),
    /// Seek to an absolute position in the current track, in seconds.
    SeekTo(f64),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub state: PlayState,
    pub track: Option<Track>,
    pub pos: f64,
    pub dur: Option<f64>,
    pub source: Option<SourceInfo>,
    pub output: Option<OutputInfo>,
    pub queue_pos: usize,
    pub queue_len: usize,
    /// The next tracks in the queue (up to two), for "up next".
    pub next: Vec<Track>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Changes whenever the queue does, so the UI knows to fetch it again.
    pub queue_version: u64,
    /// Counts tracks started, so a track repeating can be told from one
    /// carrying on.
    pub started: u64,
    /// Paused on a restored queue that hasn't been played yet this run (so
    /// it isn't reported to Plex as a session).
    pub waiting: bool,
    pub error: Option<String>,
    /// The volume of Plext's stream in desktop-sound mode (kept up to date
    /// by the volume thread); None in bit-perfect mode or when unknown.
    pub volume: Option<crate::pulse::StreamVolume>,
    /// Increases with every new error, so the UI can tell a new one apart.
    pub error_seq: u64,
    /// Increases whenever the position jumps (a seek, or restarting the
    /// track), so observers can tell a jump from normal progress.
    pub seek_seq: u64,
}

impl Status {
    /// True when every bit of the source reaches the DAC unchanged.
    pub fn bit_perfect(&self) -> bool {
        match (&self.source, &self.output) {
            (Some(s), Some(o)) => {
                s.lossless && o.direct() && o.rate == s.rate && o.fmt.bits() >= s.bits
            }
            _ => false,
        }
    }

    /// Whether "next" leads anywhere.
    pub fn has_next(&self) -> bool {
        self.queue_pos + 1 < self.queue_len || (self.repeat == Repeat::All && self.queue_len > 0)
    }
}

/// A copy of the queue for display.
#[derive(Debug, Clone, Default)]
pub struct QueueView {
    pub items: Vec<Entry>,
    pub pos: usize,
    pub version: u64,
}

pub struct Player {
    tx: Sender<Cmd>,
    status: Arc<Mutex<Status>>,
    queue: Arc<Mutex<Queue>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Player {
    pub fn spawn(server: Server, target: Target) -> Player {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let queue = Arc::new(Mutex::new(Queue::new()));
        let st = status.clone();
        let q = queue.clone();
        let handle = thread::Builder::new()
            .name("plext-player".into())
            .spawn(move || {
                Engine {
                    rx,
                    status: st,
                    queue: q,
                    server,
                    target,
                    out: None,
                    parked: None,
                }
                .run()
            })
            .expect("failed to start player thread");
        Player {
            tx,
            status,
            queue,
            handle: Some(handle),
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// A sender for other threads that control playback (e.g. MPRIS).
    pub fn commander(&self) -> Sender<Cmd> {
        self.tx.clone()
    }

    /// Shared handle to the live status (used by the Plex progress reporter).
    pub fn status_handle(&self) -> Arc<Mutex<Status>> {
        self.status.clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// The queue as it is now.
    pub fn queue(&self) -> QueueView {
        let q = self.queue.lock().unwrap();
        QueueView { items: q.items().to_vec(), pos: q.pos(), version: q.version }
    }

    /// A handle for saving the queue from another thread (when the terminal
    /// goes away, the UI thread may be stuck).
    pub fn queue_handle(&self) -> QueueHandle {
        QueueHandle { status: self.status.clone(), queue: self.queue.clone() }
    }
}

/// See `Player::queue_handle`.
#[derive(Clone)]
pub struct QueueHandle {
    status: Arc<Mutex<Status>>,
    queue: Arc<Mutex<Queue>>,
}

impl QueueHandle {
    /// The queue to save when quitting, with how far into the current track
    /// playback got. None when the queue is empty.
    pub fn saved(&self, machine_id: &str) -> Option<Saved> {
        let st = self.status.lock().unwrap().clone();
        let q = self.queue.lock().unwrap();
        let current = q.current()?.track.key.clone();
        let here = matches!(st.state, PlayState::Playing | PlayState::Paused)
            && st.track.as_ref().map(|t| t.key == current).unwrap_or(false);
        Some(q.to_saved(machine_id, if here { st.pos } else { 0.0 }))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(h) = self.handle.take() {
            // Normally it stops at once. If it's stuck waiting on the server
            // (retrying a download that won't start), don't hold up quitting:
            // the process is exiting anyway.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}


enum End {
    Finished,
    Next,
    Prev,
    Restart,
    /// The queue's current track changed under us (jumped to, or removed):
    /// play whatever is current now.
    Reload,
    Stop,
    /// A problem with this track only: skip to the next.
    Skip(String),
    /// A problem with the output device: stop the queue.
    Fatal(String),
    Command(Cmd),
}

enum Between {
    /// Nothing changed: carry on.
    Carry,
    /// The current track changed: start it from the top.
    Moved,
    /// Stop playing, handing back a command that interrupted, if any.
    Stop(Option<Cmd>),
}

struct Engine {
    rx: Receiver<Cmd>,
    status: Arc<Mutex<Status>>,
    queue: Arc<Mutex<Queue>>,
    server: Server,
    target: Target,
    out: Option<Output>,
    /// While idle: paused at this position in the current track, without
    /// holding the output (a restored queue waits like this).
    parked: Option<f64>,
}

impl Engine {
    fn run(mut self) {
        let mut pending: Option<Cmd> = None;
        loop {
            let cmd = match pending.take() {
                Some(c) => c,
                None => match self.rx.recv() {
                    Ok(c) => c,
                    Err(_) => return,
                },
            };
            match cmd {
                Cmd::Quit => return,
                Cmd::Play { queue, index } => {
                    self.with_queue(|q| q.replace(queue, index));
                    self.parked = None;
                    pending = self.play(0.0);
                }
                Cmd::TogglePause | Cmd::Resume => {
                    if !self.queue.lock().unwrap().is_empty() {
                        let from = self.parked.take().unwrap_or(0.0);
                        pending = self.play(from);
                    }
                }
                Cmd::Jump(id) => {
                    if self.with_queue(|q| q.jump(id)) {
                        self.parked = None;
                        pending = self.play(0.0);
                    }
                }
                Cmd::Next => {
                    self.with_queue(|q| {
                        q.next();
                    });
                    self.parked = self.parked.map(|_| 0.0);
                }
                Cmd::Prev => {
                    match self.parked {
                        Some(p) if p > 3.0 => {}
                        _ => self.with_queue(|q| q.prev()),
                    }
                    self.parked = self.parked.map(|_| 0.0);
                }
                Cmd::SeekBy(d) => self.parked = self.parked.map(|p| (p + d).max(0.0)),
                Cmd::SeekTo(t) => self.parked = self.parked.map(|_| t.max(0.0)),
                Cmd::Stop => self.parked = None,
                Cmd::Pause => {}
                Cmd::Restore(saved) => {
                    let position = saved.position;
                    *self.queue.lock().unwrap() = Queue::from_saved(saved);
                    self.parked = Some(position.max(0.0));
                }
                edit => {
                    if let Ok(true) = self.edit(edit) {
                        // The current track went; a paused queue waits at the next one's start.
                        self.parked = self.parked.map(|_| 0.0);
                    }
                }
            }
            self.publish();
            self.show_idle();
        }
    }

    /// Applies a queue edit. Ok(true) when it removed the current track;
    /// Err gives back a command that isn't an edit.
    fn edit(&mut self, cmd: Cmd) -> Result<bool, Cmd> {
        if !is_edit(&cmd) {
            return Err(cmd);
        }
        let gone = apply_edit(&self.queue, cmd);
        self.publish();
        Ok(gone)
    }

    /// Changes the queue and tells everyone.
    fn with_queue<R>(&self, f: impl FnOnce(&mut Queue) -> R) -> R {
        let r = f(&mut self.queue.lock().unwrap());
        self.publish();
        r
    }

    fn publish(&self) {
        publish(&self.queue, &self.status);
    }

    /// The status while nothing is playing: stopped, or paused on the
    /// current track when parked.
    fn show_idle(&self) {
        let current = self.queue.lock().unwrap().current().map(|e| e.track.clone());
        let mut s = self.status.lock().unwrap();
        s.output = None;
        match (self.parked, current) {
            (Some(p), Some(t)) => {
                s.state = PlayState::Paused;
                s.waiting = true;
                s.dur = t.duration_ms.map(|d| d as f64 / 1000.0);
                if s.track.as_ref().map(|x| x.key != t.key).unwrap_or(true) {
                    s.source = None;
                }
                s.track = Some(t);
                s.pos = p;
            }
            _ => {
                s.state = PlayState::Stopped;
                s.pos = 0.0;
                s.waiting = false;
            }
        }
    }

    /// Shows an error in the UI and records it in the log file.
    fn fail(&self, msg: String) {
        crate::log::write(&format!("error: {}", msg));
        let mut s = self.status.lock().unwrap();
        s.error = Some(msg);
        s.error_seq += 1;
    }

    fn set<F: FnOnce(&mut Status)>(&self, f: F) {
        f(&mut self.status.lock().unwrap());
    }

    fn start_download(&self, track: &Track) -> Result<Download, String> {
        // Listings sometimes leave out the media file; ask for the track's details then.
        let looked_up;
        let track = if track.part_key.is_none() {
            looked_up = self.server.track(&track.key).unwrap_or_else(|_| track.clone());
            &looked_up
        } else {
            track
        };
        let part = track
            .part_key
            .clone()
            .ok_or_else(|| format!("\"{}\" has no media file", track.title))?;
        let ext = track.container.clone().unwrap_or_default();
        let server = self.server.clone();
        Download::start(&ext, move |off| server.open_media(&part, off)).map_err(|e| e.to_string())
    }

    /// Plays the queue from its current track, starting `start_at` seconds in.
    /// Returns a command that interrupted it, if any.
    fn play(&mut self, start_at: f64) -> Option<Cmd> {
        let mut prefetch: Option<(u64, Download)> = None;
        let mut start = start_at;
        // Tracks that failed in a row, so a queue where nothing plays (with
        // repeat on) can't spin forever.
        let mut failures = 0usize;
        let pending = loop {
            // Keys pressed between tracks (say, while one track after another
            // fails to load) are handled before starting the next.
            match self.between_tracks() {
                Between::Carry => {}
                Between::Moved => start = 0.0,
                Between::Stop(c) => break c,
            }
            let entry = self.queue.lock().unwrap().current().cloned();
            let Some(entry) = entry else {
                break None;
            };
            let len = self.queue.lock().unwrap().len();
            let dl = match prefetch.take() {
                Some((id, d)) if id == entry.id => d,
                _ => match self.start_download(&entry.track) {
                    Ok(d) => d,
                    Err(e) => {
                        self.fail(e);
                        failures += 1;
                        if failures >= len || !self.with_queue(|q| q.next()) {
                            break None;
                        }
                        start = 0.0;
                        continue;
                    }
                },
            };
            let end = self.play_track(&entry, dl, &mut prefetch, start);
            start = 0.0;
            match end {
                End::Finished => {
                    failures = 0;
                    if !self.with_queue(|q| q.advance_finished()) {
                        // Natural end of the queue: let the last notes ring out.
                        if let Some(o) = self.out.as_mut() {
                            o.drain();
                        }
                        break None;
                    }
                }
                End::Next => {
                    self.flush();
                    if !self.with_queue(|q| q.next()) {
                        break None;
                    }
                }
                End::Prev => {
                    self.flush();
                    self.with_queue(|q| q.prev());
                }
                End::Restart => {
                    self.flush();
                    self.set(|s| s.seek_seq += 1);
                }
                End::Reload => self.flush(),
                End::Skip(e) => {
                    self.flush();
                    self.fail(format!("{} (skipped to the next track)", e));
                    failures += 1;
                    if failures >= len || !self.with_queue(|q| q.next()) {
                        break None;
                    }
                }
                End::Fatal(e) => {
                    self.fail(e);
                    break None;
                }
                End::Stop => {
                    self.flush();
                    break None;
                }
                End::Command(c) => {
                    self.flush();
                    break Some(c);
                }
            }
        };
        // Release the DAC so other programs can use it while we're idle.
        self.out = None;
        self.set(|s| {
            s.state = PlayState::Stopped;
            s.output = None;
            s.pos = 0.0;
        });
        pending
    }

    /// Handles commands waiting between two tracks.
    fn between_tracks(&mut self) -> Between {
        let mut moved = false;
        loop {
            let cmd = match self.rx.try_recv() {
                Ok(c) => c,
                Err(TryRecvError::Empty) => return if moved { Between::Moved } else { Between::Carry },
                Err(TryRecvError::Disconnected) => return Between::Stop(Some(Cmd::Quit)),
            };
            match cmd {
                Cmd::Quit | Cmd::Play { .. } | Cmd::Restore(_) => return Between::Stop(Some(cmd)),
                Cmd::Stop => return Between::Stop(None),
                Cmd::Next => {
                    if !self.with_queue(|q| q.next()) {
                        return Between::Stop(None);
                    }
                    moved = true;
                }
                Cmd::Prev => {
                    self.with_queue(|q| q.prev());
                    moved = true;
                }
                Cmd::Jump(id) => moved |= self.with_queue(|q| q.jump(id)),
                c if is_edit(&c) => {
                    moved |= apply_edit(&self.queue, c);
                    self.publish();
                }
                // Pausing or seeking applies to a track that's playing.
                _ => {}
            }
        }
    }

    fn flush(&mut self) {
        if let Some(o) = self.out.as_mut() {
            o.flush();
        }
    }

    fn play_track(
        &mut self,
        entry: &Entry,
        dl: Download,
        prefetch: &mut Option<(u64, Download)>,
        start_at: f64,
    ) -> End {
        let track = &entry.track;
        self.set(|s| {
            s.state = PlayState::Loading;
            s.waiting = false;
            s.started += 1;
            s.track = Some(track.clone());
            s.pos = start_at;
            s.dur = track.duration_ms.map(|d| d as f64 / 1000.0);
        });
        self.publish();

        let reader = match dl.reader() {
            Ok(r) => r,
            Err(e) => return End::Skip(e.to_string()),
        };
        let mut dec = match Decoder::open(Box::new(reader), track.container.as_deref()) {
            Ok(d) => d,
            Err(e) => return End::Skip(format!("{}: {}", track.title, e)),
        };
        let spec = StreamSpec {
            rate: dec.info.rate,
            channels: dec.info.channels,
            bits: dec.info.bits,
        };

        // Reconfigure the DAC only when the format changes; otherwise keep
        // writing into the same stream for gapless playback.
        if self.out.as_ref().map(|o| o.spec()) != Some(spec) {
            if let Some(mut o) = self.out.take() {
                o.drain();
            }
            match Output::open(&self.target, spec) {
                Ok(o) => self.out = Some(o),
                Err(e) => return output_failed(track, e),
            }
        }
        let out = self.out.as_mut().unwrap();

        let dur = dec
            .info
            .duration_secs
            .or(track.duration_ms.map(|d| d as f64 / 1000.0));
        let rate = spec.rate as f64;
        let channels = spec.channels as usize;
        let mut base = 0.0_f64; // position (s) where `frames` started counting
        if start_at > 0.0 {
            // Picking up where a restored queue left off.
            base = dec.seek(start_at).unwrap_or(0.0);
        }
        {
            let mut s = self.status.lock().unwrap();
            s.source = Some(dec.info.clone());
            s.output = Some(out.info().clone());
            s.dur = dur;
            s.pos = base;
            s.state = PlayState::Playing;
        }
        crate::log::write(&format!(
            "playing \"{}\" — {} ({} {}-bit/{} Hz)",
            track.title, track.artist, dec.info.codec, dec.info.bits, dec.info.rate
        ));

        let mut frames: u64 = 0;
        let mut paused = false;
        let mut resume_from: Option<f64> = None;
        let pos_of = |base: f64, frames: u64, out: &Output| -> f64 {
            base + (frames as i64 - out.delay()).max(0) as f64 / rate
        };

        loop {
            // Handle commands; while paused, wait for them.
            loop {
                let cmd = if paused {
                    match self.rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(c) => c,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => return End::Command(Cmd::Quit),
                    }
                } else {
                    match self.rx.try_recv() {
                        Ok(c) => c,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return End::Command(Cmd::Quit),
                    }
                };
                match cmd {
                    Cmd::TogglePause | Cmd::Pause | Cmd::Resume => {
                        let want_paused = match cmd {
                            Cmd::Pause => true,
                            Cmd::Resume => false,
                            _ => !paused,
                        };
                        if want_paused == paused {
                            continue;
                        }
                        paused = want_paused;
                        if paused {
                            let heard = pos_of(base, frames, out);
                            if !out.set_paused(true) {
                                // The DAC can't pause, so its buffer was discarded.
                                resume_from = Some(heard);
                            }
                            let mut s = self.status.lock().unwrap();
                            s.state = PlayState::Paused;
                            s.pos = heard;
                        } else {
                            out.set_paused(false);
                            if let Some(p) = resume_from.take() {
                                base = dec.seek(p).unwrap_or(p);
                                frames = 0;
                            }
                            self.status.lock().unwrap().state = PlayState::Playing;
                            break;
                        }
                    }
                    Cmd::Next => return End::Next,
                    Cmd::Prev => {
                        if pos_of(base, frames, out) > 3.0 {
                            return End::Restart;
                        }
                        return End::Prev;
                    }
                    Cmd::Stop => return End::Stop,
                    Cmd::SeekBy(_) | Cmd::SeekTo(_) => {
                        let cur = pos_of(base, frames, out);
                        let mut target = match cmd {
                            Cmd::SeekBy(d) => cur + d,
                            Cmd::SeekTo(t) => t,
                            _ => unreachable!(),
                        }
                        .max(0.0);
                        if let Some(dur) = dur {
                            target = target.min((dur - 1.0).max(0.0));
                        }
                        out.flush();
                        resume_from = None;
                        if paused {
                            paused = false;
                            self.status.lock().unwrap().state = PlayState::Playing;
                        }
                        match dec.seek(target) {
                            Ok(p) => base = p,
                            Err(_) => base = cur,
                        }
                        frames = 0;
                        let mut s = self.status.lock().unwrap();
                        s.pos = base;
                        s.seek_seq += 1;
                        break;
                    }
                    Cmd::Jump(id) => {
                        let current = entry.id;
                        if id != current {
                            let mut q = self.queue.lock().unwrap();
                            if q.jump(id) {
                                drop(q);
                                publish(&self.queue, &self.status);
                                return End::Reload;
                            }
                        }
                    }
                    // Already playing; a stray restore is ignored.
                    Cmd::Restore(_) => {}
                    c @ (Cmd::Play { .. } | Cmd::Quit) => return End::Command(c),
                    edit => {
                        let gone = apply_edit(&self.queue, edit);
                        publish(&self.queue, &self.status);
                        if gone {
                            return End::Reload;
                        }
                    }
                }
            }

            // Fetch the next track once this one is fully downloaded (again
            // if the queue changed what's next).
            if dl.is_complete() {
                let next = self.queue.lock().unwrap().peek_finished().cloned();
                let want = next.as_ref().map(|e| e.id);
                if prefetch.as_ref().map(|(id, _)| *id) != want {
                    *prefetch = None;
                    if let Some(n) = next.filter(|n| n.track.part_key.is_some()) {
                        let part = n.track.part_key.clone().unwrap();
                        let ext = n.track.container.clone().unwrap_or_default();
                        let server = self.server.clone();
                        if let Ok(d) = Download::start(&ext, move |off| server.open_media(&part, off)) {
                            *prefetch = Some((n.id, d));
                        }
                    }
                }
            }

            match dec.next_chunk() {
                Ok(Some(samples)) => {
                    let n = samples.len() / channels;
                    if let Err(e) = out.write(samples) {
                        return End::Fatal(e.to_string());
                    }
                    frames += n as u64;
                    self.status.lock().unwrap().pos = pos_of(base, frames, out);
                }
                Ok(None) => {
                    let pos = pos_of(base, frames, out);
                    if dec.skipped > 0 {
                        crate::log::write(&format!(
                            "\"{}\": skipped {} damaged frame(s) (last: {})",
                            track.title,
                            dec.skipped,
                            dec.last_problem.as_deref().unwrap_or("?")
                        ));
                    }
                    // Ending well before the expected length means data went missing.
                    let decoded = base + frames as f64 / rate;
                    if let Some(d) = dur {
                        if decoded + 3.0 < d {
                            crate::log::write(&format!(
                                "\"{}\" ended early: decoded {:.1}s of {:.1}s (heard to {:.1}s)",
                                track.title, decoded, d, pos
                            ));
                        }
                    }
                    return End::Finished;
                }
                Err(e) => return End::Skip(format!("{}: {}", track.title, e)),
            }
        }
    }
}

/// What to do when the output won't open for `track`: a format the output
/// can't play (say 96 kHz on a DAC that tops out at 48) skips this track,
/// as the rest may well play; anything else (the device busy or gone)
/// stops the queue.
fn output_failed(track: &Track, e: anyhow::Error) -> End {
    if e.downcast_ref::<crate::output::Unsupported>().is_some() {
        End::Skip(format!("\"{}\": {}", track.title, e))
    } else {
        End::Fatal(e.to_string())
    }
}

/// Copies the queue's position, length, up-next and modes into the status.
fn publish(queue: &Mutex<Queue>, status: &Mutex<Status>) {
    let (pos, len, next, shuffle, repeat, version) = {
        let q = queue.lock().unwrap();
        (
            q.pos(),
            q.len(),
            q.upcoming(2).into_iter().map(|e| e.track.clone()).collect(),
            q.shuffle(),
            q.repeat(),
            q.version,
        )
    };
    let mut s = status.lock().unwrap();
    s.queue_pos = pos;
    s.queue_len = len;
    s.next = next;
    s.shuffle = shuffle;
    s.repeat = repeat;
    s.queue_version = version;
}

fn is_edit(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Append(_)
            | Cmd::AppendMixed(_)
            | Cmd::PlayNext(_)
            | Cmd::Remove(_)
            | Cmd::Move { .. }
            | Cmd::ClearUpcoming
            | Cmd::ToggleShuffle
            | Cmd::SetShuffle(_)
            | Cmd::CycleRepeat
            | Cmd::SetRepeat(_)
    )
}

/// Applies a queue edit to the shared queue. Returns true when it removed
/// the track that was current. Commands that aren't edits are ignored.
fn apply_edit(queue: &Arc<Mutex<Queue>>, cmd: Cmd) -> bool {
    let mut q = queue.lock().unwrap();
    match cmd {
        Cmd::Append(t) => {
            q.append(t);
        }
        Cmd::PlayNext(t) => {
            q.play_next(t);
        }
        Cmd::AppendMixed(t) => {
            q.append_mixed(t);
        }
        Cmd::Remove(id) => return q.remove(id),
        Cmd::Move { id, by } => q.move_by(id, by),
        Cmd::ClearUpcoming => q.clear_upcoming(),
        Cmd::ToggleShuffle => {
            let on = !q.shuffle();
            q.set_shuffle(on);
        }
        Cmd::SetShuffle(on) => q.set_shuffle(on),
        Cmd::CycleRepeat => {
            let r = q.repeat().cycled();
            q.set_repeat(r);
        }
        Cmd::SetRepeat(r) => q.set_repeat(r),
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    //! The engine against a small local web server standing in for Plex,
    //! playing to ALSA's "null" device. Tracks are short bursts of noise
    //! served slowly, so each takes a moment to play.

    use super::*;
    use crate::plex::Plex;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::time::Instant;

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// Serves `files` (path → bytes) a few KB at a time.
    fn serve(files: HashMap<String, Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let files = Arc::new(files);
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let files = files.clone();
                thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = r.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let mut w = conn;
                    match files.get(&path) {
                        Some(body) => {
                            let _ = write!(w, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            for chunk in body.chunks(4096) {
                                if w.write_all(chunk).is_err() {
                                    return;
                                }
                                thread::sleep(Duration::from_millis(8));
                            }
                        }
                        None => {
                            let _ = write!(w, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                    }
                });
            }
        });
        format!("http://{}", addr)
    }

    fn track_for_test(key: &str) -> Track {
        Track {
            key: key.to_string(),
            title: key.to_string(),
            album_key: None,
            artist: "Test".into(),
            album: "Test".into(),
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

    /// Three short FLAC tracks, "a" "b" "c", served locally; and a player.
    fn setup() -> Option<(Player, Vec<Track>)> {
        if !have_ffmpeg() {
            eprintln!("skipping: ffmpeg not installed");
            return None;
        }
        if crate::output::AlsaOutput::open("null", StreamSpec { rate: 44_100, channels: 2, bits: 16 }).is_err() {
            eprintln!("skipping: ALSA null device unavailable");
            return None;
        }
        let mut files = HashMap::new();
        let mut tracks = Vec::new();
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("plext-q-{}-{}-{}.flac", std::process::id(), n, key));
            let out = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("anoisesrc=d=0.4:c=white:r=44100:seed={}", i + 1))
                .args(["-ac", "2", "-sample_fmt", "s16", "-c:a", "flac"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let part = format!("/library/parts/{}/file.flac", key);
            files.insert(part.clone(), std::fs::read(&path).unwrap());
            let _ = std::fs::remove_file(&path);
            tracks.push(Track {
                key: key.to_string(),
                title: key.to_string(),
                album_key: None,
                artist: "Test".into(),
                album: "Test".into(),
                index: Some(i as u32 + 1),
                disc: None,
                duration_ms: Some(400),
                codec: Some("flac".into()),
                bit_depth: Some(16),
                sample_rate: Some(44_100),
                channels: Some(2),
                bitrate_kbps: None,
                part_key: Some(part),
                container: Some("flac".into()),
                rating: None,
                thumb: None,
            });
        }
        let base = serve(files);
        let server = Server::new(Plex::new("plext-test"), &base, "token", "machine");
        Some((Player::spawn(server, Target::Alsa("null".into())), tracks))
    }

    /// Sends `start`, then records each track as it starts until the player
    /// stops (or `limit` tracks have started), sending `during` once the
    /// first has begun.
    fn watch(p: &Player, limit: usize, start: Cmd, during: impl FnOnce(&Player)) -> Vec<String> {
        let mut seen = Vec::new();
        let mut started = p.status().started;
        p.send(start);
        let mut during = Some(during);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut was_active = false;
        while Instant::now() < deadline {
            let st = p.status();
            if st.started != started {
                started = st.started;
                seen.push(st.track.as_ref().map(|t| t.key.clone()).unwrap_or_default());
                if let Some(f) = during.take() {
                    f(p);
                }
                if seen.len() >= limit {
                    break;
                }
            }
            let active = st.state != PlayState::Stopped;
            if was_active && !active {
                break;
            }
            was_active |= active;
            thread::sleep(Duration::from_millis(2));
        }
        seen
    }

    fn id_of(p: &Player, key: &str) -> u64 {
        p.queue().items.iter().find(|e| e.track.key == key).unwrap().id
    }

    #[test]
    fn a_format_the_output_cant_play_skips_the_track_not_the_queue() {
        let t = Track { title: "Hi-res".into(), ..track_for_test("a") };
        let too_many = crate::output::Output::open(
            &Target::SoundServer { sink: None },
            StreamSpec { rate: 96_000, channels: 40, bits: 24 },
        )
        .err()
        .expect("40 channels never open");
        match output_failed(&t, too_many) {
            End::Skip(why) => assert!(why.contains("Hi-res") && why.contains("40 channels"), "{}", why),
            _ => panic!("an unplayable format should skip the track"),
        }
        assert!(matches!(output_failed(&t, anyhow::anyhow!("hw:1,0 is busy")), End::Fatal(_)), "a busy device stops the queue");
    }

    #[test]
    fn plays_the_queue_in_order_then_stops_at_the_start() {
        let Some((p, tracks)) = setup() else { return };
        assert_eq!(watch(&p, 10, Cmd::Play { queue: tracks, index: 0 }, |_| {}), ["a", "b", "c"]);
        let st = p.status();
        assert_eq!(st.state, PlayState::Stopped);
        assert_eq!(st.queue_pos, 0, "ready to play the queue again");
        assert_eq!(st.queue_len, 3);
    }

    #[test]
    fn tracks_added_while_playing_are_played() {
        let Some((p, tracks)) = setup() else { return };
        let seen = watch(&p, 10, Cmd::Play { queue: vec![tracks[0].clone()], index: 0 }, |p| {
            p.send(Cmd::Append(vec![tracks[2].clone()]));
            p.send(Cmd::PlayNext(vec![tracks[1].clone()]));
        });
        assert_eq!(seen, ["a", "b", "c"]);
    }

    #[test]
    fn removing_the_upcoming_track_skips_it() {
        let Some((p, tracks)) = setup() else { return };
        let seen = watch(&p, 10, Cmd::Play { queue: tracks, index: 0 }, |p| p.send(Cmd::Remove(id_of(p, "b"))));
        assert_eq!(seen, ["a", "c"]);
    }

    #[test]
    fn jumping_plays_that_track_and_carries_on_from_there() {
        let Some((p, tracks)) = setup() else { return };
        let seen = watch(&p, 10, Cmd::Play { queue: tracks, index: 0 }, |p| p.send(Cmd::Jump(id_of(p, "c"))));
        assert_eq!(seen, ["a", "c"]);
    }

    #[test]
    fn repeat_one_plays_the_track_again_and_repeat_all_starts_over() {
        let Some((p, tracks)) = setup() else { return };
        p.send(Cmd::SetRepeat(Repeat::One));
        assert_eq!(watch(&p, 3, Cmd::Play { queue: tracks.clone(), index: 1 }, |_| {}), ["b", "b", "b"]);
        assert_eq!(watch(&p, 4, Cmd::SetRepeat(Repeat::All), |_| {}), ["c", "a", "b", "c"]);
        p.send(Cmd::Stop);
    }

    #[test]
    fn a_restored_queue_waits_paused_then_resumes_there() {
        let Some((p, tracks)) = setup() else { return };
        let mut q = Queue::new();
        q.replace(tracks, 1);
        p.send(Cmd::Restore(q.to_saved("machine", 0.2)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while p.status().state != PlayState::Paused && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let st = p.status();
        assert_eq!(st.state, PlayState::Paused);
        assert_eq!(st.track.as_ref().map(|t| t.key.as_str()), Some("b"));
        assert!((st.pos - 0.2).abs() < 1e-9);
        assert!(st.output.is_none(), "a waiting queue mustn't hold the DAC");
        // The saved queue round-trips through the player too.
        let saved = p.queue_handle().saved("machine").unwrap();
        assert_eq!(saved.pos, 1);
        assert_eq!(watch(&p, 10, Cmd::TogglePause, |_| {}), ["b", "c"]);
    }
}
