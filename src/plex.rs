//! Minimal Plex client: link-code sign-in, server discovery and music library browsing.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

pub const PRODUCT: &str = "Plext";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq)]
pub struct Artist {
    pub key: String,
    pub title: String,
}

/// A row of things the server suggests ("Recently Played", "Stations"…).
#[derive(Debug, Clone, PartialEq)]
pub struct Hub {
    pub title: String,
    /// e.g. "music.stations.36"
    pub id: String,
    /// What the items are: "album", "artist", "station"…
    pub kind: String,
    /// Path for the full list (None when the hub is all there is).
    pub more: Option<String>,
    pub items: Vec<HubItem>,
}

impl Hub {
    pub fn is_stations(&self) -> bool {
        self.kind == "station" || self.id.starts_with("music.stations")
    }
}

/// One thing in a hub: an album, artist, station…
#[derive(Debug, Clone, PartialEq)]
pub struct HubItem {
    pub title: String,
    /// The artist, for an album or track.
    pub by: String,
    pub kind: String,
    pub key: String,
    /// Server path, e.g. "/library/sections/36/stations/1" for a station.
    pub path: String,
    pub year: Option<u32>,
}

/// An album found by a search, with who it's by.
#[derive(Debug, Clone, PartialEq)]
pub struct FoundAlbum {
    pub album: Album,
    pub artist: String,
}

/// What a search found in the library, best matches first.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub albums: Vec<FoundAlbum>,
    pub tracks: Vec<Track>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.artists.is_empty() && self.albums.is_empty() && self.tracks.is_empty()
    }
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

#[derive(Debug, Clone, PartialEq)]
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
    pub(crate) fn from_metadata(m: &Value) -> ReleaseKind {
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
            // See `Checked`: Plex never redirects, and a followed redirect
            // would carry the token to wherever it points.
            .redirects(0)
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
        let resp = self.req("GET", url, token).call().checked()?;
        let v: Value = resp.into_json().context("invalid JSON from Plex")?;
        Ok(v)
    }

    // ---------- sign-in ----------

    /// Requests a 4-character link code. Returns (pin id, code).
    pub fn create_pin(&self) -> Result<(u64, String)> {
        let resp = self
            .req("POST", "https://plex.tv/api/v2/pins", None)
            .send_form(&[("strong", "false")])
            .checked()?;
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
        parse_resources(&v, account_token)
    }

    /// Tries each candidate URL in order (encrypted local addresses first)
    /// and returns the first that reaches this very server.
    pub fn find_working_url(&self, res: &Resource) -> Option<String> {
        res.urls
            .iter()
            .find(|u| self.verify(u, &res.machine_id).is_ok())
            .cloned()
    }

    /// The machine identifier of the Plex server answering at `base`.
    /// Asked without the token: Plex tells anyone who it is, so nothing
    /// secret is sent before we know who is listening.
    pub fn identity(&self, base: &str) -> Result<String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(4))
            .redirects(0)
            .build();
        let v: Value = agent
            .get(&format!("{}/identity", base.trim_end_matches('/')))
            .set("Accept", "application/json")
            .call()
            .checked()?
            .into_json()
            .with_context(|| format!("no Plex server at {}", base))?;
        v["MediaContainer"]["machineIdentifier"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| anyhow!("no Plex server at {}", base))
    }

    /// Ok when `base` reaches the server `machine_id` and no other, so the
    /// token is only ever sent to the server it belongs to (a saved LAN
    /// address can belong to some other machine on another network).
    pub fn verify(&self, base: &str, machine_id: &str) -> Result<()> {
        let found = self.identity(base)?;
        if machine_id.is_empty() || found != machine_id {
            bail!("a different Plex server answered at {}", base);
        }
        Ok(())
    }
}

