//! Audio output: ALSA straight to the DAC (bit-perfect), or the desktop's
//! sound server (see `pulse.rs`).
//!
//! For bit-perfect playback the device must be a `hw:` device (no ALSA plugins,
//! no PipeWire/PulseAudio), the hardware is switched to the track's exact sample
//! rate (resampling disabled), and samples are written unaltered. Samples narrower
//! than the hardware's container (e.g. 16-bit into a 32-bit slot) are padded with
//! zero bits, which keeps every original bit intact.

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{anyhow, bail, Result};

/// Sample container sent to the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFmt {
    S16,
    /// 24 bits packed in 3 bytes
    S24Packed,
    /// 24 bits in the low 3 bytes of a 4-byte word
    S24In32,
    S32,
}

impl SampleFmt {
    fn alsa(self) -> Format {
        match self {
            SampleFmt::S16 => Format::S16LE,
            SampleFmt::S24Packed => Format::S243LE,
            SampleFmt::S24In32 => Format::S24LE,
            SampleFmt::S32 => Format::S32LE,
        }
    }

    pub fn bits(self) -> u32 {
        match self {
            SampleFmt::S16 => 16,
            SampleFmt::S24Packed | SampleFmt::S24In32 => 24,
            SampleFmt::S32 => 32,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            SampleFmt::S16 => 2,
            SampleFmt::S24Packed => 3,
            SampleFmt::S24In32 | SampleFmt::S32 => 4,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SampleFmt::S16 => "S16_LE",
            SampleFmt::S24Packed => "S24_3LE",
            SampleFmt::S24In32 => "S24_LE",
            SampleFmt::S32 => "S32_LE",
        }
    }

