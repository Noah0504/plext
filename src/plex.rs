//! Minimal Plex client: link-code sign-in, server discovery and music library browsing.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

pub const PRODUCT: &str = "Plext";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct Artist {
    pub key: String,
    pub title: String,
}

/// A playlist on the server.
#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub key: String,
    pub title: String,
    /// Built by Plex from rules; its tracks can't be edited by hand.
    pub smart: bool,
    pub count: u32,
    pub duration_ms: u64,
}

/// A playlist's tracks. `item_ids[i]` is Plex's id for `tracks[i]`'s place
/// in the playlist (the same track can appear twice), used to remove or
/// move it.
#[derive(Debug, Clone, Default)]
pub struct PlaylistTracks {
    pub tracks: Vec<Track>,
    pub item_ids: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct Album {
    pub key: String,
    pub title: String,
    pub year: Option<u32>,
    pub kind: ReleaseKind,
}

/// Plex's release types, in the order an artist's releases are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReleaseKind {
    Album,
    Ep,
    Single,
    Live,
    Compilation,
    Soundtrack,
    Demo,
    Remix,
    Other,
}

impl ReleaseKind {
    /// Heading for a group of releases of this type in the artist tree.
    pub fn group_title(self) -> &'static str {
        match self {
            ReleaseKind::Album => "Albums",
            ReleaseKind::Ep => "EPs",
            ReleaseKind::Single => "Singles",
            ReleaseKind::Live => "Live",
            ReleaseKind::Compilation => "Compilations",
            ReleaseKind::Soundtrack => "Soundtracks",
            ReleaseKind::Demo => "Demos",
            ReleaseKind::Remix => "Remixes",
            ReleaseKind::Other => "Other",
        }
    }

    /// Short label shown after the album title; none for regular albums.
    pub fn label(self) -> Option<&'static str> {
        match self {
            ReleaseKind::Album => None,
            ReleaseKind::Ep => Some("EP"),
            ReleaseKind::Single => Some("Single"),
            ReleaseKind::Live => Some("Live"),
            ReleaseKind::Compilation => Some("Compilation"),
            ReleaseKind::Soundtrack => Some("Soundtrack"),
            ReleaseKind::Demo => Some("Demo"),
            ReleaseKind::Remix => Some("Remix"),
            ReleaseKind::Other => Some("Other"),
        }
    }

    /// The type to show beside a release's name: every known type, albums
    /// included, so they all read the same way; nothing when Plex doesn't know.
    pub fn display_label(self) -> Option<&'static str> {
        match self {
            ReleaseKind::Album => Some("Album"),
            ReleaseKind::Other => None,
            k => k.label(),
        }
    }

    /// Reads the release type from an album's `Format` and `Subformat` tags
    /// (e.g. Format "EP", Subformat "Live").
    fn from_metadata(m: &Value) -> ReleaseKind {
        let tags: Vec<String> = ["Format", "Subformat"]
            .iter()
            .flat_map(|k| m[*k].as_array().into_iter().flatten())
            .filter_map(|t| t["tag"].as_str())
            .map(|t| t.to_lowercase())
            .collect();
        let has = |w: &str| tags.iter().any(|t| t.contains(w));
        if has("live") {
            ReleaseKind::Live
        } else if has("compilation") {
            ReleaseKind::Compilation
        } else if has("soundtrack") {
            ReleaseKind::Soundtrack
        } else if has("demo") {
            ReleaseKind::Demo
        } else if has("remix") {
            ReleaseKind::Remix
        } else if has("ep") && !has("album") {
            ReleaseKind::Ep
        } else if has("single") {
            ReleaseKind::Single
        } else if tags.is_empty() || has("album") {
            ReleaseKind::Album
        } else {
            ReleaseKind::Other
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Track {
    pub key: String,
    pub title: String,
    /// Rating key of the album this track belongs to.
    pub album_key: Option<String>,
    /// Track artist (falls back to the album artist).
    pub artist: String,
    pub album: String,
    pub index: Option<u32>,
    pub disc: Option<u32>,
    pub duration_ms: Option<u64>,
    pub codec: Option<String>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    /// Path of the original media file on the server, e.g. "/library/parts/123/456/file.flac".
    pub part_key: Option<String>,
    pub container: Option<String>,
    /// Your rating in Plex's units: 1-10, where 2 is one star (so odd numbers
    /// are half stars). `None` when the track isn't rated.
    pub rating: Option<u8>,
    /// Server path of the album cover, e.g. "/library/metadata/123/thumb/1700000000".
    pub thumb: Option<String>,
}

impl Track {
    /// Short format label for lists, e.g. "24/192" or "MP3 320".
    pub fn format_label(&self) -> String {
        match (self.bit_depth, self.sample_rate) {
            (Some(b), Some(r)) => format!("{}/{}", b, khz(r)),
            _ => match (&self.codec, self.bitrate_kbps) {
                (Some(c), Some(br)) => format!("{} {}", c.to_uppercase(), br),
                (Some(c), None) => c.to_uppercase(),
                _ => String::new(),
            },
        }
    }
}

/// 44100 -> "44.1", 192000 -> "192"
pub fn khz(rate: u32) -> String {
    if rate % 1000 == 0 {
        format!("{}", rate / 1000)
    } else {
        let s = format!("{:.1}", rate as f64 / 1000.0);
        s.trim_end_matches(".0").to_string()
    }
}

#[derive(Debug, Clone)]
pub struct Library {
    pub key: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub name: String,
    pub machine_id: String,
    pub token: String,
    pub owned: bool,
    /// Candidate base URLs, best first.
    pub urls: Vec<String>,
}

#[derive(Clone)]
pub struct Plex {
    agent: ureq::Agent,
    client_id: String,
}

impl Plex {
    pub fn new(client_id: &str) -> Plex {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(30))
            .user_agent(&format!("{}/{}", PRODUCT, VERSION))
            .build();
        Plex {
            agent,
            client_id: client_id.to_string(),
        }
    }

    fn req(&self, method: &str, url: &str, token: Option<&str>) -> ureq::Request {
        let device = hostname();
        let mut r = self
            .agent
            .request(method, url)
            .set("Accept", "application/json")
            .set("X-Plex-Product", PRODUCT)
            .set("X-Plex-Version", VERSION)
            .set("X-Plex-Client-Identifier", &self.client_id)
            .set("X-Plex-Platform", "Linux")
            .set("X-Plex-Device", "Linux")
            .set("X-Plex-Device-Name", &device);
        if let Some(t) = token {
            r = r.set("X-Plex-Token", t);
        }
        r
    }

    fn get_json(&self, url: &str, token: Option<&str>) -> Result<Value> {
        let resp = self.req("GET", url, token).call().map_err(describe)?;
        let v: Value = resp.into_json().context("invalid JSON from Plex")?;
        Ok(v)
    }

    // ---------- sign-in ----------

    /// Requests a 4-character link code. Returns (pin id, code).
    pub fn create_pin(&self) -> Result<(u64, String)> {
        let resp = self
            .req("POST", "https://plex.tv/api/v2/pins", None)
            .send_form(&[("strong", "false")])
            .map_err(describe)?;
        let v: Value = resp.into_json()?;
        let id = v["id"].as_u64().ok_or_else(|| anyhow!("no pin id in response"))?;
        let code = v["code"]
            .as_str()
            .ok_or_else(|| anyhow!("no code in response"))?
            .to_string();
        Ok((id, code))
    }

    /// Returns the account token once the user has entered the code at plex.tv/link.
    pub fn check_pin(&self, id: u64) -> Result<Option<String>> {
        let v = self.get_json(&format!("https://plex.tv/api/v2/pins/{}", id), None)?;
        Ok(v["authToken"].as_str().filter(|s| !s.is_empty()).map(String::from))
    }

    // ---------- servers ----------

    pub fn servers(&self, account_token: &str) -> Result<Vec<Resource>> {
        let v = self.get_json(
            "https://clients.plex.tv/api/v2/resources?includeHttps=1&includeRelay=1",
            Some(account_token),
        )?;
        let arr = v.as_array().ok_or_else(|| anyhow!("unexpected resources response"))?;
        let mut out = Vec::new();
        for r in arr {
            let provides = r["provides"].as_str().unwrap_or("");
            if !provides.split(',').any(|p| p == "server") {
                continue;
            }
            let mut local = Vec::new();
            let mut remote = Vec::new();
            let mut relay = Vec::new();
            for c in r["connections"].as_array().into_iter().flatten() {
                let uri = c["uri"].as_str().unwrap_or("").to_string();
                if uri.is_empty() {
                    continue;
                }
                if c["relay"].as_bool().unwrap_or(false) {
                    relay.push(uri);
                } else if c["local"].as_bool().unwrap_or(false) {
                    local.push(uri);
                    // Plain http on the LAN also works when plex.direct DNS is blocked by the router.
                    if let (Some(addr), Some(port)) = (c["address"].as_str(), c["port"].as_u64()) {
                        local.push(format!("http://{}:{}", addr, port));
                    }
                } else {
                    remote.push(uri);
                }
            }
            let mut urls = local;
            urls.extend(remote);
            urls.extend(relay);
            urls.dedup();
            out.push(Resource {
                name: r["name"].as_str().unwrap_or("Plex server").to_string(),
                machine_id: r["clientIdentifier"].as_str().unwrap_or("").to_string(),
                token: r["accessToken"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| account_token.to_string()),
                owned: r["owned"].as_bool().unwrap_or(false),
                urls,
            });
        }
        Ok(out)
    }

    /// Tries each candidate URL and returns the first that answers.
    pub fn find_working_url(&self, res: &Resource) -> Option<String> {
        res.urls
            .iter()
            .find(|u| self.ping(u, &res.token).is_ok())
            .cloned()
    }

    pub fn ping(&self, base: &str, token: &str) -> Result<()> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(4))
            .build();
        agent
            .get(&format!("{}/identity", base))
            .set("Accept", "application/json")
            .set("X-Plex-Token", token)
            .call()
            .map_err(describe)?;
        Ok(())
    }
}

