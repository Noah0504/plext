# Changelog

## Unreleased

### New

- **Search.** Press `f` anywhere and type to search your library for artists, albums and tracks
  (Plex's own search, so tracks turn up by their artist or album too). Results come as you pause.
  Play an album, or a track's album from that track, with `Enter`; queue with `a` / `A`; add to a
  playlist with `P`; start a mix with `m`; open an artist in the Artists view. Search is also the
  fourth view `v` cycles through.
- `plext inspect search <words>` shows what your server's search returns, for troubleshooting.
  `plext inspect` now also takes an artist name without quotes (`plext inspect Cat Power`).

### Changed

- The `plext inspect` commands now all start the same way: they say they only read from your
  server, and where to report a problem (the output of `plext inspect mixes` used to ask you to
  paste it into a chat, a leftover from testing).

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
