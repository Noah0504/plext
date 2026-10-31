//! MPRIS support: makes Plext show up in the desktop's media controls (GNOME's
//! top bar and lock screen, KDE's media applet, `playerctl`, …) and lets the
//! keyboard's play/pause, next and previous keys control it.
//!
//! The D-Bus service runs on its own thread. It reads the player's shared
//! `Status` and sends it `Cmd`s, just like the UI does; it never touches the
//! audio path. When there's no session bus (a bare console, or over SSH) it
//! logs that and quietly does nothing.
//!
//! Spec: https://specifications.freedesktop.org/mpris-spec/latest/

use crate::covers::Covers;
use crate::volume::Mixer;
use crate::player::{Cmd, PlayState, Status};
use crate::queue::Repeat;
use crate::plex::Track;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{blocking, interface};

const BUS_NAME: &str = "org.mpris.MediaPlayer2.plext";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
/// The track id MPRIS reserves for "nothing is playing".
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
/// How often to look for changes to announce.
const POLL: Duration = Duration::from_millis(200);

pub struct Mpris {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Mpris {
    /// `mixer` is there in desktop-sound mode, so the volume can be changed.
    pub fn spawn(status: Arc<Mutex<Status>>, commands: Sender<Cmd>, covers: Option<Covers>, mixer: Option<Mixer>) -> Mpris {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = thread::Builder::new()
            .name("plext-mpris".into())
            .spawn(move || {
                if let Err(e) = run(status, commands, covers, mixer, &s) {
                    crate::log::write(&format!("mpris: media controls unavailable: {}", e));
                }
            })
            .ok();
        Mpris { stop, handle }
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------- what the desktop sees ----------

/// Everything about the player's state that MPRIS announces changes to.
#[derive(Debug, Clone, PartialEq, Default)]
struct View {
    playback: &'static str,
    track_key: Option<String>,
    length_ms: Option<u64>,
    has_art: bool,
    can_next: bool,
    can_prev: bool,
    can_play: bool,
    can_pause: bool,
    can_seek: bool,
    shuffle: bool,
    loop_status: &'static str,
    /// The volume in tenths of a percent, to notice changes.
    volume: u32,
}

fn is_active(st: &Status) -> bool {
    st.track.is_some() && st.state != PlayState::Stopped
}

fn playback_status(st: &Status) -> &'static str {
    match st.state {
        PlayState::Playing | PlayState::Loading => "Playing",
        PlayState::Paused => "Paused",
        PlayState::Stopped => "Stopped",
    }
}

fn length_us(st: &Status) -> Option<i64> {
    st.dur.filter(|d| *d > 0.0).map(|d| (d * 1_000_000.0) as i64)
}

fn position_us(st: &Status) -> i64 {
    if is_active(st) {
        (st.pos.max(0.0) * 1_000_000.0) as i64
    } else {
        0
    }
}

fn view(st: &Status, covers: Option<&Covers>) -> View {
    let active = is_active(st);
    let has_art = st
        .track
        .as_ref()
        .and_then(|t| t.thumb.as_deref())
        .zip(covers)
        .map(|(thumb, c)| c.cached(thumb).is_some())
        .unwrap_or(false);
    View {
        playback: playback_status(st),
        track_key: st.track.as_ref().filter(|_| active).map(|t| t.key.clone()),
        length_ms: length_us(st).map(|u| (u / 1000) as u64),
        has_art,
        can_next: active && st.has_next(),
        // Previous always works while playing: it restarts the track or goes back.
        can_prev: active,
        can_play: active,
        can_pause: active,
        can_seek: active && st.state != PlayState::Loading && st.dur.is_some(),
        shuffle: st.shuffle,
        loop_status: loop_status(st.repeat),
        volume: (volume(st) * 1000.0).round() as u32,
    }
}

/// The volume MPRIS reports: the stream's in desktop-sound mode (0 when
/// muted), full in bit-perfect mode (the DAC or amp sets it there).
fn volume(st: &Status) -> f64 {
    match st.volume {
        Some(v) if v.muted => 0.0,
        Some(v) => v.percent as f64 / 100.0,
        None => 1.0,
    }
}

/// MPRIS names for the repeat modes.
fn loop_status(r: Repeat) -> &'static str {
    match r {
        Repeat::Off => "None",
        Repeat::All => "Playlist",
        Repeat::One => "Track",
    }
}

/// D-Bus object path identifying a track. Plex rating keys are numbers, but
/// keep only characters object paths allow, just in case.
fn track_id(track: &Track) -> OwnedObjectPath {
    let key: String = track
        .key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let key = if key.is_empty() { "unknown".to_string() } else { key };
    OwnedObjectPath::try_from(format!("/plext/track/{}", key))
        .unwrap_or_else(|_| OwnedObjectPath::try_from(NO_TRACK).unwrap())
}

fn owned<'a, V: Into<Value<'a>>>(v: V) -> OwnedValue {
    // Only values holding file descriptors can fail to convert; these never do.
    OwnedValue::try_from(v.into()).expect("plain D-Bus value")
}

fn metadata(st: &Status, covers: Option<&Covers>) -> HashMap<String, OwnedValue> {
    let mut m = HashMap::new();
    let track = match st.track.as_ref().filter(|_| is_active(st)) {
        Some(t) => t,
        None => {
            m.insert("mpris:trackid".into(), owned(ObjectPath::try_from(NO_TRACK).unwrap()));
            return m;
        }
    };
    m.insert("mpris:trackid".into(), owned(track_id(track).into_inner()));
    if let Some(len) = length_us(st) {
        m.insert("mpris:length".into(), owned(len));
    }
    m.insert("xesam:title".into(), owned(track.title.as_str()));
    if !track.artist.is_empty() {
        m.insert("xesam:artist".into(), owned(vec![track.artist.as_str()]));
    }
    if !track.album.is_empty() {
        m.insert("xesam:album".into(), owned(track.album.as_str()));
    }
    if let Some(n) = track.index {
        m.insert("xesam:trackNumber".into(), owned(n as i32));
    }
    if let Some(n) = track.disc {
        m.insert("xesam:discNumber".into(), owned(n as i32));
    }
    if let (Some(thumb), Some(c)) = (track.thumb.as_deref(), covers) {
        if let Some(path) = c.cached(thumb) {
            m.insert("mpris:artUrl".into(), owned(format!("file://{}", path.display())));
        }
    }
    m
}

// ---------- the D-Bus interfaces ----------

struct Root;

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        "Plext".into()
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    status: Arc<Mutex<Status>>,
    commands: Mutex<Sender<Cmd>>,
    covers: Option<Covers>,
    mixer: Option<Mixer>,
}

