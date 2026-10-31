//! Output through the desktop's sound server (PipeWire or PulseAudio).
//!
//! This speaks the PulseAudio native protocol directly over its Unix socket,
//! which PipeWire also serves (through pipewire-pulse). Doing it in Rust
//! keeps the release binary static: the usual route, ALSA's `pipewire` or
//! `pulse` plugin, is a shared library that a static binary can't load.
//!
//! The stream is opened in the track's own format; the sound server mixes
//! it with other apps and converts as needed for the chosen output, so this
//! mode is never bit-perfect, but it plays anywhere the desktop can.
//!
//! Everything here is blocking, like the ALSA output: `write` waits until
//! the server asks for more audio.

use crate::output::{OutputInfo, Route, SampleFmt, StreamSpec};
use anyhow::{anyhow, bail, Context, Result};
use pulseaudio::protocol::{
    self as pa, stream::BufferAttr, stream::StreamFlags, ChannelMap, ChannelPosition, Command,
    CorkStreamParams, PlaybackStreamParams, Prop, Props, SampleFormat, SampleSpec,
};
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::io::{BufReader, Cursor, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How much audio the server keeps queued for us. Long enough to ride out
/// a busy system, short enough that pause, skip and seek feel immediate.
const TARGET_LATENCY: Duration = Duration::from_millis(250);

/// The longest the server may stay silent before we give up on it. Drains
/// wait at most about TARGET_LATENCY, and requests arrive many times a
/// second while playing.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest piece of audio sent in one packet (the server's own clients use
/// similar sizes).
const MAX_PACKET: usize = 64 * 1024;

/// A sound-server output (sink) the user can choose.
#[derive(Debug, Clone)]
pub struct Sink {
    pub name: String,
    pub desc: String,
}

/// What the sound server says about itself and its outputs.
#[derive(Debug, Clone)]
pub struct ServerSummary {
    /// "PipeWire" or "PulseAudio".
    pub kind: &'static str,
    pub sinks: Vec<Sink>,
    /// The sink the desktop currently plays through.
    pub default_sink: Option<String>,
}

/// Lists the sound server's outputs. Fails if no server is running.
pub fn query() -> Result<ServerSummary> {
    let mut c = Conn::connect()?;
    let kind = c.server_kind()?;
    let info: pa::ServerInfo = c.call_reply(Command::GetServerInfo)?;
    let sinks: pa::SinkInfoList = c.call_reply(Command::GetSinkInfoList)?;
    Ok(ServerSummary {
        kind,
        sinks: sinks
            .iter()
            .map(|s| Sink {
                name: lossy(&s.name),
                desc: s.description.as_deref().map(lossy).unwrap_or_default(),
            })
            .collect(),
        default_sink: info.default_sink_name.as_deref().map(lossy),
    })
}

/// A stream's volume as the desktop's sound settings show it: 100% is
/// full (the server's "norm"), and the curve is the one their sliders use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamVolume {
    pub percent: u32,
    pub muted: bool,
}

/// The loudest a stream is set: 100%, no boost (which would clip).
pub const MAX_PERCENT: u32 = 100;

/// A connection for reading and setting a stream's volume, separate from
/// the one carrying the audio so it never holds up playback.
pub struct VolumeControl {
    conn: Conn,
}

impl VolumeControl {
    pub fn connect() -> Result<VolumeControl> {
        Ok(VolumeControl { conn: Conn::connect()? })
    }

    /// The volume of stream `index` (its loudest channel, as the desktop's
    /// mixer shows a stream's balance-locked slider).
    pub fn get(&mut self, index: u32) -> Result<StreamVolume> {
        let info: pa::SinkInputInfo = self.conn.call_reply(Command::GetSinkInputInfo(index))?;
        Ok(StreamVolume { percent: percent_of(&info.cvolume), muted: info.muted })
    }