    /// Preference order for a source of the given bit depth: the native width
    /// first, then wider containers (lossless zero-padding), narrower last.
    fn candidates(src_bits: u32) -> &'static [SampleFmt] {
        use SampleFmt::*;
        match src_bits {
            0..=16 => &[S16, S32, S24Packed, S24In32],
            17..=24 => &[S24Packed, S32, S24In32, S16],
            _ => &[S32, S24Packed, S24In32, S16],
        }
    }

    /// Appends samples to `out` in this container's byte layout.
    /// Input samples are left-justified 32-bit (the source's bits sit at the top).
    pub fn encode(self, samples: &[i32], out: &mut Vec<u8>) {
        out.reserve(samples.len() * self.bytes());
        match self {
            SampleFmt::S16 => {
                for &s in samples {
                    out.extend_from_slice(&((s >> 16) as i16).to_le_bytes());
                }
            }
            SampleFmt::S24Packed => {
                for &s in samples {
                    let b = (s >> 8).to_le_bytes();
                    out.extend_from_slice(&b[..3]);
                }
            }
            SampleFmt::S24In32 => {
                for &s in samples {
                    out.extend_from_slice(&(s >> 8).to_le_bytes());
                }
            }
            SampleFmt::S32 => {
                for &s in samples {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
}

/// The stream format a track needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSpec {
    pub rate: u32,
    pub channels: u32,
    pub bits: u32,
}

/// How the audio reaches the speakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Straight to the hardware through an ALSA `hw:` device.
    Direct,
    /// Through an ALSA device with plugins in the way (`plughw:`, `default`…).
    AlsaPlugins,
    /// Through the desktop's sound server: "PipeWire" or "PulseAudio".
    SoundServer(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Human-friendly name, e.g. the DAC's card name.
    pub label: String,
    pub rate: u32,
    pub fmt: SampleFmt,
    pub route: Route,
    /// The sound server's id for Plext's stream (its "sink input"), whose
    /// volume the volume keys change. None for ALSA.
    pub stream: Option<u32>,
}

impl OutputInfo {
    /// Direct hardware access (no ALSA plugin layer, no sound server).
    pub fn direct(&self) -> bool {
        self.route == Route::Direct
    }
}

/// Where to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An ALSA device, e.g. "hw:CARD=DAC,DEV=0".
    Alsa(String),
    /// The desktop's sound server; `sink` None follows the desktop's default output.
    SoundServer { sink: Option<String> },
}

impl Target {
    /// How the target is shown in `plext status` and the log.
    pub fn describe(&self) -> String {
        match self {
            Target::Alsa(d) => d.clone(),
            Target::SoundServer { sink: Some(s) } => s.clone(),
            Target::SoundServer { sink: None } => "the desktop's default output".into(),
        }
    }
}

/// An open output of either kind.
pub enum Output {
    Alsa(AlsaOutput),
    Pulse(crate::pulse::PulseOutput),
}

impl Output {
    pub fn open(target: &Target, spec: StreamSpec) -> Result<Output> {
        match target {
            Target::Alsa(d) => AlsaOutput::open(d, spec).map(Output::Alsa),
            Target::SoundServer { sink } => {
                crate::pulse::PulseOutput::open(sink.as_deref(), spec).map(Output::Pulse)
            }
        }
    }

    pub fn spec(&self) -> StreamSpec {
        match self {
            Output::Alsa(o) => o.spec,
            Output::Pulse(o) => o.spec,
        }
    }

    pub fn info(&self) -> &OutputInfo {
        match self {
            Output::Alsa(o) => &o.info,
            Output::Pulse(o) => &o.info,
        }
    }

    /// Writes interleaved, left-justified 32-bit samples. Blocks while the
    /// output's buffer is full.
    pub fn write(&mut self, samples: &[i32]) -> Result<()> {
        match self {
            Output::Alsa(o) => o.write(samples),
            Output::Pulse(o) => o.write(samples),
        }
    }

    /// Pauses or resumes. Returns false if the output can't pause and the
    /// queued audio was discarded instead (the caller should then resume by
    /// seeking back to the position that was actually heard).
    pub fn set_paused(&mut self, paused: bool) -> bool {
        match self {
            Output::Alsa(o) => o.set_paused(paused),
            Output::Pulse(o) => o.set_paused(paused),
        }
    }

    /// Discards whatever is still queued (skip/seek/stop).
    pub fn flush(&mut self) {
        match self {
            Output::Alsa(o) => o.flush(),
            Output::Pulse(o) => o.flush(),
        }
    }

    /// Lets queued audio finish playing.
    pub fn drain(&mut self) {
        match self {
            Output::Alsa(o) => o.drain(),
            Output::Pulse(o) => o.drain(),
        }
    }

    /// Frames written but not yet heard.
    pub fn delay(&self) -> i64 {
        match self {
            Output::Alsa(o) => o.delay(),
            Output::Pulse(o) => o.delay(),
        }
    }
}

/// ALSA output.
pub struct AlsaOutput {
    pcm: PCM,
    pub spec: StreamSpec,
    pub info: OutputInfo,
    can_pause: bool,
    buf: Vec<u8>,
}

impl AlsaOutput {
    pub fn open(device: &str, spec: StreamSpec) -> Result<AlsaOutput> {
        let pcm = PCM::new(device, Direction::Playback, false).map_err(|e| {
            if e.errno() == libc_ebusy() {
                anyhow!(
                    "{} is busy. Another program (often PipeWire or PulseAudio) is using it. \
                     To play through your desktop's sound instead, run `plext output system`.",
                    device
                )
            } else {
                anyhow!(
                    "could not open {}: {}. Run `plext device` to see your devices, or \
                     `plext output system` to play through your desktop's sound.",
                    device,
                    e
                )
            }
        })?;

        let mut chosen = None;
        let mut last_err = String::new();
        for &fmt in SampleFmt::candidates(spec.bits) {
            match configure(&pcm, spec, fmt) {
                Ok(()) => {
                    chosen = Some(fmt);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let fmt = chosen.ok_or_else(|| {
            anyhow!(
                "{} can't play {}-bit / {} Hz / {} ch without conversion ({})",
                device,
                spec.bits,
                spec.rate,
                spec.channels,
                last_err
            )
        })?;

        let can_pause = pcm
            .hw_params_current()
            .map(|h| h.can_pause())
            .unwrap_or(false);
        pcm.prepare()?;

        Ok(AlsaOutput {
            info: OutputInfo {
                label: friendly_name(device),
                rate: spec.rate,
                fmt,
                route: if is_direct(device) { Route::Direct } else { Route::AlsaPlugins },
                stream: None,
            },
            pcm,
            spec,
            can_pause,
            buf: Vec::new(),
        })
    }

    /// Writes interleaved, left-justified 32-bit samples. Blocks while the
    /// hardware buffer is full.
    pub fn write(&mut self, samples: &[i32]) -> Result<()> {
        self.buf.clear();
        self.info.fmt.encode(samples, &mut self.buf);
        let frame_bytes = self.info.fmt.bytes() * self.spec.channels as usize;
        let mut off = 0;
        while off < self.buf.len() {
            let io = self.pcm.io_bytes();
            match io.writei(&self.buf[off..]) {
                Ok(frames) => off += frames * frame_bytes,
                Err(e) => {
                    // Underrun or suspend: recover and keep going.
                    self.pcm
                        .try_recover(e, true)
                        .map_err(|e| anyhow!("audio device error: {}", e))?;
                }
            }
        }
        Ok(())
    }

    /// Pauses or resumes. Returns false if the hardware can't pause and the
    /// queued audio was discarded instead (the caller should then resume by
    /// seeking back to the position that was actually heard).
    pub fn set_paused(&mut self, paused: bool) -> bool {
        if self.can_pause && self.pcm.pause(paused).is_ok() {
            return true;
        }
        if paused {
            let _ = self.pcm.drop();
        } else {
            let _ = self.pcm.prepare();
        }
        false
    }

    /// Discards whatever is still queued in the hardware buffer (skip/seek/stop).
    pub fn flush(&mut self) {
        let _ = self.pcm.drop();
        let _ = self.pcm.prepare();
    }

    /// Lets queued audio finish playing.
    pub fn drain(&mut self) {
        if self.pcm.state() == State::Running {
            let _ = self.pcm.drain();
        }
        let _ = self.pcm.prepare();
    }

    /// Frames written but not yet heard.
    pub fn delay(&self) -> i64 {
        match self.pcm.state() {
            State::Running | State::Paused => self.pcm.delay().map(|d| d as i64).unwrap_or(0),
            _ => 0,
        }
    }
}

fn configure(pcm: &PCM, spec: StreamSpec, fmt: SampleFmt) -> Result<()> {
    let hwp = HwParams::any(pcm)?;
    hwp.set_rate_resample(false)?;
    hwp.set_access(Access::RWInterleaved)?;
    hwp.set_format(fmt.alsa())?;
    hwp.set_channels(spec.channels)?;
    hwp.set_rate(spec.rate, ValueOr::Nearest)?;
    if hwp.get_rate()? != spec.rate {
        bail!("rate {} not supported", spec.rate);
    }
    // ~300 ms buffer: enough headroom over USB, short enough for snappy skips.
    let _ = hwp.set_buffer_time_near(300_000, ValueOr::Nearest);
    let _ = hwp.set_period_time_near(50_000, ValueOr::Nearest);
    pcm.hw_params(&hwp)?;
    Ok(())
}

/// The sound card's name for a device string: "hw:CARD=D90,DEV=0" or "hw:1,0"
/// becomes e.g. "Topping D90". Other device strings are returned unchanged.
pub fn friendly_name(device: &str) -> String {
    let spec = device
        .strip_prefix("hw:")
        .or_else(|| device.strip_prefix("plughw:"));
    let card = spec.map(|s| {
        let first = s.split(',').next().unwrap_or("");
        first.strip_prefix("CARD=").unwrap_or(first).to_string()
    });
    card.and_then(|c| std::ffi::CString::new(c).ok())
        .and_then(|c| alsa::card::Card::from_str(&c).ok())
        .and_then(|c| c.get_name().ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| device.to_string())
}

fn is_direct(device: &str) -> bool {
    device.starts_with("hw:")
}

fn libc_ebusy() -> i32 {
    16
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub name: String,
    pub desc: String,
}

/// Lists hardware playback devices (the ones that allow bit-perfect output).
pub fn list_devices() -> Vec<DeviceEntry> {
    let mut out = Vec::new();
    if let Ok(iter) = alsa::device_name::HintIter::new_str(None, "pcm") {
        for h in iter {
            let name = match h.name {
                Some(n) => n,
                None => continue,
            };
            if !name.starts_with("hw:") {
                continue;
            }
            if matches!(h.direction, Some(Direction::Capture)) {
                continue;
            }
            let desc = h.desc.unwrap_or_default().replace('\n', " — ");
            out.push(DeviceEntry { name, desc });
        }
    }
    out
}

/// Picks a sensible default: the first USB hardware device, if any.
pub fn guess_device() -> Option<String> {
    list_devices()
        .into_iter()
        .find(|d| d.desc.to_lowercase().contains("usb"))
        .map(|d| d.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_layouts() {
        // 0x123456 as a left-justified 24-bit sample, and a negative one.
        let s = [0x1234_5600_i32, -0x0000_0100_i32];
        let mut out = Vec::new();
        SampleFmt::S24Packed.encode(&s, &mut out);
        assert_eq!(out, [0x56, 0x34, 0x12, 0xff, 0xff, 0xff]);
        out.clear();
        SampleFmt::S24In32.encode(&s, &mut out);
        assert_eq!(out, [0x56, 0x34, 0x12, 0x00, 0xff, 0xff, 0xff, 0xff]);
        out.clear();
        SampleFmt::S32.encode(&s, &mut out);
        assert_eq!(out, [0x00, 0x56, 0x34, 0x12, 0x00, 0xff, 0xff, 0xff]);
    }
}