/// A connection to one Plex Media Server.
#[derive(Clone)]
pub struct Server {
    plex: Plex,
    pub base: String,
    pub token: String,
    pub machine_id: String,
}

/// A play queue created on the server, so Plex can track what we're playing.
#[derive(Debug, Clone)]
pub struct PlayQueue {
    pub id: u64,
    /// Track rating key -> playQueueItemID
    pub items: Vec<(String, u64)>,
}

impl PlayQueue {
    pub fn item_id(&self, track_key: &str) -> Option<u64> {
        self.items.iter().find(|(k, _)| k == track_key).map(|(_, id)| *id)
    }
}

/// One progress report for the server ("timeline"). Plex derives its
/// now-playing sessions, play counts and webhooks from these.
pub struct Timeline<'a> {
    pub track: &'a Track,
    pub state: &'static str, // "playing" | "paused" | "stopped"
    pub time_ms: u64,
    pub queue: Option<&'a PlayQueue>,
}

impl Server {
    pub fn new(plex: Plex, base: &str, token: &str, machine_id: &str) -> Server {
        Server {
            plex,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            machine_id: machine_id.to_string(),
        }
    }

    /// Creates a play queue for an album on the server.
    pub fn create_play_queue(&self, album_key: &str) -> Result<PlayQueue> {
        let uri = format!(
            "server://{}/com.plexapp.plugins.library/library/metadata/{}",
            self.machine_id, album_key
        );
        let url = format!(
            "{}/playQueues?type=audio&shuffle=0&repeat=0&continuous=0&own=1&uri={}",
            self.base,
            encode(&uri)
        );
        let resp = self
            .plex
            .req("POST", &url, Some(&self.token))
            .call()
            .map_err(describe)?;
        let v: Value = resp.into_json()?;
        let id = v["MediaContainer"]["playQueueID"]
            .as_u64()
            .ok_or_else(|| anyhow!("no playQueueID in response"))?;
        let items = items(&v, "Metadata")
            .filter_map(|m| Some((str_of(&m["ratingKey"]), m["playQueueItemID"].as_u64()?)))
            .collect();
        Ok(PlayQueue { id, items })
    }