    /// Sets stream `index` to `percent`, keeping the balance between its
    /// channels. Returns the volume it now has.
    pub fn set(&mut self, index: u32, percent: u32) -> Result<StreamVolume> {
        let info: pa::SinkInputInfo = self.conn.call_reply(Command::GetSinkInputInfo(index))?;
        if !info.volume_writable {
            bail!("the sound server doesn't let Plext change its volume");
        }
        let volume = scaled(&info.cvolume, percent.min(MAX_PERCENT), info.channel_map.num_channels());
        self.conn.call_ack(Command::SetSinkInputVolume(pa::SetStreamVolumeParams { index, volume }))?;
        self.get(index)
    }
}

/// The loudest channel, in percent of full.
fn percent_of(v: &pa::ChannelVolume) -> u32 {
    let max = v.channels().iter().map(|c| c.as_u32()).max().unwrap_or(0) as u64;
    ((max * 100 + 0x8000) / 0x10000) as u32
}

/// `v` with its loudest channel at `percent` and the others in proportion.
fn scaled(v: &pa::ChannelVolume, percent: u32, channels: u8) -> pa::ChannelVolume {
    let target = (percent as u64 * 0x10000 / 100) as u32;
    let old: Vec<u32> = v.channels().iter().map(|c| c.as_u32()).collect();
    let max = old.iter().copied().max().unwrap_or(0);
    let mut out = pa::ChannelVolume::empty();
    if max == 0 || old.is_empty() {
        for _ in 0..channels.max(1) {
            out.push(pa::Volume::from_u32_clamped(target));
        }
    } else {
        for c in old {
            out.push(pa::Volume::from_u32_clamped((c as u64 * target as u64 / max as u64) as u32));
        }
    }
    out
}

pub struct PulseOutput {
    conn: RefCell<Conn>,
    pub spec: StreamSpec,
    pub info: OutputInfo,
    channel: u32,
    /// Bytes the server has asked for and not yet received.
    credit: usize,
    frame_bytes: usize,
    buf: Vec<u8>,
}

impl PulseOutput {
    /// Opens a playback stream on `sink`, or on the desktop's default
    /// output when `sink` is None (it then follows the default if the user
    /// changes it).
    pub fn open(sink: Option<&str>, spec: StreamSpec) -> Result<PulseOutput> {
        let mut conn = Conn::connect()?;
        let kind = conn.server_kind()?;

        let fmt = if spec.bits <= 16 { SampleFmt::S16 } else { SampleFmt::S32 };
        let sample_spec = SampleSpec {
            format: match fmt {
                SampleFmt::S16 => SampleFormat::S16Le,
                _ => SampleFormat::S32Le,
            },
            channels: u8::try_from(spec.channels)
                .ok()
                .filter(|&c| c >= 1 && c <= 32)
                .ok_or_else(|| anyhow!("{} channels are more than the sound server takes", spec.channels))?,
            sample_rate: spec.rate,
        };
        let frame_bytes = fmt.bytes() * spec.channels as usize;
        let target = (spec.rate as u64 * TARGET_LATENCY.as_millis() as u64 / 1000) as u32
            * frame_bytes as u32;

        let mut props = Props::new();
        props.set(Prop::MediaName, c"Plext");
        props.set(Prop::MediaRole, c"music");
        let sink_name = match sink {
            Some(s) => CString::new(s).context("invalid sink name")?,
            None => pa::DEFAULT_SINK.to_owned(),
        };
        let params = PlaybackStreamParams {
            sample_spec,
            channel_map: channel_map(spec.channels),
            sink_name: Some(sink_name),
            buffer_attr: BufferAttr {
                max_length: u32::MAX,
                target_length: target,
                pre_buffering: u32::MAX,
                minimum_request_length: u32::MAX,
                fragment_size: u32::MAX,
            },
            cvolume: None,
            props,
            flags: StreamFlags {
                adjust_latency: true,
                // Let the user move Plext to another output while it plays,
                // and keep its volume where they left it.
                ..Default::default()
            },
            ..Default::default()
        };
        let reply: pa::CreatePlaybackStreamReply = conn
            .call_reply(Command::CreatePlaybackStream(params))
            .map_err(|e| match sink {
                Some(s) => anyhow!(
                    "couldn't play through \"{}\" ({}). Run `plext device` to see the outputs {} has",
                    s, e, kind
                ),
                None => anyhow!("couldn't start playback on {} ({})", kind, e),
            })?;

        // Name the output the way the desktop does ("Built-in Audio Analog
        // Stereo", "Topping D90").
        let sinks: pa::SinkInfoList = conn.call_reply(Command::GetSinkInfoList)?;
        let label = sinks
            .iter()
            .find(|s| s.index == reply.sink_index)
            .map(|s| s.description.as_deref().map(lossy).unwrap_or_else(|| lossy(&s.name)))
            .or_else(|| reply.sink_name.as_deref().map(lossy))
            .unwrap_or_else(|| kind.to_string());

        Ok(PulseOutput {
            conn: RefCell::new(conn),
            spec,
            info: OutputInfo {
                label,
                rate: spec.rate,
                fmt,
                route: Route::SoundServer(kind),
                stream: Some(reply.stream_index),
            },
            channel: reply.channel,
            credit: reply.requested_bytes as usize,
            frame_bytes,
            buf: Vec::new(),
        })
    }

