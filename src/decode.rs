//! Decodes a media stream to interleaved, left-justified 32-bit integer samples.
//!
//! Lossless sources (FLAC, ALAC, WAV, AIFF) come out with their original bits in
//! the top of each 32-bit word and zeros below, so the output stage can hand them
//! to the DAC unchanged.

use anyhow::{anyhow, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    Decoder as SymDecoder, DecoderOptions, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC, CODEC_TYPE_NULL,
};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub codec: String,
    pub lossless: bool,
    pub rate: u32,
    pub channels: u32,
    /// Bits per sample of the source (for lossy codecs, the output width we use).
    pub bits: u32,
    pub duration_secs: Option<f64>,
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymDecoder>,
    track_id: u32,
    pub info: SourceInfo,
    sample_buf: Option<SampleBuffer<i32>>,
    time_base: Option<TimeBase>,
}

impl Decoder {
    pub fn open(source: Box<dyn MediaSource>, ext: Option<&str>) -> Result<Decoder> {
        let mss = MediaSourceStream::new(source, Default::default());
        let mut hint = Hint::new();
        if let Some(e) = ext {
            hint.with_extension(e);
        }
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| anyhow!("unsupported or unreadable file: {}", e))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track found"))?;
        let params = &track.codec_params;
        let decoder = symphonia::default::get_codecs()
            .make(params, &DecoderOptions::default())
            .map_err(|e| anyhow!("unsupported codec: {}", e))?;

        let codec_desc = symphonia::default::get_codecs()
            .get_codec(params.codec)
            .map(|d| d.short_name.to_uppercase())
            .unwrap_or_else(|| "?".into());
        let lossless = params.codec == CODEC_TYPE_FLAC
            || params.codec == CODEC_TYPE_ALAC
            || codec_desc.starts_with("PCM");
        // Some containers (e.g. ALAC in M4A) only reveal these once the decoder
        // has parsed its codec-specific setup, so fall back to the decoder's view.
        let dparams = decoder.codec_params();
        // (M4A stores the rate in a 16-bit field, so rates above 65535 Hz read as 0 there.)
        let rate = params
            .sample_rate
            .filter(|&r| r > 0)
            .or(dparams.sample_rate.filter(|&r| r > 0))
            .or_else(|| Some(decoder.last_decoded().spec().rate).filter(|&r| r > 0))
            .ok_or_else(|| anyhow!("unknown sample rate"))?;
        let channels = params
            .channels
            .or(dparams.channels)
            .map(|c| c.count() as u32)
            .or_else(|| {
                let n = decoder.last_decoded().spec().channels.count() as u32;
                (n > 0).then_some(n)
            })
            .ok_or_else(|| anyhow!("unknown channel layout"))?;
        let bits = match params.bits_per_sample.or(dparams.bits_per_sample) {
            Some(b) if lossless => b,
            // Lossy codecs decode to floating point; 24 bits holds that losslessly enough.
            _ => 24,
        };
        let duration_secs = match (params.n_frames, params.time_base) {
            (Some(n), Some(tb)) => {
                let t = tb.calc_time(n);
                Some(t.seconds as f64 + t.frac)
            }
            (Some(n), None) => Some(n as f64 / rate as f64),
            _ => None,
        };

        let time_base = params.time_base;
        Ok(Decoder {
            time_base,
            track_id: track.id,
            format,
            decoder,
            info: SourceInfo {
                codec: if codec_desc.starts_with("PCM") { "PCM".into() } else { codec_desc },
                lossless,
                rate,
                channels,
                bits,
                duration_secs,
            },
            sample_buf: None,
        })
    }

    /// Decodes the next chunk. Returns `None` at the end of the stream.
    pub fn next_chunk(&mut self) -> Result<Option<&[i32]>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                Err(SymError::ResetRequired) => return Ok(None),
                Err(e) => return Err(anyhow!("read error: {}", e)),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let cap = decoded.capacity() as u64;
                    let need_new = self
                        .sample_buf
                        .as_ref()
                        .map_or(true, |b| (b.capacity() as u64) < cap * spec.channels.count() as u64);
                    if need_new {
                        self.sample_buf = Some(SampleBuffer::<i32>::new(cap, spec));
                    }
                    let buf = self.sample_buf.as_mut().unwrap();
                    buf.copy_interleaved_ref(decoded);
                    if buf.samples().is_empty() {
                        continue;
                    }
                    return Ok(Some(self.sample_buf.as_ref().unwrap().samples()));
                }
                // A corrupt packet: skip it rather than stopping the track.
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(anyhow!("decode error: {}", e)),
            }
        }
    }

    /// Seeks to `secs`; returns the position actually reached.
    pub fn seek(&mut self, secs: f64) -> Result<f64> {
        let secs = secs.max(0.0);
        let to = SeekTo::Time {
            time: Time::new(secs.trunc() as u64, secs.fract()),
            track_id: Some(self.track_id),
        };
        let seeked = self
            .format
            .seek(SeekMode::Accurate, to)
            .map_err(|e| anyhow!("seek failed: {}", e))?;
        self.decoder.reset();
        Ok(match self.time_base {
            Some(tb) => {
                let t = tb.calc_time(seeked.actual_ts);
                t.seconds as f64 + t.frac
            }
            None => seeked.actual_ts as f64 / self.info.rate as f64,
        })
    }
}
