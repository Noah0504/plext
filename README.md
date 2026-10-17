# Plext

A terminal music player for Plex with bit-perfect playback straight to your DAC.
Browse your album artists and albums, stream lossless and high-res audio untouched,
all from a single static binary for Linux.

![Plext playing a 24-bit/96 kHz album bit-perfect through a USB DAC](docs/screenshot.png)

## Features

- **Bit-perfect output.** Plays the original file from your server (no transcoding), decodes it,
  and writes the samples unchanged to the DAC through ALSA's `hw:` device. The DAC is switched to
  each track's native sample rate and resampling is disabled. The now-playing bar shows
  **◆ BIT-PERFECT** only when every bit reaches the DAC unaltered.
- **Gapless playback** between tracks of the same format; the next track is pre-fetched.
- **Formats:** FLAC, ALAC, WAV, AIFF (lossless), plus MP3, AAC, Vorbis.
- **All release types**: albums, EPs, singles, live albums, compilations and more, grouped under
  headings when you expand an artist (using the release types Plex reads from your tags, the same
  classification MusicBrainz uses).
- **Reports playback to Plex** like official players do, so plays show up in Plex's dashboard and
  history, count toward play counts, and trigger webhooks (`media.play`, `media.pause`,
  `media.resume`, `media.stop`, `media.scrobble`).
- **Plex sign-in with a link code**, automatic server discovery, LAN and remote connections.
- **Single static binary**: no dependencies to install.

## Requirements

- Linux on x86_64 (any distribution; the binary has no dependencies)
- A Plex Media Server with a music library, and a Plex account
- For bit-perfect playback, a USB DAC or other sound card that Plext can access directly
- Plex Pass only if you want Plex webhooks; everything else works without it

## Install

Download `plext-x86_64-linux.tar.gz` from the [Releases](../../releases) page, then:

```sh
tar xzf plext-x86_64-linux.tar.gz
sudo install plext /usr/local/bin/
```

To upgrade, download the new release and run the same two commands; your settings are kept.

## Getting started

```sh
plext login          # sign in at plex.tv/link, pick your server and music library
plext device         # list your output devices
plext device hw:CARD=DAC,DEV=0   # choose your DAC
plext                # start playing
```

If no device is set, Plext picks the first USB audio device it finds.

## Commands

| Command | What it does |
| --- | --- |
| `plext` | Start the player (runs `login` first if you haven't signed in) |
| `plext login` | Sign in at plex.tv/link and choose a server and music library |
| `plext logout` | Forget the saved Plex sign-in |
| `plext device` | List output devices; `plext device <name>` sets the default |
| `plext status` | Show the server, library, device, and config and log file locations |
| `plext inspect "Artist"` | Show what your server reports about an artist's releases |
| `plext --device <name>` | Play through a different device for this session only |

## Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move |
| `PgUp` `PgDn` | Move a page |
| `Home` `End` / `g` `G` | Jump to the top / bottom |
| `→` / `l` | Expand an artist; on an album, move to its track list |
| `←` / `h` | Collapse / back to the artist list |
| `Enter` | Play the album (in the track list, from the selected track) |
| `Space` | Pause / resume |
| `n` / `p` | Next / previous track (`p` restarts the track if you're more than 3 s in) |
| `.` `]` / `,` `[` | Seek forward / back 10 s |
| `s` | Stop |
| `Tab` | Switch pane |
| `/` | Filter artists (`Esc` clears) |
| `q` / `Ctrl+C` | Quit |

## Plex integration

While you listen, Plext reports progress to your server every 10 seconds and whenever you play,
pause, skip or stop. Plex marks a track as played (and sends `media.scrobble`) once you're about
90% of the way through. Webhooks require Plex Pass and are configured on the server.

To turn reporting off, add this to `~/.config/plext/config.toml`:

```toml
report_playback = false
```

## Options

Settings live in `~/.config/plext/config.toml` (`plext status` shows the path). The file also
holds your Plex sign-in token, so Plext keeps it readable only by you.

| Option | Default | What it does |
| --- | --- | --- |
| `device` | first USB DAC | ALSA output device; set it with `plext device <name>` |
| `report_playback` | `true` | Report playback to Plex (dashboard, history, webhooks) |
| `group_releases` | `true` | Group an artist's releases under headings (Albums, EPs, Live…). With `false`, they're one list, oldest first |

## Bit-perfect notes

- **Exclusive access.** While Plext is playing, the DAC is reserved for it. Other sounds won't play
  through it, and Plext releases the device when playback stops.
- **Volume** stays at full digital scale; use your DAC's or amplifier's volume control.
- **Narrower DAC containers.** 16-bit sources sent to a 32-bit-only DAC are padded with zero bits,
  which preserves every original bit. If a DAC can't accept a source's bit depth or sample rate
  natively, Plext tells you rather than converting silently.
- **Lossy files** (MP3, AAC, Vorbis) play normally but are never shown as bit-perfect.
- **DSD** isn't supported yet.

## Troubleshooting

- **"Device is busy."** Your desktop sound server (PipeWire or PulseAudio) is holding the DAC.
  Stop other audio, or set the DAC's profile to *Off* in your sound settings
  (e.g. `pavucontrol` → Configuration) while you listen.
- **A track was skipped, or something flashed by.** Errors stay on screen for 15 seconds and are
  written to `~/.local/state/plext/plext.log`, along with each track played and any dropped
  connections Plext recovered from.
- **Can't reach the server.** Plext tries the server's other addresses automatically. If your
  server moved or your sign-in expired, run `plext login` again.
- **A release is in the wrong group.** Plext uses the release type Plex reads from your tags.
  `plext inspect "Artist"` shows what the server reports for each release.

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

## License

Plext is released under the [MIT License](LICENSE). The release binary also includes
third-party libraries under their own licenses, listed in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md). If you change Plext's dependencies,
regenerate that file with `cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md`
(install with `cargo install cargo-about --features cli`).

## About this project

Plext was built with Claude, Anthropic's AI model, under the direction of Noah Duffy, and is
tested on real hardware. The bit-perfect claims are backed by the automated tests described
above, which run on every build.

Plext is an independent project and is not affiliated with or endorsed by Plex, Inc.