    /// Writes interleaved, left-justified 32-bit samples. Blocks until the
    /// server has room for them.
    pub fn write(&mut self, samples: &[i32]) -> Result<()> {
        self.buf.clear();
        self.info.fmt.encode(samples, &mut self.buf);
        let conn = self.conn.get_mut();
        let mut off = 0;
        while off < self.buf.len() {
            // Whole frames only; the server always asks in frame multiples,
            // but never split one across packets regardless.
            let room = (self.credit.min(MAX_PACKET) / self.frame_bytes) * self.frame_bytes;
            if room == 0 {
                self.credit += conn.wait_for_request(self.channel)?;
                continue;
            }
            let n = room.min(self.buf.len() - off);
            pa::write_memblock(conn.sock.get_mut(), self.channel, &self.buf[off..off + n], 0)
                .map_err(lost)?;
            self.credit -= n;
            off += n;
        }
        Ok(())
    }

    /// Pauses or resumes. The server keeps what's queued while paused, so
    /// this always returns true.
    pub fn set_paused(&mut self, paused: bool) -> bool {
        let channel = self.channel;
        let ok = self
            .conn
            .get_mut()
            .call_ack(Command::CorkPlaybackStream(CorkStreamParams { channel, cork: paused }))
            .is_ok();
        self.credit += self.conn.get_mut().take_credit(channel);
        ok
    }

    /// Discards whatever is still queued (skip/seek/stop).
    pub fn flush(&mut self) {
        let channel = self.channel;
        let _ = self.conn.get_mut().call_ack(Command::FlushPlaybackStream(channel));
        self.credit += self.conn.get_mut().take_credit(channel);
    }

    /// Lets queued audio finish playing.
    pub fn drain(&mut self) {
        let channel = self.channel;
        let _ = self.conn.get_mut().call_ack(Command::DrainPlaybackStream(channel));
        self.credit += self.conn.get_mut().take_credit(channel);
    }

    /// Frames written but not yet heard.
    pub fn delay(&self) -> i64 {
        let mut conn = self.conn.borrow_mut();
        let timing: Result<pa::PlaybackLatency> = conn.call_reply(Command::GetPlaybackLatency(
            pa::LatencyParams { channel: self.channel, now: SystemTime::now() },
        ));
        match timing {
            Ok(t) => delay_frames(&t, self.spec.rate, self.frame_bytes),
            Err(_) => 0,
        }
    }
}

/// Queued bytes not yet played, plus the sink's own latency, in frames.
fn delay_frames(t: &pa::PlaybackLatency, rate: u32, frame_bytes: usize) -> i64 {
    let queued = (t.write_offset - t.read_offset).max(0) / frame_bytes as i64;
    let in_sink = if t.playing {
        (t.sink_usec as u128 * rate as u128 / 1_000_000) as i64
    } else {
        0
    };
    queued + in_sink
}