impl Player {
    fn snapshot(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.commands.lock().unwrap().send(cmd);
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        if view(&self.snapshot(), None).can_next {
            self.send(Cmd::Next);
        }
    }

    fn previous(&self) {
        if is_active(&self.snapshot()) {
            self.send(Cmd::Prev);
        }
    }

    fn pause(&self) {
        if is_active(&self.snapshot()) {
            self.send(Cmd::Pause);
        }
    }

    fn play_pause(&self) {
        if is_active(&self.snapshot()) {
            self.send(Cmd::TogglePause);
        }
    }

    fn stop(&self) {
        if is_active(&self.snapshot()) {
            self.send(Cmd::Stop);
        }
    }

    fn play(&self) {
        if is_active(&self.snapshot()) {
            self.send(Cmd::Resume);
        }
    }

    /// Seeks by `offset` microseconds. Past the end means the next track.
    fn seek(&self, offset: i64) {
        let st = self.snapshot();
        let v = view(&st, None);
        if !v.can_seek {
            return;
        }
        let target = position_us(&st).saturating_add(offset).max(0);
        match length_us(&st) {
            Some(len) if target >= len => {
                if v.can_next {
                    self.send(Cmd::Next);
                }
            }
            _ => self.send(Cmd::SeekTo(target as f64 / 1_000_000.0)),
        }
    }