    /// Reports playback progress to the server.
    pub fn report_timeline(&self, t: &Timeline, session_id: &str) -> Result<()> {
        let mut url = format!(
            "{}/:/timeline?ratingKey={}&key={}&state={}&time={}&hasMDE=1",
            self.base,
            encode(&t.track.key),
            encode(&format!("/library/metadata/{}", t.track.key)),
            t.state,
            t.time_ms,
        );
        if let Some(d) = t.track.duration_ms {
            url.push_str(&format!("&duration={}", d));
        }
        if let Some(q) = t.queue {
            url.push_str(&format!("&containerKey={}", encode(&format!("/playQueues/{}", q.id))));
            if let Some(item) = q.item_id(&t.track.key) {
                url.push_str(&format!("&playQueueItemID={}", item));
            }
        }
        self.plex
            .req("GET", &url, Some(&self.token))
            .set("X-Plex-Provides", "player")
            .set("X-Plex-Session-Identifier", session_id)
            .timeout(Duration::from_secs(5))
            .call()
            .map_err(describe)?;
        Ok(())
    }

    /// Sets your rating for an item (1-10, two per star), or clears it with `None`.
    pub fn rate(&self, key: &str, rating: Option<u8>) -> Result<()> {
        let value = rating.map(|r| r.min(10) as i32).unwrap_or(-1);
        let url = format!(
            "{}/:/rate?key={}&identifier=com.plexapp.plugins.library&rating={}",
            self.base,
            encode(key),
            value
        );
        self.plex
            .req("PUT", &url, Some(&self.token))
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(describe)?;
        Ok(())
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.plex
            .get_json(&format!("{}{}", self.base, path), Some(&self.token))
    }

