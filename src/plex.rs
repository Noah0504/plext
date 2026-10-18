//! Minimal Plex client: link-code sign-in, server discovery and music library browsing.

use anyhow::{anyhow, Context, Result};
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

#[derive(Debug, Clone)]
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

        // The children listing usually lacks bit depth / sample rate. A batched
        // metadata request for all tracks returns the full stream details.
        let keys: Vec<&str> = tracks.iter().map(|t| t.key.as_str()).collect();
        if !keys.is_empty() {
            if let Ok(detail) = self.get(&format!("/library/metadata/{}", keys.join(","))) {
                for d in items(&detail, "Metadata") {
                    let full = parse_track(d);
                    if let Some(t) = tracks.iter_mut().find(|t| t.key == full.key) {
                        *t = merge(t.clone(), full);
                    }
                }
            }
        }
        tracks.sort_by_key(|t| (t.disc.unwrap_or(1), t.index.unwrap_or(u32::MAX)));
        Ok(tracks)
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
}
