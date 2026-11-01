# Changelog

## 1.0.0

Plext 1.0: search, volume control in desktop-sound mode, color themes, Raspberry Pi builds, and a
round of fixes that make it safer, lighter and more robust. From here on, settings and commands
stay compatible between versions.

### New

- **Raspberry Pi builds.** Releases now include `plext-aarch64-linux.tar.gz` for the Pi 3, 4, 5
  and Zero 2 W on a 64-bit OS, and `plext-armv6-linux.tar.gz` for every Pi on a 32-bit OS, down
  to the Pi 1 and Zero. The README's new Raspberry Pi section covers which to pick, running Plext
  over SSH in tmux, and why bit-perfect on a Pi needs a USB or I2S DAC.
- **Search.** Press `f` anywhere and type to search your library for artists, albums and tracks
  (Plex's own search, so tracks turn up by their artist or album too). Results come as you pause.
  Play an album, or a track's album from that track, with `Enter`; queue with `a` / `A`; add to a
  playlist with `P`; start a mix with `m`; open an artist in the Artists view. Search is also the
  fourth view `v` cycles through.
- **Volume in desktop-sound mode.** `<` and `>` turn Plext's volume down and up in 5% steps, and
  the now-playing bar shows it. It's Plext's own volume in your desktop's sound settings, so
  changes made there show up in Plext too. MPRIS clients (`playerctl volume`, desktop media
  widgets) can read and set it. In bit-perfect mode the DAC or amp still sets the volume.
- **Color themes.** `T` opens a picker that shows each theme as you move and saves the one you
  keep. Built in: Plext (the default) and Plext Light, Catppuccin (Mocha, Macchiato, Frappé,
  Latte), Dracula and Alucard, Everforest, GitHub, Gruvbox, Kanagawa, Nord and Nord Light, One
  Dark and One Light, Rosé Pine (and Moon, Dawn), Solarized, and Tokyo Night (Night, Storm, Day).
  `terminal` uses your terminal's own colors, and `auto` picks a dark or light theme by asking
  your terminal what its background is. Themes paint their own background unless you set
  `theme_background = false`, and a `[colors]` table changes single colors.
- `plext inspect search <words>` shows what your server's search returns, for troubleshooting.
  `plext inspect` now also takes an artist name without quotes (`plext inspect Cat Power`).
- **`plext device` shows what each device can play**: its widest sample size and highest sample
  rate, "in use by another program", or that it can't play bit-perfect at all (the HDMI audio
  on a Raspberry Pi takes no plain PCM). Setup's device list shows the same, and when no device
  is set, Plext's automatic choice skips the ones that can't play bit-perfect.

### Changed

- **Lighter and snappier.** Plext used to redraw the whole screen ten times a second, idle or
  not. It now draws only when something on screen changes, and only builds the rows of long
  lists that can be on screen. Idle, it uses almost no CPU; with thousands of artists, each
  redraw takes about half the time.
- **Tracks start faster.** Downloads share one connection to your server instead of opening a
  new one (and an encrypted handshake) for every track.
- **A track your DAC can't play is skipped** (say, 96 kHz on a DAC that tops out at 48 kHz), with
  a message naming it, instead of stopping the whole queue.
- **Streaming files moved out of `/tmp`.** Tracks stream into `~/.cache/plext/stream`, private to
  you, and each file is deleted as soon as it's open, so it takes no space once Plext is done
  with it, even after a crash. On many systems `/tmp` is in memory, where a high-res track and
  the next one could fill it on a small machine.
- **Stations and mixes** keep the last 200 tracks played in the queue rather than growing it for
  as long as they run.
- The `plext inspect` commands now all start the same way: they say they only read from your
  server, and where to report a problem (the output of `plext inspect mixes` used to ask you to
  paste it into a chat, a leftover from testing).

### Fixed

- **Closing the terminal while Plext was running left it running in the background at full CPU,
  forever, without saving the queue.** It now saves the queue and quits. This includes an SSH
  connection dropping. `kill` also quits cleanly now, saving the queue and restoring the
  terminal.
- Saving settings could leave `config.toml` readable by other users for a moment, and a crash
  part way through could lose your sign-in. It's now written privately and swapped in whole.

### Security