/// Speaker positions in the order FLAC and WAV store channels.
fn channel_map(channels: u32) -> ChannelMap {
    use ChannelPosition::*;
    let positions: &[ChannelPosition] = match channels {
        1 => return ChannelMap::mono(),
        2 => return ChannelMap::stereo(),
        3 => &[FrontLeft, FrontRight, FrontCenter],
        4 => &[FrontLeft, FrontRight, RearLeft, RearRight],
        5 => &[FrontLeft, FrontRight, FrontCenter, RearLeft, RearRight],
        6 => &[FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight],
        7 => &[FrontLeft, FrontRight, FrontCenter, Lfe, RearCenter, SideLeft, SideRight],
        8 => &[FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight, SideLeft, SideRight],
        _ => &[],
    };
    if positions.is_empty() {
        // Anything else: unnamed channels, passed through in order.
        let aux = [Aux0, Aux1, Aux2, Aux3, Aux4, Aux5, Aux6, Aux7, Aux8, Aux9, Aux10, Aux11, Aux12,
            Aux13, Aux14, Aux15, Aux16, Aux17, Aux18, Aux19, Aux20, Aux21, Aux22, Aux23, Aux24,
            Aux25, Aux26, Aux27, Aux28, Aux29, Aux30, Aux31];
        return ChannelMap::new(aux.into_iter().take(channels as usize));
    }
    ChannelMap::new(positions.iter().copied())
}

/// A server error in words.
fn describe(e: pa::PulseError) -> String {
    use pa::PulseError::*;
    match e {
        AccessDenied | AuthKey => "the sound server refused access".into(),
        NoEntity => "no such output".into(),
        NotSupported | Invalid => "the sound server can't play this format".into(),
        Timeout => "the sound server timed out".into(),
        other => format!("sound server error: {:?}", other),
    }
}

fn lossy(s: &CStr) -> String {
    s.to_string_lossy().into_owned()
}

fn lost(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("lost the connection to the sound server ({})", e)
}

/// One connection to the sound server.
struct Conn {
    sock: BufReader<UnixStream>,
    version: u16,
    seq: u32,
    /// Requests for audio that arrived while waiting for something else,
    /// by stream channel.
    pending_credit: Vec<(u32, usize)>,
    killed: bool,
}

impl Conn {
    fn connect() -> Result<Conn> {
        let path = socket_path().ok_or_else(|| {
            anyhow!("no PipeWire or PulseAudio server found (is pipewire-pulse or pulseaudio running?)")
        })?;
        let stream = UnixStream::connect(&path).map_err(|e| {
            anyhow!("couldn't connect to the sound server at {} ({})", path.display(), e)
        })?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        let mut conn = Conn {
            sock: BufReader::new(stream),
            version: pa::MAX_VERSION,
            seq: 0,
            pending_credit: Vec::new(),
            killed: false,
        };

        // PipeWire doesn't check the cookie; PulseAudio does.
        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|p| std::fs::read(p).ok())
            .unwrap_or_default();
        let auth: pa::AuthReply = conn
            .call_reply(Command::Auth(pa::AuthParams {
                version: pa::MAX_VERSION,
                supports_shm: false,
                supports_memfd: false,
                cookie,
            }))
            .context("couldn't connect to the sound server")?;
        conn.version = auth.version.min(pa::MAX_VERSION);
        if conn.version < pa::MIN_VERSION {
            bail!("the sound server's protocol (version {}) is too old", conn.version);
        }

