//! Volume in desktop-sound mode. Plext's stream on the sound server has its
//! own volume, the one the desktop's sound settings show next to "Plext".
//! A small thread follows that stream, reads its volume (so changes made
//! in the desktop's mixer show up too) and applies changes asked for with
//! the volume keys or over MPRIS. It talks to the sound server on its own
//! connection, so it never holds up the audio.
//!
//! In bit-perfect mode there's nothing to do: the DAC or amp sets the volume.

use crate::player::Status;
use crate::pulse::{StreamVolume, VolumeControl, MAX_PERCENT};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How often the volume is read, to catch changes made elsewhere.
const POLL: Duration = Duration::from_millis(500);
/// Wait this long before trying the sound server again after a failure.
const RETRY: Duration = Duration::from_secs(3);
/// One press of a volume key.
pub const STEP: u32 = 5;

enum MixCmd {
    /// Up (positive) or down a step at a time.
    Step(i32),
    /// To this percentage.
    Set(u32),
}

/// Changes the volume of Plext's stream; cheap to clone.
#[derive(Clone)]
pub struct Mixer {
    tx: Sender<MixCmd>,
}

impl Mixer {
    /// Starts following the stream the player reports in `status`, and
    /// writes its volume into `status.volume`.
    pub fn spawn(status: Arc<Mutex<Status>>) -> Mixer {
        let (tx, rx) = mpsc::channel();
        let _ = thread::Builder::new().name("plext-volume".into()).spawn(move || run(rx, status));
        Mixer { tx }
    }

    /// `steps` steps up (or down, when negative).
    pub fn step(&self, steps: i32) {
        let _ = self.tx.send(MixCmd::Step(steps));
    }

    pub fn set(&self, percent: u32) {
        let _ = self.tx.send(MixCmd::Set(percent.min(MAX_PERCENT)));
    }
}

#[cfg(test)]
impl Mixer {
    /// A mixer whose requests go nowhere, for UI tests.
    pub fn detached() -> Mixer {
        let (tx, _) = mpsc::channel();
        Mixer { tx }
    }
}

/// The next stop on the 5% scale from `from`, `steps` stops away.
pub fn stepped(from: u32, steps: i32) -> u32 {
    let snapped = if steps > 0 { from / STEP * STEP } else { from.div_ceil(STEP) * STEP };
    (snapped as i64 + steps as i64 * STEP as i64).clamp(0, MAX_PERCENT as i64) as u32
}

fn run(rx: Receiver<MixCmd>, status: Arc<Mutex<Status>>) {
    let mut ctl: Option<VolumeControl> = None;
    let mut failed_at: Option<Instant> = None;
    // The stream we last saw, and the volume asked for this session (kept
    // when a new stream opens, e.g. for a track in another format).
    let mut seen: Option<u32> = None;
    let mut wanted: Option<u32> = None;
    // The stream a failure was last logged for, so a stream that has just
    // closed doesn't fill the log.
    let mut logged: Option<u32> = None;
    loop {
        let cmd = match rx.recv_timeout(POLL) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let stream = status.lock().unwrap().output.as_ref().and_then(|o| o.stream);
        let Some(stream) = stream else {
            seen = None;
            status.lock().unwrap().volume = None;
            continue;
        };
        if ctl.is_none() && failed_at.map(|t| t.elapsed() >= RETRY).unwrap_or(true) {
            match VolumeControl::connect() {
                Ok(c) => ctl = Some(c),
                Err(e) => {
                    failed_at = Some(Instant::now());
                    crate::log::write(&format!("volume: couldn't reach the sound server: {:#}", e));
                }
            }
        }
        let Some(c) = ctl.as_mut() else { continue };

        let result = (|| {
            let mut v = c.get(stream)?;
            if seen != Some(stream) {
                seen = Some(stream);
                if let Some(w) = wanted.filter(|&w| w != v.percent) {
                    v = c.set(stream, w)?;
                }
            }
            match cmd {
                Some(MixCmd::Step(n)) => {
                    let to = stepped(v.percent, n);
                    wanted = Some(to);
                    v = c.set(stream, to)?;
                }
                Some(MixCmd::Set(p)) => {
                    wanted = Some(p);
                    v = c.set(stream, p)?;
                }
                None => {}
            }
            anyhow::Ok(v)
        })();
        match result {
            Ok(v) => set_volume(&status, Some(v)),
            Err(e) => {
                // Usually the stream has just closed (the next track opens a
                // new one); if the connection itself went, make a new one.
                let msg = format!("{:#}", e);
                if logged != Some(stream) {
                    logged = Some(stream);
                    crate::log::write(&format!("volume: {}", msg));
                }
                if msg.contains("connection") || msg.contains("stopped responding") || msg.contains("closed") {
                    ctl = None;
                }
                seen = None;
                set_volume(&status, None);
            }
        }
    }
}

