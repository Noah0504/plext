//! End-to-end checks that the decode → output path changes no bits.
//!
//! Each test makes a file of random samples with ffmpeg, streams it through
//! `Download` (slowly, in small chunks, like a network transfer), decodes it
//! with `Decoder`, encodes it in the DAC container format with `SampleFmt`, and
//! compares the result byte-for-byte with ffmpeg's own raw decode.
//! Tests are skipped when ffmpeg isn't installed.

use crate::decode::Decoder;
use crate::download::Download;
use crate::output::SampleFmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("plext-test-{}-{}", std::process::id(), name))
}

/// Random noise at full scale (worst case: every bit matters).
fn make_noise_raw(path: &Path, bits: u32, rate: u32, channels: u32, secs: f64) {
    let frames = (rate as f64 * secs) as usize;
    let bytes = if bits == 16 { 2 } else { 3 };
    let mut data = Vec::with_capacity(frames * channels as usize * bytes);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..frames * channels as usize {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        data.extend_from_slice(&x.to_le_bytes()[..bytes]);
    }
    std::fs::write(path, data).unwrap();
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Opens `path` through a throttled Download, as if it were streaming from Plex.
fn stream(path: &Path) -> Download {
    let p = path.to_path_buf();
    let ext = p.extension().unwrap().to_string_lossy().to_string();
    Download::start(&ext, move || {
        struct Slow(std::fs::File);
        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_micros(200));
                let n = buf.len().min(7919); // odd chunk size to hit boundaries
                self.0.read(&mut buf[..n])
            }
        }
        let f = std::fs::File::open(&p)?;
        let len = f.metadata()?.len();
        Ok((Box::new(Slow(f)) as Box<dyn Read + Send>, Some(len)))
    })
    .unwrap()
}