/// The account's servers from plex.tv's resources answer, each with its
/// addresses best first: encrypted local ones, plain-http local ones,
/// remote, then relay (last, as it's too slow for lossless audio).
fn parse_resources(v: &Value, account_token: &str) -> Result<Vec<Resource>> {
    let arr = v.as_array().ok_or_else(|| anyhow!("unexpected resources response"))?;
    let mut out = Vec::new();
    for r in arr {
        let provides = r["provides"].as_str().unwrap_or("");
        if !provides.split(',').any(|p| p == "server") {
            continue;
        }
        let mut local = Vec::new();
        let mut local_http = Vec::new();
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
                // Plain http on the LAN also works when plex.direct DNS
                // is blocked by the router. Tried only after every
                // encrypted local address.
                if let (Some(addr), Some(port)) = (c["address"].as_str(), c["port"].as_u64()) {
                    local_http.push(format!("http://{}:{}", addr, port));
                }
            } else {
                remote.push(uri);
            }
        }
        let mut urls = local;
        urls.extend(local_http);
        urls.extend(remote);
        urls.extend(relay);
        // Each address once, at its best place.
        let mut seen = std::collections::HashSet::new();
        urls.retain(|u| seen.insert(u.clone()));
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

/// Every Plex response goes through `checked`: errors in words, and
/// redirects refused. Plex never redirects, and a followed one would carry
/// the X-Plex-Token header (which ureq doesn't strip) to wherever it points,
/// even from https to plain http.
trait Checked {
    fn checked(self) -> Result<ureq::Response>;
}

impl Checked for std::result::Result<ureq::Response, ureq::Error> {
    fn checked(self) -> Result<ureq::Response> {
        let resp = self.map_err(describe)?;
        if (300..400).contains(&resp.status()) {
            bail!(
                "Plex answered {} with a redirect (to {}), which Plext doesn't follow",
                resp.get_url(),
                resp.header("Location").unwrap_or("nowhere")
            );
        }
        Ok(resp)
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
            .checked()?;
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
            .checked()?;
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
            .checked()?;
        Ok(())
    }

    /// The full URL of `path` on this server. Paths that come from the
    /// server (hub and station keys, media files) must start with "/":
    /// glued onto the base, anything else could name another host
    /// ("@example.com/…" or ".example.com/…").
    fn url(&self, path: &str) -> Result<String> {
        if !path.starts_with('/') {
            bail!("Plex sent an unexpected path: {:?}", path);
        }
        Ok(format!("{}{}", self.base, path))
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.plex.get_json(&self.url(path)?, Some(&self.token))
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
    /// The artist (key, name) of a track, from its details.
    pub fn track_artist(&self, key: &str) -> Result<(String, String)> {
        let v = self.get(&format!("/library/metadata/{}", encode(key)))?;
        let m = items(&v, "Metadata").next().ok_or_else(|| anyhow!("track {} not found", key))?;
        let artist = str_of(&m["grandparentRatingKey"]);
        if artist.is_empty() {
            bail!("no artist for track {}", key);
        }
        Ok((artist, str_of(&m["grandparentTitle"])))
    }

    pub fn track(&self, key: &str) -> Result<Track> {
        let v = self.get(&format!("/library/metadata/{}", encode(key)))?;
        let t = items(&v, "Metadata").next().map(parse_track);
        t.ok_or_else(|| anyhow!("track {} not found", key))
    }

    // ---------- search ----------

    /// The path `search` asks, for `plext inspect search`.
    pub fn search_path(library_key: &str, query: &str, limit: usize) -> String {
        format!("/hubs/search?query={}&sectionId={}&limit={}", encode(query), encode(library_key), limit)
    }

    /// Artists, albums and tracks in this library matching `query` (up to
    /// `limit` of each), as Plex ranks them. Plex matches words anywhere in
    /// titles, and finds tracks by artist or album name too.
    ///
    /// Some servers' search leaves its track list empty (Noah's does, on
    /// Plex 1.43); then tracks whose title contains the words are fetched
    /// from the library directly.
    pub fn search(&self, library_key: &str, query: &str, limit: usize) -> Result<SearchResults> {
        let v = self.get(&Self::search_path(library_key, query, limit))?;
        let mut found = parse_search(&v, library_key);
        if found.tracks.is_empty() {
            if let Ok(v) = self.get(&Self::track_title_path(library_key, query, limit)) {
                found.tracks = tracks_titled(&v, query);
            }
        }
        Ok(found)
    }

    /// Tracks in the library whose title contains `query` (Plex's `title`
    /// filter matches anywhere in the title, ignoring case).
    pub fn track_title_path(library_key: &str, query: &str, limit: usize) -> String {
        format!(
            "/library/sections/{}/all?type=10&title={}&X-Plex-Container-Start=0&X-Plex-Container-Size={}",
            encode(library_key),
            encode(query),
            limit
        )
    }

    // ---------- discover: hubs, stations, sonic features ----------

    /// The library's suggestion rows, stations included.
    pub fn hubs(&self, library_key: &str) -> Result<Vec<Hub>> {
        let v = self.get(&format!("/hubs/sections/{}?includeStations=1&count=12", encode(library_key)))?;
        Ok(items(&v, "Hub")
            .map(|h| {
                let mut entries: Vec<HubItem> = h["Metadata"].as_array().into_iter().flatten().map(parse_hub_item).collect();
                entries.extend(h["Directory"].as_array().into_iter().flatten().map(parse_hub_item));
                Hub {
                    title: str_of(&h["title"]),
                    id: str_of(&h["hubIdentifier"]),
                    kind: str_of(&h["type"]),
                    more: h["key"].as_str().filter(|k| !k.is_empty() && h["more"].as_bool().unwrap_or(false)).map(String::from),
                    items: entries,
                }
            })
            .collect())
    }

    /// The full list behind a hub's "more" path.
    pub fn hub_items(&self, path: &str) -> Result<Vec<HubItem>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let v = self.get(&format!("{}{}X-Plex-Container-Start=0&X-Plex-Container-Size=100", path, sep))?;
        let mut out: Vec<HubItem> = items(&v, "Metadata").map(parse_hub_item).collect();
        out.extend(items(&v, "Directory").map(parse_hub_item));
        Ok(out)
    }