fn set_volume(status: &Mutex<Status>, v: Option<StreamVolume>) {
    let mut s = status.lock().unwrap();
    if s.volume != v {
        s.volume = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real PipeWire or PulseAudio server, when one is running:
    /// the mixer follows Plext's stream, steps and sets its volume (what
    /// the server reports, not just what Plext thinks), sees changes made
    /// elsewhere, and carries the volume over to the next stream.
    #[test]
    fn follows_and_changes_the_streams_volume_on_a_running_sound_server() {
        use crate::output::StreamSpec;
        use crate::pulse::PulseOutput;
        let spec = StreamSpec { rate: 44_100, channels: 2, bits: 16 };
        let out = match PulseOutput::open(None, spec) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("skipping: no sound server: {}", e);
                return;
            }
        };
        let stream = out.info.stream.expect("a sound-server stream has an id");
        let status = Arc::new(Mutex::new(Status { output: Some(out.info.clone()), ..Status::default() }));
        let wait = |want: &dyn Fn(Option<StreamVolume>) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let v = status.lock().unwrap().volume;
                if want(v) || Instant::now() > deadline {
                    return v;
                }
                thread::sleep(Duration::from_millis(20));
            }
        };
        let mut probe = VolumeControl::connect().unwrap();
        probe.set(stream, 50).unwrap();

        let mixer = Mixer::spawn(status.clone());
        assert_eq!(wait(&|v| v.map(|v| v.percent) == Some(50)).map(|v| v.percent), Some(50));
        mixer.step(1);
        assert_eq!(wait(&|v| v.map(|v| v.percent) == Some(55)).map(|v| v.percent), Some(55));
        assert_eq!(probe.get(stream).unwrap().percent, 55, "the server has it");
        mixer.step(-3);
        wait(&|v| v.map(|v| v.percent) == Some(40));
        assert_eq!(probe.get(stream).unwrap().percent, 40);
        mixer.set(150);
        wait(&|v| v.map(|v| v.percent) == Some(100));
        assert_eq!(probe.get(stream).unwrap().percent, 100, "never above full");
        mixer.set(30);
        wait(&|v| v.map(|v| v.percent) == Some(30));

        // Changed in the desktop's mixer: Plext sees it.
        probe.set(stream, 70).unwrap();
        assert_eq!(wait(&|v| v.map(|v| v.percent) == Some(70)).map(|v| v.percent), Some(70));

        // A new stream (the next track in another format) gets the volume
        // last chosen in Plext.
        mixer.set(35);
        wait(&|v| v.map(|v| v.percent) == Some(35));
        let out2 = PulseOutput::open(None, StreamSpec { rate: 48_000, channels: 2, bits: 24 }).unwrap();
        let stream2 = out2.info.stream.unwrap();
        probe.set(stream2, 90).unwrap();
        drop(out);
        status.lock().unwrap().output = Some(out2.info.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while probe.get(stream2).unwrap().percent != 35 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(probe.get(stream2).unwrap().percent, 35);

        // Nothing playing: no volume to show.
        status.lock().unwrap().output = None;
        assert_eq!(wait(&|v| v.is_none()), None);
    }

    #[test]
    fn steps_land_on_the_5_percent_scale() {
        assert_eq!(stepped(50, 1), 55);
        assert_eq!(stepped(50, -1), 45);
        assert_eq!(stepped(67, 1), 70);
        assert_eq!(stepped(67, -1), 65);
        assert_eq!(stepped(98, 1), 100);
        assert_eq!(stepped(100, 1), 100);
        assert_eq!(stepped(3, -1), 0);
        assert_eq!(stepped(0, -1), 0);
        assert_eq!(stepped(40, 3), 55);
    }
}