- **Your Plex sign-in token goes only to your server.** Plext used to check that the saved
  server address still worked by sending it the token. If that address was a plain `http://`
  one on your home network and your computer was on another network, whatever answered there
  got the token. Plext now asks the server who it is first (which needs no token) and only
  carries on if it's yours. Encrypted addresses are always tried before plain http.
- Plext refuses redirects from the server, which could otherwise carry the token to another
  site, and refuses server-supplied paths that would point to another host.
- Album covers larger than any real cover (over 4096×4096 pixels or 64 MB) are refused before
  they're decoded.
- Names and text from the server printed by `plext status`, `plext login` and `plext inspect`
  can no longer carry terminal escape sequences (the player screen already filtered them).

### Upgrading from 0.6

Download the new release and install it the same way; your settings and saved queue are kept.
Nothing to change: settings and commands are the same as in 0.6. Stream files from earlier
versions may be left in `/tmp` as `plext-*` files; they're safe to delete (a reboot usually clears
them anyway).

### Known limitations

- DSD isn't supported yet.
- The Raspberry Pi builds are tested in emulation; reports from real Pis are welcome.

## 0.6.0

A big release: desktop sound, a play queue, your Plex playlists, and a new Discover view with
stations and mixes.

### New

- **Play through your desktop's sound.** Besides bit-perfect output straight to your DAC, Plext
  can now play through PipeWire or PulseAudio, to any speakers or headphones and alongside other
  apps. Setup asks which you want; `plext output` switches any time, and `plext device` lists the
  outputs for the mode you're in. The release binary is still fully static.
- **A play queue.** `a` adds an album or track to the end of the queue and `A` plays it next.
  `u` shows the queue, where you can jump to a track, remove it (`d`), reorder (`J`/`K`) or clear
  what's coming up (`c`). `z` turns shuffle on and off (the original order comes back when you
  turn it off) and `r` cycles repeat: the whole queue, the current track, off. Shuffle and repeat
  show in the now-playing bar and in your desktop's media controls.
- **The queue is saved.** When you quit, Plext saves the queue and brings it back next time,
  paused at the same track and position. Press `Space` to carry on.
- **Your Plex playlists.** `v` switches the left pane to your playlists. Play or queue one like
  an album; add albums, tracks or playlists to a playlist with `P` (or start a new one); save the
  queue as a playlist with `S`; remove and reorder tracks; rename (`R`) and delete (`D`)
  playlists. Changes are saved to Plex, so they show up in Plexamp too. Smart playlists can be
  played, renamed and deleted, but their tracks can't be edited.
- **Big playlists open instantly.** Playlists with thousands of tracks load a page at a time,
  and you can start playing before they've finished loading; the rest joins the queue as it
  arrives (shuffled in, if shuffle is on).
- **Discover.** A third view, with:
  - **Stations** from your Plex server, such as Library Radio, Deep Cuts and Time Travel Radio.
  - **Mixes** built from Plex's sonic analysis: track radio (tracks that sound like one you
    pick) and artist mixes (an artist and artists who sound like them).
  - **Sonic adventures**: highlight any track and press `M`, and the queue travels from what's
    playing to that track, through tracks whose sound lies in between.
  - **Your library**: the shelves your server suggests, like Recently Played, Most Played in
    December and Haven't Played in 5 Months.

  Stations and mixes keep going, adding tracks as the queue runs low, until you play something
  else. Press `m` on any track or artist to start a mix without leaving where you are. Mixes and
  sonic adventures need Plex Pass and sonic analysis turned on for the library.
- **Keys on screen.** The bottom line now shows the keys for wherever you are, and `?` (or `F1`)
  opens a window listing every shortcut.
- **`plext inspect mixes`** shows what your Plex server offers for stations, mixes and sonic
  features, for troubleshooting.

### Fixed

- When several tracks in a row couldn't be loaded, Plext ignored keys (including quit) until it
  had tried them all. It now responds between tracks.
- Quitting no longer waits on a download that's stuck retrying an unreachable server.

### Upgrading from 0.5

Download the new release and install it the same way; your settings are kept. Plext carries on
playing bit-perfect as before. To try desktop sound, run `plext output`.

### Known limitations

- Linux on x86_64 only for now.
- DSD isn't supported yet.