    /// A batch of tracks from a station. Each call starts the station
    /// afresh on the server, which picks a new set.
    pub fn station_tracks(&self, station_path: &str) -> Result<Vec<Track>> {
        let uri = format!("server://{}/com.plexapp.plugins.library{}", self.machine_id, station_path);
        let v = self.send(
            "POST",
            &format!("/playQueues?type=audio&shuffle=0&repeat=0&continuous=0&own=1&uri={}", encode(&uri)),
        )?;
        Ok(items(&v, "Metadata").filter(|m| m["type"].as_str().map_or(true, |t| t == "track")).map(parse_track).collect())
    }

    /// Tracks that sound like this one (Plex's sonic analysis), nearest first.
    pub fn similar_tracks(&self, track_key: &str, limit: usize, max_distance: f32) -> Result<Vec<Track>> {
        let v = self.get(&format!(
            "/library/metadata/{}/nearest?limit={}&maxDistance={}",
            encode(track_key),
            limit,
            max_distance
        ))?;
        Ok(items(&v, "Metadata").map(parse_track).collect())
    }

    /// Artists that sound like this one: (key, name), nearest first.
    pub fn similar_artists(&self, artist_key: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let v = self.get(&format!("/library/metadata/{}/nearest?limit={}&maxDistance=0.25", encode(artist_key), limit))?;
        Ok(items(&v, "Metadata")
            .filter(|m| m["type"].as_str().map_or(true, |t| t == "artist"))
            .map(|m| (str_of(&m["ratingKey"]), str_of(&m["title"])))
            .collect())
    }

    /// Every track by an artist.
    pub fn artist_tracks(&self, artist_key: &str) -> Result<Vec<Track>> {
        let v = self.get(&format!("/library/metadata/{}/allLeaves", encode(artist_key)))?;
        Ok(items(&v, "Metadata").map(parse_track).collect())
    }

