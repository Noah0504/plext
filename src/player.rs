//! Playback engine: runs on its own thread, owns the DAC, and plays a queue of
//! tracks. Consecutive tracks with the same format are played gaplessly.

use crate::decode::{Decoder, SourceInfo};
use crate::download::Download;
use crate::output::{Output, OutputInfo, StreamSpec};
use crate::plex::{Server, Track};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub enum Cmd {
    Play { queue: Vec<Track>, index: usize },
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

#[derive(Debug, Clone, Default)]
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
    pub error: Option<String>,
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
                s.lossless && o.direct && o.rate == s.rate && o.fmt.bits() >= s.bits
            }
            _ => false,
        }
    }
}

pub struct Player {
    tx: Sender<Cmd>,
    status: Arc<Mutex<Status>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Player {
    pub fn spawn(server: Server, device: String) -> Player {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let st = status.clone();
        let handle = thread::Builder::new()
            .name("plext-player".into())
            .spawn(move || {
                Engine {
                    rx,
                    status: st,
                    server,
                    device,
                    out: None,
                }
                .run()
            })
            .expect("failed to start player thread");
        Player {
            tx,
            status,
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
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

enum End {
    Finished,
    Next,
    Prev,
    Restart,
    Stop,
    /// A problem with this track only: skip to the next.
    Skip(String),
    /// A problem with the output device: stop the queue.
    Fatal(String),
    Command(Cmd),
}

struct Engine {
    rx: Receiver<Cmd>,
    status: Arc<Mutex<Status>>,
    server: Server,
    device: String,
    out: Option<Output>,
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
                Cmd::Play { queue, index } => pending = self.play_queue(queue, index),
                Cmd::Quit => return,
                _ => {} // nothing playing
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
        let part = track
            .part_key
            .clone()
            .ok_or_else(|| format!("\"{}\" has no media file", track.title))?;
        let ext = track.container.clone().unwrap_or_default();
        let server = self.server.clone();
        Download::start(&ext, move |off| server.open_media(&part, off)).map_err(|e| e.to_string())
    }

    /// Plays the queue from `idx`. Returns a command that interrupted it, if any.
    fn play_queue(&mut self, queue: Vec<Track>, mut idx: usize) -> Option<Cmd> {
        let mut prefetch: Option<(usize, Download)> = None;
        let pending = loop {
            if idx >= queue.len() {
                // Natural end of the queue: let the last notes ring out.
                if let Some(o) = self.out.as_mut() {
                    o.drain();
                }
                break None;
            }
            let dl = match prefetch.take() {
                Some((i, d)) if i == idx => d,
                _ => match self.start_download(&queue[idx]) {
                    Ok(d) => d,
                    Err(e) => {
                        self.fail(e);
                        idx += 1;
                        continue;
                    }
                },
            };
            let end = self.play_track(&queue, idx, dl, &mut prefetch);
            match end {
                End::Finished => idx += 1,
                End::Next => {
                    self.flush();
                    idx += 1;
                }
                End::Prev => {
                    self.flush();
                    idx = idx.saturating_sub(1);
                }
                End::Restart => {
                    self.flush();
                    self.set(|s| s.seek_seq += 1);
                }
                End::Skip(e) => {
                    self.flush();
                    self.fail(format!("{} (skipped to the next track)", e));
                    idx += 1;
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

    fn flush(&mut self) {
        if let Some(o) = self.out.as_mut() {
            o.flush();
        }
    }

    fn play_track(
        &mut self,
        queue: &[Track],
        idx: usize,
        dl: Download,
        prefetch: &mut Option<(usize, Download)>,
    ) -> End {
        let track = &queue[idx];
        self.set(|s| {
            s.state = PlayState::Loading;
            s.track = Some(track.clone());
            s.pos = 0.0;
            s.dur = track.duration_ms.map(|d| d as f64 / 1000.0);
            s.queue_pos = idx;
            s.queue_len = queue.len();
            s.next = queue[idx + 1..].iter().take(2).cloned().collect();
        });

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
        if self.out.as_ref().map(|o| o.spec) != Some(spec) {
            if let Some(mut o) = self.out.take() {
                o.drain();
            }
            match Output::open(&self.device, spec) {
                Ok(o) => self.out = Some(o),
                Err(e) => return End::Fatal(e.to_string()),
            }
        }
        let out = self.out.as_mut().unwrap();

        let dur = dec
            .info
            .duration_secs
            .or(track.duration_ms.map(|d| d as f64 / 1000.0));
        {
            let mut s = self.status.lock().unwrap();
            s.source = Some(dec.info.clone());
            s.output = Some(out.info.clone());
            s.dur = dur;
            s.state = PlayState::Playing;
        }
        crate::log::write(&format!(
            "playing \"{}\" — {} ({} {}-bit/{} Hz)",
            track.title, track.artist, dec.info.codec, dec.info.bits, dec.info.rate
        ));

        let rate = spec.rate as f64;
        let channels = spec.channels as usize;
        let mut base = 0.0_f64; // position (s) where `frames` started counting
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
                    c @ (Cmd::Play { .. } | Cmd::Quit) => return End::Command(c),
                }
            }

            // Start fetching the next track once this one is fully downloaded.
            if prefetch.is_none() && idx + 1 < queue.len() && dl.is_complete() {
                let next = &queue[idx + 1];
                if let Some(part) = next.part_key.clone() {
                    let ext = next.container.clone().unwrap_or_default();
                    let server = self.server.clone();
                    if let Ok(d) = Download::start(&ext, move |off| server.open_media(&part, off)) {
                        *prefetch = Some((idx + 1, d));
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