    /// Jumps to `position` microseconds, if `track_id` is still the current track.
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        let st = self.snapshot();
        if !view(&st, None).can_seek || position < 0 {
            return;
        }
        let current = match st.track.as_ref() {
            Some(t) => self::track_id(t),
            None => return,
        };
        if current.as_str() != track_id.as_str() {
            return;
        }
        if length_us(&st).map(|len| position > len).unwrap_or(false) {
            return;
        }
        self.send(Cmd::SeekTo(position as f64 / 1_000_000.0));
    }

    fn open_uri(&self, _uri: &str) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("Plext plays from your Plex library only".into()))
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        playback_status(&self.snapshot()).into()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    /// Playback speed can't change: that would alter the audio.
    #[zbus(property)]
    fn set_rate(&self, _rate: f64) {}

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        metadata(&self.snapshot(), self.covers.as_ref())
    }

    /// Plext's own volume in desktop-sound mode. In bit-perfect mode it's
    /// always full and can't change: the DAC or amp sets the volume.
    #[zbus(property)]
    fn volume(&self) -> f64 {
        volume(&self.snapshot())
    }

    #[zbus(property)]
    fn set_volume(&self, value: f64) {
        if let Some(m) = &self.mixer {
            if value.is_finite() && self.snapshot().volume.is_some() {
                m.set((value.clamp(0.0, 1.0) * 100.0).round() as u32);
            }
        }
    }

    /// Clients read this when they need it and extrapolate in between, so
    /// changes aren't announced (the spec says not to); jumps get `Seeked`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        position_us(&self.snapshot())
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        view(&self.snapshot(), None).can_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        view(&self.snapshot(), None).can_prev
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        view(&self.snapshot(), None).can_play
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        view(&self.snapshot(), None).can_pause
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        view(&self.snapshot(), None).can_seek
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.snapshot().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&self, on: bool) {
        self.send(Cmd::SetShuffle(on));
    }

    /// "None", "Track" or "Playlist" (the whole queue).
    #[zbus(property)]
    fn loop_status(&self) -> String {
        loop_status(self.snapshot().repeat).to_string()
    }

    #[zbus(property)]
    fn set_loop_status(&self, value: String) -> zbus::fdo::Result<()> {
        let r = match value.as_str() {
            "None" => Repeat::Off,
            "Track" => Repeat::One,
            "Playlist" => Repeat::All,
            other => return Err(zbus::fdo::Error::InvalidArgs(format!("unknown loop status {}", other))),
        };
        self.send(Cmd::SetRepeat(r));
        Ok(())
    }
}

// ---------- the service thread ----------

fn run(
    status: Arc<Mutex<Status>>,
    commands: Sender<Cmd>,
    covers: Option<Covers>,
    mixer: Option<Mixer>,
    stop: &AtomicBool,
) -> zbus::Result<()> {
    let player = Player {
        status: status.clone(),
        commands: Mutex::new(commands),
        covers: covers.clone(),
        mixer,
    };
    let conn = blocking::connection::Builder::session()?
        .serve_at(OBJECT_PATH, Root)?
        .serve_at(OBJECT_PATH, player)?
        .build()?;
    let name = claim_name(&conn)?;
    crate::log::write(&format!("mpris: media controls on as {}", name));

    let iface = conn.object_server().interface::<_, Player>(OBJECT_PATH)?;
    let emitter = iface.signal_emitter().clone();
    let (mut last, mut last_seek) = {
        let st = status.lock().unwrap();
        (view(&st, covers.as_ref()), st.seek_seq)
    };
    let mut requested: Option<String> = None;

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(POLL);
        let st = status.lock().unwrap().clone();

        // Fetch the cover for a new track; it's announced once it has arrived.
        if let (Some(c), Some(t)) = (covers.as_ref(), st.track.as_ref().filter(|_| is_active(&st))) {
            if let Some(thumb) = t.thumb.as_deref() {
                if requested.as_deref() != Some(thumb) {
                    c.request(thumb);
                    requested = Some(thumb.to_string());
                }
            }
        }

        let now = view(&st, covers.as_ref());
        if now != last {
            let p = iface.get();
            if now.playback != last.playback {
                announce(p.playback_status_changed(&emitter));
            }
            if (now.track_key.as_ref(), now.length_ms, now.has_art)
                != (last.track_key.as_ref(), last.length_ms, last.has_art)
            {
                announce(p.metadata_changed(&emitter));
            }
            if now.can_next != last.can_next {
                announce(p.can_go_next_changed(&emitter));
            }
            if now.can_prev != last.can_prev {
                announce(p.can_go_previous_changed(&emitter));
            }
            if now.can_play != last.can_play {
                announce(p.can_play_changed(&emitter));
            }
            if now.can_pause != last.can_pause {
                announce(p.can_pause_changed(&emitter));
            }
            if now.can_seek != last.can_seek {
                announce(p.can_seek_changed(&emitter));
            }
            if now.shuffle != last.shuffle {
                announce(p.shuffle_changed(&emitter));
            }
            if now.loop_status != last.loop_status {
                announce(p.loop_status_changed(&emitter));
            }
            if now.volume != last.volume {
                announce(p.volume_changed(&emitter));
            }
            last = now;
        }
        if st.seek_seq != last_seek {
            last_seek = st.seek_seq;
            announce(Player::seeked(&emitter, position_us(&st)));
        }
    }
    let _ = conn.release_name(name.as_str());
    Ok(())
}