        let mut props = Props::new();
        props.set(Prop::ApplicationName, c"Plext");
        props.set(Prop::ApplicationIconName, c"audio-x-generic");
        let _: pa::SetClientNameReply = conn.call_reply(Command::SetClientName(props))?;
        Ok(conn)
    }

    /// "PipeWire" or "PulseAudio", from the server's name (PipeWire calls
    /// itself "PulseAudio (on PipeWire 1.2.3)").
    fn server_kind(&mut self) -> Result<&'static str> {
        let info: pa::ServerInfo = self.call_reply(Command::GetServerInfo)?;
        let name = info.server_name.as_deref().map(lossy).unwrap_or_default();
        Ok(if name.contains("PipeWire") { "PipeWire" } else { "PulseAudio" })
    }

    fn send(&mut self, cmd: Command) -> Result<u32> {
        self.seq = self.seq.wrapping_add(1) & 0x7fff_ffff;
        let seq = self.seq;
        pa::write_command_message(self.sock.get_mut(), seq, &cmd, self.version).map_err(lost)?;
        Ok(seq)
    }

    /// Reads the next control message, skipping any audio packets. A reply's
    /// payload is left in the returned cursor.
    fn recv(&mut self) -> Result<(u32, Command, Cursor<Vec<u8>>)> {
        loop {
            let desc = pa::read_descriptor(&mut self.sock).map_err(|e| match e {
                pa::ProtocolError::Io(ref io)
                    if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    anyhow!("the sound server stopped responding")
                }
                e => lost(e),
            })?;
            let mut payload = vec![0; desc.length as usize];
            self.sock.read_exact(&mut payload).map_err(lost)?;
            if desc.channel != u32::MAX {
                continue; // audio for a record stream; we have none
            }
            let mut cur = Cursor::new(payload);
            match Command::read_tag_prefixed(&mut cur, self.version) {
                Ok((seq, cmd)) => return Ok((seq, cmd, cur)),
                // A message this library doesn't know. The whole packet has
                // been read, so the stream is still in step: skip it.
                Err(_) => continue,
            }
        }
    }

    /// Handles a message that isn't the reply being waited for.
    fn note_event(&mut self, cmd: Command) {
        match cmd {
            Command::Request(r) => self.add_credit(r.channel, r.length as usize),
            Command::PlaybackStreamKilled(_) => self.killed = true,
            _ => {}
        }
    }

    fn add_credit(&mut self, channel: u32, n: usize) {
        match self.pending_credit.iter_mut().find(|(c, _)| *c == channel) {
            Some((_, have)) => *have += n,
            None => self.pending_credit.push((channel, n)),
        }
    }

    fn take_credit(&mut self, channel: u32) -> usize {
        match self.pending_credit.iter_mut().find(|(c, _)| *c == channel) {
            Some((_, have)) => std::mem::take(have),
            None => 0,
        }
    }

    /// Sends a command and waits for its reply, returning the reply's payload.
    fn call(&mut self, cmd: Command) -> Result<Cursor<Vec<u8>>> {
        let want = self.send(cmd)?;
        loop {
            if self.killed {
                bail!("the sound server closed Plext's stream");
            }
            let (seq, cmd, cur) = self.recv()?;
            if seq == want {
                return match cmd {
                    Command::Reply => Ok(cur),
                    Command::Error(e) => Err(anyhow!("{}", describe(e))),
                    other => Err(anyhow!("unexpected reply {:?}", other)),
                };
            }
            self.note_event(cmd);
        }
    }

    fn call_reply<T: pa::CommandReply>(&mut self, cmd: Command) -> Result<T> {
        let mut cur = self.call(cmd)?;
        let version = self.version;
        let mut ts = pa::TagStructReader::new(&mut cur, version);
        T::read(&mut ts, version).map_err(|e| anyhow!("bad reply from the sound server: {}", e))
    }

    fn call_ack(&mut self, cmd: Command) -> Result<()> {
        self.call(cmd).map(|_| ())
    }

    /// Blocks until the server asks for more audio on `channel`, returning
    /// how many bytes it wants.
    fn wait_for_request(&mut self, channel: u32) -> Result<usize> {
        loop {
            let have = self.take_credit(channel);
            if have > 0 {
                return Ok(have);
            }
            if self.killed {
                bail!("the sound server closed Plext's stream");
            }
            let (_, cmd, _) = self.recv()?;
            self.note_event(cmd);
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.sock.get_mut().flush();
        let _ = self.sock.get_ref().shutdown(std::net::Shutdown::Both);
    }
}