    /// The server's JSON answer for a path, for diagnostics (`plext inspect mixes`).
    pub fn get_raw(&self, path: &str) -> Result<Value> {
        self.get(path)
    }

    pub fn music_libraries(&self) -> Result<Vec<Library>> {
        let v = self.get("/library/sections")?;
        Ok(items(&v, "Directory")
            .filter(|d| d["type"].as_str() == Some("artist"))
            .map(|d| Library {
                key: str_of(&d["key"]),
                title: str_of(&d["title"]),
            })
            .collect())
    }

    pub fn artists(&self, library_key: &str) -> Result<Vec<Artist>> {
        let v = self.get(&format!("/library/sections/{}/all?type=8", library_key))?;
        Ok(items(&v, "Metadata")
            .map(|m| Artist {
                key: str_of(&m["ratingKey"]),
                title: str_of(&m["title"]),
            })
            .collect())
    }

    /// Raw metadata for each of an artist's releases (for `plext inspect`).
    pub fn album_details_raw(&self, library_key: &str, artist_key: &str) -> Result<Vec<Value>> {
        let albums = self.albums(library_key, artist_key)?;
        let keys: Vec<String> = albums.iter().map(|a| a.key.clone()).collect();
        let mut out = Vec::new();
        for chunk in keys.chunks(50) {
            let v = self.get(&format!("/library/metadata/{}", chunk.join(",")))?;
            out.extend(items(&v, "Metadata").cloned());
        }
        Ok(out)
    }

    /// All releases by an artist: albums, EPs, singles, live albums, compilations…
    pub fn albums(&self, library_key: &str, artist_key: &str) -> Result<Vec<Album>> {
        let parse = |v: &Value| -> Vec<Album> {
            items(v, "Metadata")
                .filter(|m| m["type"].as_str().map_or(true, |t| t == "album"))
                .map(|m| Album {
                    key: str_of(&m["ratingKey"]),
                    title: str_of(&m["title"]),
                    year: m["year"].as_u64().map(|y| y as u32),
                    kind: ReleaseKind::from_metadata(m),
                })
                .collect()
        };
        // Filtering the library by artist returns every release type; an artist's
        // "children" may list only regular albums on servers that group releases.
        let filtered = self
            .get(&format!(
                "/library/sections/{}/all?type=9&artist.id={}",
                library_key, artist_key
            ))
            .map(|v| parse(&v));
        let children = self
            .get(&format!("/library/metadata/{}/children", artist_key))
            .map(|v| parse(&v));
        let mut albums = match (filtered, children) {
            (Err(e), Err(_)) => return Err(e),
            (a, b) => {
                let mut all = a.unwrap_or_default();
                for al in b.unwrap_or_default() {
                    match all.iter_mut().find(|x| x.key == al.key) {
                        // Keep the more specific release type if the two disagree.
                        Some(x) if x.kind == ReleaseKind::Album => x.kind = al.kind,
                        Some(_) => {}
                        None => all.push(al),
                    }
                }
                all
            }
        };
        // List responses usually omit the Format/Subformat tags that carry the
        // release type; a batched metadata request returns them.
        let keys: Vec<String> = albums.iter().map(|a| a.key.clone()).collect();
        for chunk in keys.chunks(50) {
            if let Ok(v) = self.get(&format!("/library/metadata/{}", chunk.join(","))) {
                for m in items(&v, "Metadata") {
                    let key = str_of(&m["ratingKey"]);
                    let kind = ReleaseKind::from_metadata(m);
                    if let Some(al) = albums.iter_mut().find(|a| a.key == key) {
                        if al.kind == ReleaseKind::Album {
                            al.kind = kind;
                        }
                    }
                }
            }
        }
        albums.sort_by(|a, b| {
            (a.kind, a.year.unwrap_or(u32::MAX), a.title.to_lowercase())
                .cmp(&(b.kind, b.year.unwrap_or(u32::MAX), b.title.to_lowercase()))
        });
        Ok(albums)
    }

    pub fn tracks(&self, album_key: &str) -> Result<Vec<Track>> {
        let v = self.get(&format!("/library/metadata/{}/children", album_key))?;
        let mut tracks: Vec<Track> = items(&v, "Metadata").map(parse_track).collect();
        self.fill_stream_details(&mut tracks);
        tracks.sort_by_key(|t| (t.disc.unwrap_or(1), t.index.unwrap_or(u32::MAX)));
        Ok(tracks)
    }