/// Takes `org.mpris.MediaPlayer2.plext`, or a per-process name if another
/// Plext already has it, as the spec suggests for multiple instances.
fn claim_name(conn: &blocking::Connection) -> zbus::Result<String> {
    let flags = RequestNameFlags::DoNotQueue.into();
    match conn.request_name_with_flags(BUS_NAME, flags) {
        Ok(RequestNameReply::PrimaryOwner) | Ok(RequestNameReply::AlreadyOwner) => {
            return Ok(BUS_NAME.to_string())
        }
        Ok(_) | Err(zbus::Error::NameTaken) => {}
        Err(e) => return Err(e),
    }
    let name = format!("{}.instance{}", BUS_NAME, std::process::id());
    conn.request_name_with_flags(name.as_str(), flags)?;
    Ok(name)
}

fn announce<F: std::future::Future<Output = zbus::Result<()>>>(f: F) {
    if let Err(e) = zbus::block_on(f) {
        crate::log::write(&format!("mpris: couldn't send an update: {}", e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            key: "4242".into(),
            title: "Northbound".into(),
            album_key: Some("100".into()),
            artist: "Low Tide Choir".into(),
            album: "Saltmarsh".into(),
            index: Some(5),
            disc: Some(1),
            duration_ms: Some(187_000),
            codec: Some("flac".into()),
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            channels: Some(2),
            bitrate_kbps: Some(636),
            part_key: None,
            container: None,
            rating: None,
            thumb: None,
        }
    }

    fn playing(queue_pos: usize, queue_len: usize) -> Status {
        Status {
            state: PlayState::Playing,
            track: Some(track()),
            pos: 25.5,
            dur: Some(187.0),
            queue_pos,
            queue_len,
            ..Default::default()
        }
    }

    #[test]
    fn stopped_has_no_track_and_no_controls() {
        let st = Status::default();
        let v = view(&st, None);
        assert_eq!(v.playback, "Stopped");
        assert!(!v.can_play && !v.can_pause && !v.can_next && !v.can_prev && !v.can_seek);
        let m = metadata(&st, None);
        assert_eq!(m.len(), 1);
        assert_eq!(position_us(&st), 0);
    }

    #[test]
    fn playing_reports_track_position_and_controls() {
        let st = playing(1, 3);
        let v = view(&st, None);
        assert_eq!(v.playback, "Playing");
        assert!(v.can_next && v.can_prev && v.can_seek && v.can_pause);
        assert_eq!(position_us(&st), 25_500_000);
        let m = metadata(&st, None);
        assert_eq!(m["xesam:title"], owned("Northbound"));
        assert_eq!(m["mpris:length"], owned(187_000_000i64));
        assert_eq!(m["xesam:trackNumber"], owned(5i32));
        assert_eq!(m["mpris:trackid"], owned(track_id(&track()).into_inner()));
        assert_eq!(track_id(&track()).as_str(), "/plext/track/4242");
    }

    #[test]
    fn last_track_cannot_go_next() {
        assert!(!view(&playing(2, 3), None).can_next);
    }

    #[test]
    fn loading_counts_as_playing_but_cannot_seek() {
        let mut st = playing(0, 1);
        st.state = PlayState::Loading;
        let v = view(&st, None);
        assert_eq!(v.playback, "Playing");
        assert!(!v.can_seek);
    }
}

/// A live check against a real session bus, driven from outside (e.g. with
/// `playerctl`). A tiny fake engine answers commands the way the real one
/// does, so state changes flow back out as D-Bus signals. Run with:
///   dbus-run-session -- sh -c 'cargo test mpris_live -- --ignored --nocapture & sleep 3; playerctl …'
#[cfg(test)]
mod live {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn mpris_live() {
        let secs: u64 = std::env::var("MPRIS_LIVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
        let mut queue: Vec<Track> = (1..=3)
            .map(|i| Track {
                key: format!("{}", 4240 + i),
                title: format!("Song {}", i),
                index: Some(i),
                ..tests_track()
            })
            .collect();
        queue[0].title = "Northbound".into();
        let status = Arc::new(Mutex::new(Status {
            state: PlayState::Playing,
            track: Some(queue[0].clone()),
            pos: 0.0,
            dur: Some(187.0),
            queue_pos: 0,
            queue_len: queue.len(),
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel();
        let _mpris = Mpris::spawn(status.clone(), tx, None, None);
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            let cmd = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => c,
                Err(_) => {
                    let mut s = status.lock().unwrap();
                    if s.state == PlayState::Playing {
                        s.pos += 0.1;
                    }
                    continue;
                }
            };
            let mut s = status.lock().unwrap();
            let name = match &cmd {
                Cmd::TogglePause => "TogglePause".to_string(),
                Cmd::Pause => "Pause".into(),
                Cmd::Resume => "Resume".into(),
                Cmd::Next => "Next".into(),
                Cmd::Prev => "Prev".into(),
                Cmd::Stop => "Stop".into(),
                Cmd::SeekTo(t) => format!("SeekTo({:.1})", t),
                Cmd::SeekBy(d) => format!("SeekBy({:.1})", d),
                Cmd::Play { .. } => "Play".into(),
                Cmd::Quit => "Quit".into(),
                Cmd::SetShuffle(on) => format!("SetShuffle({})", on),
                Cmd::SetRepeat(r) => format!("SetRepeat({:?})", r),
                _ => "queue edit".into(),
            };
            println!("engine got: {}", name);
            match cmd {
                Cmd::TogglePause | Cmd::Pause | Cmd::Resume => {
                    let pause = match cmd {
                        Cmd::Pause => true,
                        Cmd::Resume => false,
                        _ => s.state == PlayState::Playing,
                    };
                    s.state = if pause { PlayState::Paused } else { PlayState::Playing };
                }
                Cmd::Next | Cmd::Prev => {
                    let i = if matches!(cmd, Cmd::Next) { s.queue_pos + 1 } else { s.queue_pos.saturating_sub(1) };
                    s.queue_pos = i.min(queue.len() - 1);
                    s.track = Some(queue[s.queue_pos].clone());
                    s.pos = 0.0;
                    s.state = PlayState::Playing;
                }
                Cmd::Stop => {
                    s.state = PlayState::Stopped;
                    s.pos = 0.0;
                }
                Cmd::SeekTo(t) => {
                    s.pos = t;
                    s.seek_seq += 1;
                }
                Cmd::SetShuffle(on) => s.shuffle = on,
                Cmd::SetRepeat(r) => s.repeat = r,
                _ => {}
            }
        }
    }

    fn tests_track() -> Track {
        Track {
            key: String::new(),
            title: String::new(),
            album_key: Some("100".into()),
            artist: "Low Tide Choir".into(),
            album: "Saltmarsh".into(),
            index: None,
            disc: Some(1),
            duration_ms: Some(187_000),
            codec: Some("flac".into()),
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            channels: Some(2),
            bitrate_kbps: Some(636),
            part_key: None,
            container: None,
            rating: None,
            thumb: None,
        }
    }
}