/// Where the sound server's socket is: $PULSE_SERVER (unix: only),
/// $PULSE_RUNTIME_PATH, or the usual place under the user's runtime dir.
fn socket_path() -> Option<PathBuf> {
    let uid_dir = || {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self")
            .ok()
            .map(|m| PathBuf::from(format!("/run/user/{}", m.uid())))
    };
    let candidates = [
        std::env::var("PULSE_SERVER")
            .ok()
            .and_then(|s| s.strip_prefix("unix:").map(PathBuf::from)),
        std::env::var_os("PULSE_RUNTIME_PATH").map(|p| PathBuf::from(p).join("native")),
        std::env::var_os("XDG_RUNTIME_DIR").map(|p| PathBuf::from(p).join("pulse/native")),
        uid_dir().map(|d| d.join("pulse/native")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_maps_follow_flac_order() {
        assert_eq!(channel_map(1).num_channels(), 1);
        assert_eq!(channel_map(2), ChannelMap::stereo());
        let six: Vec<_> = channel_map(6).into_iter().collect();
        use ChannelPosition::*;
        assert_eq!(six, [FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight]);
        assert_eq!(channel_map(12).num_channels(), 12);
    }

    #[test]
    fn volume_percent_is_the_loudest_channel_and_scaling_keeps_the_balance() {
        let mut v = pa::ChannelVolume::empty();
        v.push(pa::Volume::from_u32_clamped(0x10000));
        v.push(pa::Volume::from_u32_clamped(0x8000));
        assert_eq!(percent_of(&v), 100);
        let half = scaled(&v, 50, 2);
        assert_eq!(half.channels().iter().map(|c| c.as_u32()).collect::<Vec<_>>(), [0x8000, 0x4000]);
        assert_eq!(percent_of(&half), 50);
        // From silence, every channel gets the same.
        let from_zero = scaled(&pa::ChannelVolume::muted(2), 30, 2);
        assert_eq!(percent_of(&from_zero), 30);
        assert_eq!(from_zero.channels()[0], from_zero.channels()[1]);
    }

    #[test]
    fn delay_counts_queued_audio_and_sink_latency() {
        let t = pa::PlaybackLatency {
            sink_usec: 20_000,
            source_usec: 0,
            playing: true,
            local_time: SystemTime::now(),
            remote_time: SystemTime::now(),
            write_offset: 44_100 * 4,
            read_offset: 0,
            underrun_for: 0,
            playing_for: 0,
        };
        // One second queued (16-bit stereo) + 20 ms in the sink.
        assert_eq!(delay_frames(&t, 44_100, 4), 44_100 + 882);
        // Paused: only what's queued.
        let paused = pa::PlaybackLatency { playing: false, ..t };
        assert_eq!(delay_frames(&paused, 44_100, 4), 44_100);
    }

    /// Plays through a real PipeWire or PulseAudio server, when one is
    /// running: the server sets the pace, and pause, flush and drain work.
    #[test]
    fn plays_through_a_running_sound_server() {
        use std::time::Instant;
        let spec = StreamSpec { rate: 48_000, channels: 2, bits: 24 };
        let mut out = match PulseOutput::open(None, spec) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("skipping: no sound server: {}", e);
                return;
            }
        };
        assert_eq!(out.info.fmt, SampleFmt::S32);
        assert!(matches!(out.info.route, Route::SoundServer(_)));
        // Silence (nothing to hear while the tests run), in 20 ms chunks, for 1.5 s.
        let chunk_frames = 960;
        let chunk = |n: usize| vec![0i32; n * 2];
        let start = Instant::now();
        for _ in 0..75 {
            out.write(&chunk(chunk_frames)).unwrap();
        }
        // The server only takes about TARGET_LATENCY ahead, so most of the
        // 1.5 s must have been spent playing.
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(1000), "writes didn't wait for the server: {:?}", took);
        let d = out.delay();
        assert!(d > 0 && d < 48_000, "delay {} frames", d);

        // Paused, nothing is consumed.
        assert!(out.set_paused(true));
        let before = out.delay();
        std::thread::sleep(Duration::from_millis(300));
        let after = out.delay();
        assert!((before - after).abs() < 4_800, "played while paused: {} → {}", before, after);
        assert!(out.set_paused(false));

        // Flushed, nothing is left queued (only the sink's own latency).
        out.flush();
        assert!(out.delay() < 48_000 / 5, "after flush: {}", out.delay());

        // Writing again works, and drain returns once it has played.
        for _ in 0..10 {
            out.write(&chunk(chunk_frames)).unwrap();
        }
        let start = Instant::now();
        out.drain();
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