    /// Listings usually lack bit depth / sample rate. Batched metadata
    /// requests return the full stream details. Best-effort: tracks keep
    /// what they have if this fails.
    fn fill_stream_details(&self, tracks: &mut [Track]) {
        let mut keys: Vec<String> = tracks.iter().map(|t| t.key.clone()).collect();
        keys.sort();
        keys.dedup();
        for chunk in keys.chunks(50) {
            if let Ok(detail) = self.get(&format!("/library/metadata/{}", chunk.join(","))) {
                for d in items(&detail, "Metadata") {
                    let full = parse_track(d);
                    for t in tracks.iter_mut().filter(|t| t.key == full.key) {
                        *t = merge(t.clone(), full.clone());
                    }
                }
            }
        }
    }

    // ---------- playlists ----------

    /// The audio playlists with music from this library, sorted by title.
    pub fn playlists(&self, library_key: &str) -> Result<Vec<Playlist>> {
        let v = self.get(&format!("/playlists?playlistType=audio&sectionID={}", encode(library_key)))?;
        let mut out: Vec<Playlist> = items(&v, "Metadata")
            .filter(|m| m["playlistType"].as_str().map_or(true, |t| t == "audio"))
            .map(parse_playlist)
            .collect();
        out.sort_by_key(|p| p.title.to_lowercase());
        Ok(out)
    }

    /// Up to `size` of a playlist's tracks, starting at `start`, and how many
    /// it has in all. Big playlists (thousands of tracks) load a page at a
    /// time this way instead of in one huge response.
    ///
    /// The listing has what playback needs (the media file) but not the
    /// stream details albums fetch for their format column; the playlist
    /// view doesn't show that, and the player reads the format from the file.
    pub fn playlist_page(&self, playlist_key: &str, start: usize, size: usize) -> Result<(PlaylistTracks, usize)> {
        let v = self.get(&format!(
            "/playlists/{}/items?X-Plex-Container-Start={}&X-Plex-Container-Size={}",
            encode(playlist_key),
            start,
            size
        ))?;
        let mut out = PlaylistTracks::default();
        let all: Vec<&Value> = items(&v, "Metadata").collect();
        for m in all.iter().filter(|m| m["type"].as_str().map_or(true, |t| t == "track")) {
            out.tracks.push(parse_track(m));
            out.item_ids.push(m["playlistItemID"].as_u64().unwrap_or(0));
        }
        let mc = &v["MediaContainer"];
        let total = mc["totalSize"]
            .as_u64()
            .or_else(|| mc["leafCount"].as_u64())
            .map(|n| n as usize)
            // No total given: assume this page is the last unless it's full.
            .unwrap_or(if all.len() >= size { start + all.len() + 1 } else { start + all.len() });
        Ok((out, total))
    }

    /// One track's full details (used when a track arrives without its media file).
    pub fn track(&self, key: &str) -> Result<Track> {
        let v = self.get(&format!("/library/metadata/{}", encode(key)))?;
        let t = items(&v, "Metadata").next().map(parse_track);
        t.ok_or_else(|| anyhow!("track {} not found", key))
    }

    /// "server://…/library/metadata/1,2,3": how Plex refers to a set of items.
    fn items_uri(&self, keys: &[String]) -> String {
        format!(
            "server://{}/com.plexapp.plugins.library/library/metadata/{}",
            self.machine_id,
            keys.join(",")
        )
    }

    fn send(&self, method: &str, path_and_query: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path_and_query);
        let resp = self
            .plex
            .req(method, &url, Some(&self.token))
            .timeout(Duration::from_secs(20))
            .call()
            .map_err(describe)?;
        // Some of these answer with an empty body.
        let text = resp.into_string().unwrap_or_default();
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Creates a playlist holding `track_keys`, in order. Returns it.
    pub fn create_playlist(&self, title: &str, track_keys: &[String]) -> Result<Playlist> {
        if track_keys.is_empty() {
            bail!("a new playlist needs at least one track");
        }
        // Long lists go in pieces: one URL can only be so long.
        let (first, rest) = track_keys.split_at(track_keys.len().min(PLAYLIST_BATCH));
        let v = self.send(
            "POST",
            &format!(
                "/playlists?type=audio&smart=0&title={}&uri={}",
                encode(title),
                encode(&self.items_uri(first))
            ),
        )?;
        let created = items(&v, "Metadata")
            .next()
            .map(parse_playlist)
            .ok_or_else(|| anyhow!("Plex didn't return the new playlist"))?;
        if !rest.is_empty() {
            self.add_to_playlist(&created.key, rest)?;
        }
        Ok(Playlist {
            count: track_keys.len() as u32,
            ..created
        })
    }