    /// A path of tracks that moves smoothly from one track's sound to another's.
    pub fn sonic_path(&self, library_key: &str, from: &str, to: &str) -> Result<Vec<Track>> {
        let v = self.get(&format!(
            "/library/sections/{}/computePath?startID={}&endID={}",
            encode(library_key),
            encode(from),
            encode(to)
        ))?;
        Ok(items(&v, "Metadata").map(parse_track).collect())
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
        let url = self.url(path_and_query)?;
        let resp = self
            .plex
            .req(method, &url, Some(&self.token))
            .timeout(Duration::from_secs(20))
            .call()
            .checked()?;
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
            .checked()?;
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
        let url = self.url(part_key)?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .redirects(0)
            .build();
        let mut req = agent
            .get(&url)
            .set("X-Plex-Token", &self.token)
            .set("X-Plex-Client-Identifier", &self.plex.client_id)
            .set("X-Plex-Product", PRODUCT);
        if offset > 0 {
            req = req.set("Range", &format!("bytes={}-", offset));
        }
        let resp = req.call().checked()?;
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

/// The music hubs of a `/hubs/search` answer. Results from another
/// library (a server may ignore `sectionId`) are left out.
pub(crate) fn parse_search(v: &Value, library_key: &str) -> SearchResults {
    let mut out = SearchResults::default();
    for h in items(v, "Hub") {
        let entries = h["Metadata"].as_array().into_iter().flatten().chain(h["Directory"].as_array().into_iter().flatten());
        for m in entries {
            let section = str_of(&m["librarySectionID"]);
            if !section.is_empty() && section != library_key {
                continue;
            }
            let kind = m["type"].as_str().unwrap_or_else(|| h["type"].as_str().unwrap_or(""));
            match kind {
                "artist" => out.artists.push(Artist { key: str_of(&m["ratingKey"]), title: str_of(&m["title"]) }),
                "album" => out.albums.push(FoundAlbum {
                    album: Album {
                        key: str_of(&m["ratingKey"]),
                        title: str_of(&m["title"]),
                        year: m["year"].as_u64().map(|y| y as u32),
                        kind: ReleaseKind::from_metadata(m),
                    },
                    artist: str_of(&m["parentTitle"]),
                }),
                "track" => out.tracks.push(parse_track(m)),
                _ => {}
            }
        }
    }
    // The same item can come back in more than one hub.
    let mut seen = std::collections::HashSet::new();
    out.artists.retain(|a| seen.insert(format!("ar{}", a.key)));
    out.albums.retain(|a| seen.insert(format!("al{}", a.album.key)));
    out.tracks.retain(|t| seen.insert(format!("tr{}", t.key)));
    out
}

/// The tracks of a title-filter answer, closest titles first: exactly the
/// words, then titles starting with them, then the rest in Plex's order.
pub(crate) fn tracks_titled(v: &Value, query: &str) -> Vec<Track> {
    let q = query.trim().to_lowercase();
    let mut tracks: Vec<Track> = items(v, "Metadata")
        .filter(|m| m["type"].as_str().is_none_or(|t| t == "track"))
        .map(parse_track)
        .collect();
    tracks.sort_by_key(|t| {
        let title = t.title.to_lowercase();
        (title != q, !title.starts_with(&q))
    });
    tracks
}

fn parse_hub_item(m: &Value) -> HubItem {
    let by = ["grandparentTitle", "parentTitle"]
        .iter()
        .map(|k| str_of(&m[*k]))
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    HubItem {
        title: str_of(&m["title"]),
        by,
        kind: str_of(&m["type"]),
        key: str_of(&m["ratingKey"]),
        path: str_of(&m["key"]),
        year: m["year"].as_u64().map(|y| y as u32),
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

    /// A server answering every request with `reply(path)` (status line,
    /// extra header lines, body), recording each request's line and headers.
    fn raw_server(
        reply: impl Fn(&str) -> (&'static str, String, String) + Send + Sync + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (seen, reply) = (log.clone(), std::sync::Arc::new(reply));
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                seen.lock().unwrap().push(head);
                let (status, headers, body) = reply(&path);
                let mut w = conn;
                let _ = write!(
                    w,
                    "HTTP/1.1 {}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status, headers, body.len(), body
                );
            }
        });
        (base, log)
    }

    fn sent_token(log: &std::sync::Mutex<Vec<String>>) -> bool {
        log.lock().unwrap().iter().any(|h| h.to_ascii_lowercase().contains("x-plex-token"))
    }

    fn identity_server(machine_id: &'static str) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        raw_server(move |_| ("200 OK", String::new(), json!({"MediaContainer": {"machineIdentifier": machine_id}}).to_string()))
    }

    #[test]
    fn the_token_goes_only_to_the_server_it_belongs_to() {
        let plex = Plex::new("plext-test");
        let (ours, ours_log) = identity_server("MACHINE");
        let (other, other_log) = identity_server("SOMEONE-ELSE");
        assert!(plex.verify(&ours, "MACHINE").is_ok());
        assert!(plex.verify(&other, "MACHINE").is_err(), "another server at a saved address");
        assert!(plex.verify(&ours, "").is_err(), "a server we can't identify");
        // Picking an address skips the impostor and lands on ours.
        let res = Resource {
            name: "Home".into(),
            machine_id: "MACHINE".into(),
            token: "secret".into(),
            owned: true,
            urls: vec![other.clone(), ours.clone()],
        };
        assert_eq!(plex.find_working_url(&res), Some(ours));
        // Checking who's there never sends the token, to either of them.
        assert!(!sent_token(&ours_log) && !sent_token(&other_log));
    }

    #[test]
    fn redirects_are_refused_so_the_token_stays_put() {
        let (elsewhere, elsewhere_log) = identity_server("X");
        let to = elsewhere.clone();
        let (base, _) = raw_server(move |path| {
            ("302 Found", format!("Location: {}{}\r\n", to, path), String::new())
        });
        let server = Server::new(Plex::new("plext-test"), &base, "secret", "MACHINE");
        let e = server.music_libraries().unwrap_err().to_string();
        assert!(e.contains("redirect"), "{}", e);
        assert!(server.open_media("/library/parts/1/f.flac", 0).is_err());
        assert!(Plex::new("plext-test").identity(&base).is_err());
        assert!(elsewhere_log.lock().unwrap().is_empty(), "nothing followed the redirect");
    }

    #[test]
    fn paths_from_the_server_must_be_paths_on_it() {
        let (base, log) = raw_server(|_| ("200 OK", String::new(), "{}".into()));
        let server = Server::new(Plex::new("plext-test"), &base, "secret", "MACHINE");
        // Glued onto "http://host:port", these would name another host.
        assert!(server.hub_items("@example.com/hubs").is_err());
        assert!(server.open_media(".example.com/f.flac", 0).is_err());
        assert!(server.get_raw("example.com").is_err());
        assert!(log.lock().unwrap().is_empty());
        // Real paths still work.
        assert!(server.hub_items("/hubs/sections/1").is_ok());
    }

    #[test]
    fn addresses_are_tried_encrypted_first_and_relay_last() {
        let v = json!([{
            "name": "Home", "clientIdentifier": "M", "provides": "server", "accessToken": "t", "owned": true,
            "connections": [
                {"uri": "https://relay.plex.direct:8443", "relay": true},
                {"uri": "https://1-2-3-4.abc.plex.direct:32400", "local": false, "address": "1.2.3.4", "port": 32400},
                {"uri": "https://192-168-1-10.abc.plex.direct:32400", "local": true, "address": "192.168.1.10", "port": 32400},
                {"uri": "https://10-0-0-5.abc.plex.direct:32400", "local": true, "address": "10.0.0.5", "port": 32400},
                {"uri": "http://10.0.0.5:32400", "local": true, "address": "10.0.0.5", "port": 32400},
            ]
        }, {"name": "A player", "provides": "player", "connections": []}]);
        let res = parse_resources(&v, "account").unwrap();
        assert_eq!(res.len(), 1, "only servers");
        assert_eq!(
            res[0].urls,
            [
                "https://192-168-1-10.abc.plex.direct:32400",
                "https://10-0-0-5.abc.plex.direct:32400",
                "http://10.0.0.5:32400",
                "http://192.168.1.10:32400",
                "https://1-2-3-4.abc.plex.direct:32400",
                "https://relay.plex.direct:8443",
            ]
        );
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

    #[test]
    fn discover_calls_parse_what_the_server_returns() {
        let t = |k: &str, title: &str| json!({"ratingKey": k, "title": title, "type": "track", "grandparentTitle": "Cat Power",
            "Media": [{"Part": [{"key": format!("/library/parts/{}/f.flac", k)}]}]});
        let (server, log) = fake_server(vec![
            ("GET", "/hubs/sections/36?", json!({"MediaContainer": {"Hub": [
                {"title": "Stations", "hubIdentifier": "music.stations.36", "type": "station", "more": false, "Metadata": [
                    {"title": "Deep Cuts Radio", "type": "playlist", "key": "/library/sections/36/stations/8", "radio": 1}]},
                {"title": "Recently Played Music", "hubIdentifier": "music.recent.played.36", "type": "artist", "more": true,
                 "key": "/library/sections/36/all?viewCount>=1&type=8", "Metadata": [{"title": "The Beatles", "type": "artist", "ratingKey": "186427", "key": "/library/metadata/186427/children"}]},
            ]}})),
            ("GET", "/library/sections/36/all?viewCount", json!({"MediaContainer": {"Metadata": [
                {"title": "Help!", "type": "album", "ratingKey": "186609", "parentTitle": "The Beatles", "year": 1965}]}})),
            ("POST", "/playQueues?", json!({"MediaContainer": {"playQueueID": 9, "Metadata": [t("1", "Free"), t("2", "Werewolf")]}})),
            ("GET", "/library/metadata/1/nearest", json!({"MediaContainer": {"Metadata": [t("5", "Lions")]}})),
            ("GET", "/library/metadata/77/nearest", json!({"MediaContainer": {"Metadata": [{"ratingKey": "78", "title": "The Staves", "type": "artist"}]}})),
            ("GET", "/library/metadata/77/allLeaves", json!({"MediaContainer": {"Metadata": [t("1", "Free"), t("3", "Fool")]}})),
            ("GET", "/library/sections/36/computePath", json!({"MediaContainer": {"Metadata": [t("1", "Free"), t("5", "Lions"), t("9", "Old Flowers")]}})),
        ]);
        let hubs = server.hubs("36").unwrap();
        assert!(hubs[0].is_stations() && !hubs[1].is_stations());
        assert_eq!(hubs[0].items[0].path, "/library/sections/36/stations/8");
        assert_eq!(hubs[0].more, None);
        assert_eq!(hubs[1].more.as_deref(), Some("/library/sections/36/all?viewCount>=1&type=8"));
        let more = server.hub_items(hubs[1].more.as_ref().unwrap()).unwrap();
        assert_eq!((more[0].title.as_str(), more[0].by.as_str(), more[0].year), ("Help!", "The Beatles", Some(1965)));

        let st = server.station_tracks("/library/sections/36/stations/8").unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st[1].part_key.as_deref(), Some("/library/parts/2/f.flac"));
        assert_eq!(server.similar_tracks("1", 25, 0.25).unwrap()[0].title, "Lions");
        assert_eq!(server.similar_artists("77", 10).unwrap(), [("78".to_string(), "The Staves".to_string())]);
        assert_eq!(server.artist_tracks("77").unwrap().len(), 2);
        assert_eq!(server.sonic_path("36", "1", "9").unwrap().len(), 3);

        let l = log.lock().unwrap().clone();
        let post = l.iter().find(|x| x.starts_with("POST")).unwrap();
        assert!(post.contains("uri=server%3A%2F%2FMACHINE%2Fcom.plexapp.plugins.library%2Flibrary%2Fsections%2F36%2Fstations%2F8"), "{}", post);
        assert!(l.iter().any(|x| x == "GET /library/metadata/1/nearest?limit=25&maxDistance=0.25"), "{:?}", l);
        assert!(l.iter().any(|x| x == "GET /library/sections/36/computePath?startID=1&endID=9"), "{:?}", l);
        assert!(l.iter().any(|x| x.starts_with("GET /library/sections/36/all?viewCount%3E=1&type=8&X-Plex-Container-Start=0")), "{:?}", l);
    }

    #[test]
    fn when_search_finds_no_tracks_they_are_looked_up_by_title() {
        // As Noah's server answers: albums and artists, but no tracks.
        let (server, log) = fake_server(vec![
            ("GET", "/hubs/search?", json!({"MediaContainer": {"Hub": [
                {"type": "album", "hubIdentifier": "album", "Metadata": [
                    {"ratingKey": "179182", "title": "Carrying Your Love with Me", "type": "album",
                     "parentTitle": "George Strait", "librarySectionID": 36}]},
                {"type": "track", "hubIdentifier": "track", "size": 0},
                {"type": "movie", "hubIdentifier": "movie", "Metadata": [
                    {"ratingKey": "1", "title": "Carrying On", "type": "movie", "librarySectionID": 1}]},
            ]}})),
            ("GET", "/library/sections/36/all?type=10", json!({"MediaContainer": {"Metadata": [
                {"ratingKey": "9", "title": "Still Carrying Your Love", "type": "track", "grandparentTitle": "Someone"},
                {"ratingKey": "8", "title": "Carrying Your Love with Me", "type": "track", "grandparentTitle": "George Strait",
                 "parentTitle": "Carrying Your Love with Me", "parentRatingKey": "179182"},
                {"ratingKey": "7", "title": "carrying your love", "type": "track"},
            ]}})),
        ]);
        let r = server.search("36", "Carrying Your Love", 30).unwrap();
        assert_eq!(r.albums.len(), 1);
        let titles: Vec<&str> = r.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["carrying your love", "Carrying Your Love with Me", "Still Carrying Your Love"]);
        let l = log.lock().unwrap().clone();
        assert_eq!(l[1], "GET /library/sections/36/all?type=10&title=Carrying%20Your%20Love&X-Plex-Container-Start=0&X-Plex-Container-Size=30");
    }

