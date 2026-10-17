# Plext

A terminal music player for Plex with bit-perfect playback straight to your DAC.
Browse your album artists and albums, stream lossless and high-res audio untouched,
all from a single static binary for Linux.

```
 ▶ plext   server: Home NAS   library: Music                                412 artists ● connected
╭ Album Artists ──────── / filter ╮╭ Northbound Winter · Delta Quartet · 2022 ── FLAC 24/192 · 8 tracks ╮
│▸ Cassia Moreau Trio             ││ #    Title                                   Format         Time  │
│▾ Delta Quartet                  ││ 1    First Frost                             24/192         6:12  │
│    Low Light Sessions (2019)    ││ 2    Lantern Road                            24/192         5:48  │
│    ♪ Northbound Winter (2022)   ││ ▶    Northbound                              24/192         7:31  │
│▸ Echo Harbor                    ││ 4    The Quiet Platform                      24/192         4:56  │
╰─────────────────────────────────╯╰───────────────────────────────────────────────────────────────────╯
╭───────────────────────────────────────────────────────────────────────────────────────────────────────╮
│ ▶ PLAYING  Northbound   Delta Quartet — Northbound Winter                             ◆ BIT-PERFECT   │
│ 3:12    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────────────────────────────────────     7:31   │
│ FLAC · 24-bit / 192 kHz · stereo               out: USB DAC · S24_3LE · exclusive · no resample │
╰───────────────────────────────────────────────────────────────────────────────────────────────────────╯
```

## Features

- **Bit-perfect output.** Plays the original file from your server (no transcoding), decodes it,
  and writes the samples unchanged to the DAC through ALSA's `hw:` device. The DAC is switched to
  each track's native sample rate and resampling is disabled. The now-playing bar shows
  **◆ BIT-PERFECT** only when every bit reaches the DAC unaltered.
- **Gapless playback** between tracks of the same format; the next track is pre-fetched.
- **Formats:** FLAC, ALAC, WAV, AIFF (lossless), plus MP3, AAC, Vorbis.
- **All release types**: albums, EPs, singles, live albums, compilations and more, labeled in the
  artist tree.
- **Reports playback to Plex** like official players do, so plays show up in Plex's dashboard and
  history, count toward play counts, and trigger webhooks (`media.play`, `media.pause`,
  `media.resume`, `media.stop`, `media.scrobble`).
- **Plex sign-in with a link code**, automatic server discovery, LAN and remote connections.
- **Single static binary**: no dependencies to install.

## Install

Download `plext-x86_64-linux.tar.gz` from the [Releases](../../releases) page, then:

```sh
tar xzf plext-x86_64-linux.tar.gz
sudo install plext /usr/local/bin/
```

## Getting started

```sh
plext login          # sign in at plex.tv/link, pick your server and music library
plext device         # list your output devices
plext device hw:CARD=DAC,DEV=0   # choose your DAC
plext                # start playing
```

If no device is set, Plext picks the first USB audio device it finds.
You can also choose one for a single session with `plext --device hw:1,0`.
`plext status` shows the current settings.

## Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move |
| `→` / `l` | Expand artist / open album's tracks |
| `←` / `h` | Collapse / back to the artist list |
| `Enter` | Play album (from the selected track in the track list) |
| `Space` | Pause / resume |
| `n` / `p` | Next / previous track |
| `.` / `,` | Seek forward / back 10 s |
| `s` | Stop |
| `Tab` | Switch pane |
| `/` | Filter artists (`Esc` clears) |
| `q` | Quit |

## Plex integration

While you listen, Plext reports progress to your server every 10 seconds and whenever you play,
pause, skip or stop. Plex marks a track as played (and sends `media.scrobble`) once you're about
90% of the way through. Webhooks require Plex Pass and are configured on the server.

To turn reporting off, add this to `~/.config/plext/config.toml`:

```toml
report_playback = false
```

## Bit-perfect notes

- **Exclusive access.** While Plext is playing, the DAC is reserved for it. Other sounds won't play
  through it, and Plext releases the device when playback stops.
- **"Device is busy."** Your desktop sound server (PipeWire or PulseAudio) is holding the DAC.
  Stop other audio, or set the DAC's profile to *Off* in your sound settings
  (e.g. `pavucontrol` → Configuration) while you listen.
- **Volume** stays at full digital scale; use your DAC's or amplifier's volume control.
- **Narrower DAC containers.** 16-bit sources sent to a 32-bit-only DAC are padded with zero bits,
  which preserves every original bit. If a DAC can't accept a source's bit depth or sample rate
  natively, Plext tells you rather than converting silently.
- **DSD** isn't supported yet.

## Building from source

Requires Rust and the ALSA development package (`libasound2-dev` on Debian/Ubuntu,
`alsa-lib-devel` on Fedora).

```sh
cargo build --release
cargo test --release   # bit-perfect tests; needs ffmpeg
```

The tests encode random audio with ffmpeg, stream it through Plext's download, decode and
output path (including the real ALSA output code, captured with ALSA's `file` device), and check
every byte matches the source.