    /// Adds tracks to the end of a playlist.
    pub fn add_to_playlist(&self, playlist_key: &str, track_keys: &[String]) -> Result<()> {
        for chunk in track_keys.chunks(PLAYLIST_BATCH) {
            self.send(
                "PUT",
                &format!("/playlists/{}/items?uri={}", encode(playlist_key), encode(&self.items_uri(chunk))),
            )?;
        }
        Ok(())
    }

    pub fn remove_from_playlist(&self, playlist_key: &str, item_id: u64) -> Result<()> {
        self.send("DELETE", &format!("/playlists/{}/items/{}", encode(playlist_key), item_id))?;
        Ok(())
    }

    /// Moves an item to just after `after`, or to the top when `after` is None.
    pub fn move_in_playlist(&self, playlist_key: &str, item_id: u64, after: Option<u64>) -> Result<()> {
        let mut path = format!("/playlists/{}/items/{}/move", encode(playlist_key), item_id);
        if let Some(a) = after {
            path.push_str(&format!("?after={}", a));
        }
        self.send("PUT", &path)?;
        Ok(())
    }

    pub fn rename_playlist(&self, playlist_key: &str, title: &str) -> Result<()> {
        self.send("PUT", &format!("/playlists/{}?title={}", encode(playlist_key), encode(title)))?;
        Ok(())
    }

    pub fn delete_playlist(&self, playlist_key: &str) -> Result<()> {
        self.send("DELETE", &format!("/playlists/{}", encode(playlist_key)))?;
        Ok(())
    }

    /// Downloads an image such as an album cover, scaled down by the server to
    /// at most `size`×`size` pixels. `path` is a server path like a track's `thumb`.
    pub fn fetch_image(&self, path: &str, size: u32) -> Result<Vec<u8>> {
        let url = format!(
            "{}/photo/:/transcode?width={size}&height={size}&minSize=1&upscale=0&url={}",
            self.base,
            encode(path)
        );
        let resp = self
            .plex
            .req("GET", &url, Some(&self.token))
            .set("Accept", "image/jpeg, image/png, image/*")
            .timeout(Duration::from_secs(15))
            .call()
            .map_err(describe)?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(16 * 1024 * 1024)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Err(anyhow!("empty image from Plex"));
        }
        Ok(bytes)
    }

    /// Opens the original media file for streaming (no transcoding), starting
    /// at byte `offset` (used to resume after a dropped connection).
    /// Returns a reader positioned at `offset` and the file's total size if known.
    pub fn open_media(&self, part_key: &str, offset: u64) -> Result<(Box<dyn Read + Send>, Option<u64>)> {
        let url = format!("{}{}", self.base, part_key);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .build();
        let mut req = agent
            .get(&url)
            .set("X-Plex-Token", &self.token)
            .set("X-Plex-Client-Identifier", &self.plex.client_id)
            .set("X-Plex-Product", PRODUCT);
        if offset > 0 {
            req = req.set("Range", &format!("bytes={}-", offset));
        }
        let resp = req.call().map_err(describe)?;
        let partial = resp.status() == 206;
        let total = if partial {
            // "Content-Range: bytes 1000-1999/2000"
            resp.header("Content-Range")
                .and_then(|r| r.rsplit('/').next())
                .and_then(|t| t.trim().parse::<u64>().ok())
        } else {
            resp.header("Content-Length").and_then(|s| s.parse::<u64>().ok())
        };
        let mut reader: Box<dyn Read + Send> = Box::new(resp.into_reader());
        if offset > 0 && !partial {
            // The server ignored the range request: skip what we already have.
            let skipped = std::io::copy(&mut (&mut reader).take(offset), &mut std::io::sink())?;
            if skipped < offset {
                return Err(anyhow!("file is shorter than expected"));
            }
        }
        Ok((reader, total))
    }
}

fn merge(base: Track, full: Track) -> Track {
    Track {
        key: base.key,
        title: if full.title.is_empty() { base.title } else { full.title },
        album_key: full.album_key.or(base.album_key),
        artist: if full.artist.is_empty() { base.artist } else { full.artist },
        album: if full.album.is_empty() { base.album } else { full.album },
        index: full.index.or(base.index),
        disc: full.disc.or(base.disc),
        duration_ms: full.duration_ms.or(base.duration_ms),
        codec: full.codec.or(base.codec),
        bit_depth: full.bit_depth.or(base.bit_depth),
        sample_rate: full.sample_rate.or(base.sample_rate),
        channels: full.channels.or(base.channels),
        bitrate_kbps: full.bitrate_kbps.or(base.bitrate_kbps),
        part_key: full.part_key.or(base.part_key),
        container: full.container.or(base.container),
        rating: full.rating.or(base.rating),
        thumb: full.thumb.or(base.thumb),
    }
}