    #[test]
    fn search_finds_artists_albums_and_tracks_in_this_library() {
        let (server, log) = fake_server(vec![("GET", "/hubs/search?", json!({"MediaContainer": {"Hub": [
            {"type": "artist", "hubIdentifier": "artist", "Metadata": [
                {"ratingKey": "77", "title": "Cat Power", "type": "artist", "librarySectionID": 36}]},
            {"type": "album", "hubIdentifier": "album", "Metadata": [
                {"ratingKey": "100", "title": "You Are Free", "type": "album", "parentTitle": "Cat Power", "year": 2003,
                 "librarySectionID": 36},
                {"ratingKey": "300", "title": "Free (audiobook)", "type": "album", "librarySectionID": 12}]},
            {"type": "track", "hubIdentifier": "track", "Metadata": [
                {"ratingKey": "5", "title": "Free", "type": "track", "grandparentTitle": "Cat Power",
                 "parentTitle": "You Are Free", "parentRatingKey": "100", "librarySectionID": "36"}]},
            {"type": "track", "hubIdentifier": "track.lyrics", "Metadata": [
                {"ratingKey": "5", "title": "Free", "type": "track", "librarySectionID": 36}]},
            {"type": "genre", "hubIdentifier": "genre", "Directory": [{"title": "Free jazz", "type": "genre"}]},
        ]}}))]);
        let r = server.search("36", "free & easy", 20).unwrap();
        assert_eq!(r.artists, vec![Artist { key: "77".into(), title: "Cat Power".into() }]);
        assert_eq!(r.albums.len(), 1, "other libraries and genres are left out");
        assert_eq!((r.albums[0].album.title.as_str(), r.albums[0].artist.as_str(), r.albums[0].album.year), ("You Are Free", "Cat Power", Some(2003)));
        assert_eq!(r.tracks.len(), 1, "a track found twice is listed once");
        assert_eq!((r.tracks[0].title.as_str(), r.tracks[0].album_key.as_deref()), ("Free", Some("100")));
        let l = log.lock().unwrap().clone();
        assert_eq!(l, vec!["GET /hubs/search?query=free%20%26%20easy&sectionId=36&limit=20"]);
    }
}
