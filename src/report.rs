//! Reports playback to the Plex server, the same way official Plex players do.
//!
//! Plex builds its "Now Playing" dashboard, play counts, history and webhooks
//! (media.play / pause / resume / stop / scrobble) from these reports, so this
//! is what makes Plext's listening visible to Plex and to webhook consumers.

use crate::player::{PlayState, Status};
use crate::plex::{PlayQueue, Server, Timeline, Track};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How often to report while playing. Plex clients typically use ~10 s.
const INTERVAL: Duration = Duration::from_secs(10);

pub struct Reporter {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Reporter {
    pub fn spawn(server: Server, status: Arc<Mutex<Status>>, session_id: String) -> Reporter {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = thread::Builder::new()
            .name("plext-report".into())
            .spawn(move || run(server, status, session_id, s))
            .expect("failed to start reporter thread");
        Reporter {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Last {
    track: Track,
    state: PlayState,
    pos: f64,
    sent_at: Instant,
}

fn run(server: Server, status: Arc<Mutex<Status>>, session_id: String, stop: Arc<AtomicBool>) {
    let mut queues: HashMap<String, Option<PlayQueue>> = HashMap::new();
    let mut last: Option<Last> = None;

    let send = |track: &Track, state: &'static str, pos: f64, queues: &HashMap<String, Option<PlayQueue>>| {
        let queue = track
            .album_key
            .as_ref()
            .and_then(|k| queues.get(k))
            .and_then(|q| q.as_ref());
        let t = Timeline {
            track,
            state,
            time_ms: (pos.max(0.0) * 1000.0) as u64,
            queue,
        };
        // Reporting is best-effort: a failure must never interrupt playback.
        let _ = server.report_timeline(&t, &session_id);
    };

    loop {
        let quitting = stop.load(Ordering::Relaxed);
        let st = if quitting {
            Status::default() // report "stopped" for whatever was playing
        } else {
            status.lock().unwrap().clone()
        };

        // A restored queue waiting to be resumed isn't a listening session yet.
        let active = matches!(st.state, PlayState::Playing | PlayState::Paused) && !st.waiting;
        let current = st.track.as_ref().filter(|_| active);

        // The previous track ended, was skipped, or playback stopped.
        if let Some(l) = &last {
            let same = current.map(|t| t.key == l.track.key).unwrap_or(false);
            if !same {
                send(&l.track, "stopped", l.pos, &queues);
                last = None;
            }
        }

        if let Some(track) = current {
            let state = if st.state == PlayState::Paused { "paused" } else { "playing" };
            let due = match &last {
                None => true,
                Some(l) => {
                    l.state != st.state
                        || (st.pos - l.pos).abs() > 3.0 // a seek (pos is tracked every tick)
                        || (st.state == PlayState::Playing && l.sent_at.elapsed() >= INTERVAL)
                }
            };
            if due {
                if let Some(album) = &track.album_key {
                    if !queues.contains_key(album) {
                        let q = server.create_play_queue(album).ok();
                        queues.insert(album.clone(), q);
                    }
                }
                send(track, state, st.pos, &queues);
                last = Some(Last {
                    track: track.clone(),
                    state: st.state,
                    pos: st.pos,
                    sent_at: Instant::now(),
                });
            } else if let Some(l) = last.as_mut() {
                l.pos = st.pos;
            }
        }

        if quitting {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}