/// Tracks per request when adding to a playlist, to keep URLs short.
const PLAYLIST_BATCH: usize = 100;

fn parse_playlist(m: &Value) -> Playlist {
    Playlist {
        key: str_of(&m["ratingKey"]),
        title: str_of(&m["title"]),
        smart: match &m["smart"] {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_u64() == Some(1),
            Value::String(s) => s == "1" || s == "true",
            _ => false,
        },
        count: m["leafCount"].as_u64().unwrap_or(0) as u32,
        duration_ms: m["duration"].as_u64().unwrap_or(0),
    }
}

fn parse_track(m: &Value) -> Track {
    let media = m["Media"].as_array().and_then(|a| a.first());
    let part = media
        .and_then(|md| md["Part"].as_array())
        .and_then(|a| a.first());
    // The audio stream (streamType 2) carries bit depth and sample rate.
    let stream = part
        .and_then(|p| p["Stream"].as_array())
        .and_then(|a| a.iter().find(|s| s["streamType"].as_u64() == Some(2)));

    let num = |v: Option<&Value>, k: &str| -> Option<u32> {
        v.and_then(|x| match &x[k] {
            Value::Number(n) => n.as_u64().map(|n| n as u32),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
    };

    let artist = m["originalTitle"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| m["grandparentTitle"].as_str())
        .unwrap_or("")
        .to_string();

    Track {
        key: str_of(&m["ratingKey"]),
        title: str_of(&m["title"]),
        album_key: Some(str_of(&m["parentRatingKey"])).filter(|s| !s.is_empty()),
        artist,
        album: str_of(&m["parentTitle"]),
        index: m["index"].as_u64().map(|n| n as u32),
        disc: m["parentIndex"].as_u64().map(|n| n as u32),
        duration_ms: m["duration"].as_u64(),
        codec: media
            .and_then(|md| md["audioCodec"].as_str())
            .or_else(|| stream.and_then(|s| s["codec"].as_str()))
            .map(String::from),
        bit_depth: num(stream, "bitDepth"),
        sample_rate: num(stream, "samplingRate"),
        channels: num(media, "audioChannels").or_else(|| num(stream, "channels")),
        bitrate_kbps: num(media, "bitrate"),
        part_key: part.and_then(|p| p["key"].as_str()).map(String::from),
        container: part
            .and_then(|p| p["container"].as_str())
            .or_else(|| media.and_then(|md| md["container"].as_str()))
            .map(String::from),
        rating: parse_rating(&m["userRating"]),
        // A track's own thumb is usually its album's cover; prefer the album's.
        thumb: m["parentThumb"]
            .as_str()
            .or_else(|| m["thumb"].as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
    }
}

/// Plex sends `userRating` as a number from 0 to 10 (e.g. `7.0` for 3.5 stars),
/// and leaves it out for unrated tracks. A rating of 0 counts as unrated.
fn parse_rating(v: &Value) -> Option<u8> {
    let r = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    let r = r.round().clamp(0.0, 10.0) as u8;
    (r > 0).then_some(r)
}

/// Percent-encodes a value for use in a query string.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn items<'a>(v: &'a Value, kind: &str) -> impl Iterator<Item = &'a Value> {
    v["MediaContainer"][kind].as_array().into_iter().flatten()
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn describe(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(401, _) => anyhow!("Plex rejected the sign-in token (401). Run `plext login` again."),
        ureq::Error::Status(code, r) => anyhow!("Plex returned HTTP {} for {}", code, r.get_url()),
        ureq::Error::Transport(t) => anyhow!("network error: {}", t),
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_rating_parses_to_half_star_steps() {
        assert_eq!(parse_rating(&json!(7.0)), Some(7));
        assert_eq!(parse_rating(&json!(10)), Some(10));
        assert_eq!(parse_rating(&json!("8.0")), Some(8));
        assert_eq!(parse_rating(&json!(0.0)), None);
        assert_eq!(parse_rating(&Value::Null), None);
        assert_eq!(parse_track(&json!({"ratingKey": 5, "title": "Free", "userRating": 9.0})).rating, Some(9));
        assert_eq!(parse_track(&json!({"ratingKey": 5, "title": "Free"})).rating, None);
    }

    fn fake_server(routes: Vec<(&'static str, &'static str, Value)>) -> (Server, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let (base, log) = crate::test_server::start(routes);
        (Server::new(Plex::new("plext-test"), &base, "tok", "MACHINE"), log)
    }

    #[test]
    fn lists_this_librarys_audio_playlists_and_their_tracks() {
        let (server, log) = fake_server(vec![
            ("GET", "/playlists?", json!({"MediaContainer": {"Metadata": [
                {"ratingKey": "20", "title": "road trip", "playlistType": "audio", "leafCount": 2, "duration": 400000},
                {"ratingKey": "21", "title": "All Music", "playlistType": "audio", "smart": true, "leafCount": 900},
                {"ratingKey": "22", "title": "Movies", "playlistType": "video"},
            ]}})),
            ("GET", "/playlists/20/items", json!({"MediaContainer": {"totalSize": 3, "Metadata": [
                {"ratingKey": "5", "title": "Free", "type": "track", "playlistItemID": 101, "grandparentTitle": "Cat Power",
                 "Media": [{"Part": [{"key": "/library/parts/5/f.flac", "container": "flac"}]}]},
                {"ratingKey": "6", "title": "Werewolf", "type": "track", "playlistItemID": 102},
                {"ratingKey": "5", "title": "Free", "type": "track", "playlistItemID": 103},
            ]}})),
            ("GET", "/library/metadata/6", json!({"MediaContainer": {"Metadata": [
                {"ratingKey": "6", "title": "Werewolf", "Media": [{"Part": [{"key": "/library/parts/6/w.flac"}]}]},
            ]}})),
        ]);
        let lists = server.playlists("3").unwrap();
        assert_eq!(lists.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(), ["All Music", "road trip"]);
        assert!(lists[0].smart && !lists[1].smart);
        assert_eq!((lists[1].count, lists[1].duration_ms), (2, 400000));
        assert_eq!(log.lock().unwrap()[0], "GET /playlists?playlistType=audio&sectionID=3");

        let (pt, total) = server.playlist_page("20", 0, 500).unwrap();
        assert_eq!(total, 3);
        assert_eq!(pt.item_ids, [101, 102, 103]);
        assert_eq!(pt.tracks[0].artist, "Cat Power");
        assert_eq!(pt.tracks[0].part_key.as_deref(), Some("/library/parts/5/f.flac"));
        // One request for the page; no per-track detail requests.
        let l = log.lock().unwrap().clone();
        assert_eq!(l.last().unwrap(), "GET /playlists/20/items?X-Plex-Container-Start=0&X-Plex-Container-Size=500");
        assert_eq!(l.len(), 2);
        // A track that arrives without its media file can be looked up.
        assert_eq!(server.track("6").unwrap().part_key.as_deref(), Some("/library/parts/6/w.flac"));
    }

    #[test]
    fn playlist_edits_send_the_right_requests() {
        let (server, log) = fake_server(vec![(
            "POST",
            "/playlists?",
            json!({"MediaContainer": {"Metadata": [{"ratingKey": "30", "title": "Mix & Match", "leafCount": 2}]}}),
        )]);
        let keys: Vec<String> = (1..=150).map(|i| i.to_string()).collect();
        let created = server.create_playlist("Mix & Match", &keys).unwrap();
        assert_eq!((created.key.as_str(), created.count), ("30", 150));
        server.remove_from_playlist("30", 7).unwrap();
        server.move_in_playlist("30", 7, Some(9)).unwrap();
        server.move_in_playlist("30", 7, None).unwrap();
        server.rename_playlist("30", "Road trip").unwrap();
        server.delete_playlist("30").unwrap();
        let log = log.lock().unwrap();
        let uri = |range: std::ops::RangeInclusive<u32>| {
            encode(&format!(
                "server://MACHINE/com.plexapp.plugins.library/library/metadata/{}",
                range.map(|i| i.to_string()).collect::<Vec<_>>().join(",")
            ))
        };
        assert_eq!(log[0], format!("POST /playlists?type=audio&smart=0&title=Mix%20%26%20Match&uri={}", uri(1..=100)));
        assert_eq!(log[1], format!("PUT /playlists/30/items?uri={}", uri(101..=150)));
        assert_eq!(&log[2..], [
            "DELETE /playlists/30/items/7",
            "PUT /playlists/30/items/7/move?after=9",
            "PUT /playlists/30/items/7/move",
            "PUT /playlists/30?title=Road%20trip",
            "DELETE /playlists/30",
        ]);
        assert!(server.create_playlist("Empty", &[]).is_err());
    }
}