fn check(name: &str, bits: u32, rate: u32, channels: u32, enc: &[&str], ext: &str, out_fmt: SampleFmt) {
    if !have_ffmpeg() {
        eprintln!("skipping {}: ffmpeg not installed", name);
        return;
    }
    let raw_fmt = if bits == 16 { "s16le" } else { "s24le" };
    let raw = tmp(&format!("{}.raw", name));
    let encoded = tmp(&format!("{}.{}", name, ext));
    let reference = tmp(&format!("{}.ref", name));
    make_noise_raw(&raw, bits, rate, channels, 2.5);

    let (rate_s, ch_s) = (rate.to_string(), channels.to_string());
    let mut args = vec!["-f", raw_fmt, "-ar", &rate_s, "-ac", &ch_s, "-i", raw.to_str().unwrap()];
    args.extend_from_slice(enc);
    args.push(encoded.to_str().unwrap());
    ffmpeg(&args);
    // Reference: ffmpeg's decode of the same file, as raw PCM at the source depth.
    ffmpeg(&["-i", encoded.to_str().unwrap(), "-f", raw_fmt, reference.to_str().unwrap()]);

    let dl = stream(&encoded);
    let mut dec = Decoder::open(Box::new(dl.reader().unwrap()), Some(ext)).unwrap();
    assert_eq!(dec.info.rate, rate, "{}: rate", name);
    assert_eq!(dec.info.channels, channels, "{}: channels", name);
    assert_eq!(dec.info.bits, bits, "{}: bit depth", name);
    assert!(dec.info.lossless, "{}: should be lossless", name);

    let mut ours = Vec::new();
    while let Some(chunk) = dec.next_chunk().unwrap() {
        out_fmt.encode(chunk, &mut ours);
    }

    // Build what the DAC should receive from the reference PCM.
    let reference = std::fs::read(&reference).unwrap();
    let src_bytes = (bits / 8) as usize;
    let mut expected = Vec::new();
    for s in reference.chunks_exact(src_bytes) {
        let v: i32 = if bits == 16 {
            (i16::from_le_bytes([s[0], s[1]]) as i32) << 16
        } else {
            i32::from_le_bytes([0, s[0], s[1], s[2]])
        };
        out_fmt.encode(&[v], &mut expected);
    }

    // Also confirm the reference itself equals the original noise (lossless encode).
    let original = std::fs::read(&raw).unwrap();
    assert_eq!(reference.len(), original.len(), "{}: ffmpeg round trip length", name);
    assert!(reference == original, "{}: ffmpeg round trip differs", name);

    assert_eq!(ours.len(), expected.len(), "{}: sample count differs", name);
    let first_diff = ours.iter().zip(&expected).position(|(a, b)| a != b);
    assert!(first_diff.is_none(), "{}: first differing byte at {:?}", name, first_diff);

    for p in [&raw, &encoded] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn flac_24_192_to_s24_packed() {
    check("flac24", 24, 192_000, 2, &["-c:a", "flac", "-sample_fmt", "s32"], "flac", SampleFmt::S24Packed);
}

#[test]
fn flac_24_96_to_s32() {
    check("flac24s32", 24, 96_000, 2, &["-c:a", "flac", "-sample_fmt", "s32"], "flac", SampleFmt::S32);
}

#[test]
fn flac_16_44_to_s16() {
    check("flac16", 16, 44_100, 2, &["-c:a", "flac"], "flac", SampleFmt::S16);
}

#[test]
fn flac_16_44_padded_into_s32() {
    check("flac16s32", 16, 44_100, 2, &["-c:a", "flac"], "flac", SampleFmt::S32);
}

#[test]
fn alac_24_96_to_s24_packed() {
    check("alac24", 24, 96_000, 2, &["-c:a", "alac", "-sample_fmt", "s32p"], "m4a", SampleFmt::S24Packed);
}

#[test]
fn wav_24_192_to_s24_packed() {
    check("wav24", 24, 192_000, 2, &["-c:a", "pcm_s24le"], "wav", SampleFmt::S24Packed);
}

#[test]
fn aiff_16_48_to_s16() {
    check("aiff16", 16, 48_000, 2, &["-c:a", "pcm_s16be"], "aiff", SampleFmt::S16);
}

/// Runs the real ALSA output code against ALSA's "file" device, which records
/// exactly the bytes the hardware would receive, and compares them with the source.
#[test]
fn alsa_output_writes_source_bits_unchanged() {
    use crate::output::{Output, StreamSpec};
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let raw = tmp("alsa.raw");
    let flac = tmp("alsa.flac");
    let captured = tmp("alsa.captured");
    make_noise_raw(&raw, 24, 192_000, 2, 2.0);
    ffmpeg(&[
        "-f", "s24le", "-ar", "192000", "-ac", "2", "-i", raw.to_str().unwrap(),
        "-c:a", "flac", "-sample_fmt", "s32", flac.to_str().unwrap(),
    ]);

    let dl = stream(&flac);
    let mut dec = Decoder::open(Box::new(dl.reader().unwrap()), Some("flac")).unwrap();
    let spec = StreamSpec { rate: dec.info.rate, channels: dec.info.channels, bits: dec.info.bits };
    let device = format!("file:FILE={},FORMAT=raw", captured.display());
    let mut out = match Output::open(&device, spec) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping: ALSA file device unavailable: {}", e);
            return;
        }
    };
    assert_eq!(out.info.fmt, SampleFmt::S24Packed, "should pick native 24-bit packed");
    assert_eq!(out.info.rate, 192_000);
    while let Some(chunk) = dec.next_chunk().unwrap() {
        out.write(chunk).unwrap();
    }
    out.drain();
    drop(out);

    let original = std::fs::read(&raw).unwrap();
    let got = std::fs::read(&captured).unwrap();
    assert_eq!(got.len(), original.len(), "captured length");
    assert!(got == original, "bytes sent to ALSA differ from the source PCM");
    for p in [&raw, &flac, &captured] {
        let _ = std::fs::remove_file(p);
    }
}
