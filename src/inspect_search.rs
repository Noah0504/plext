//! `plext inspect search <words>`: asks the server's search what it finds,
//! and prints both what came back and what Plext makes of it, so search
//! can be checked against a real library.
//!
//! It only reads. Paths are printed without the sign-in token.

use crate::plex::{parse_search, tracks_titled, Server};
use serde_json::Value;
use std::fmt::Write;
use std::time::Instant;

/// Items shown per hub.
const SHOW: usize = 8;
/// Results asked for per kind (the same as the Search view asks for).
pub const LIMIT: usize = crate::app::SEARCH_LIMIT;

pub fn report(server: &Server, library_key: &str, query: &str) -> String {
    let mut out = String::new();
    let o = &mut out;
    let _ = writeln!(o, "Plext {} — search check for \"{}\"", env!("CARGO_PKG_VERSION"), query);
    let _ = writeln!(o, "(Read-only. Paste everything from here down into the chat.)\n");

    let path = Server::search_path(library_key, query, LIMIT);
    let _ = writeln!(o, "== Server's answer ({})", path);
    let started = Instant::now();
    let v = match server.get_raw(&path) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(o, "  FAILED: {:#}", e);
            return out;
        }
    };
    let _ = writeln!(o, "  answered in {} ms", started.elapsed().as_millis());
    raw_hubs(o, &v);

    let mut r = parse_search(&v, library_key);

    // Plext's fallback when the server's search returns no tracks.
    let path = Server::track_title_path(library_key, query, LIMIT);
    let _ = writeln!(o, "\n== Tracks by title ({})", path);
    let started = Instant::now();
    match server.get_raw(&path) {
        Ok(tv) => {
            let tracks = tracks_titled(&tv, query);
            let _ = writeln!(o, "  answered in {} ms: {} tracks", started.elapsed().as_millis(), tracks.len());
            if r.tracks.is_empty() {
                r.tracks = tracks;
            } else {
                let _ = writeln!(o, "  (not used: the search itself found tracks)");
            }
        }
        Err(e) => {
            let _ = writeln!(o, "  FAILED: {:#}", e);
        }
    }

    let _ = writeln!(o, "\n== What the Search view shows");
    let _ = writeln!(o, "  Artists ({})", r.artists.len());
    for a in r.artists.iter().take(SHOW) {
        let _ = writeln!(o, "    {}  [{}]", a.title, a.key);
    }
    let _ = writeln!(o, "  Albums ({})", r.albums.len());
    for a in r.albums.iter().take(SHOW) {
        let year = a.album.year.map(|y| format!(" · {}", y)).unwrap_or_default();
        let _ = writeln!(o, "    {} — {}{}  [{}]", a.album.title, a.artist, year, a.album.key);
    }
    let _ = writeln!(o, "  Tracks ({})", r.tracks.len());
    for t in r.tracks.iter().take(SHOW) {
        let playable = if t.part_key.is_some() { "" } else { "  (no media file listed)" };
        let _ = writeln!(o, "    {} — {} · {}  [{}]{}", t.title, t.artist, t.album, t.key, playable);
    }
    if r.is_empty() {
        let _ = writeln!(o, "  (nothing)");
    }
    out
}

/// Every hub the server sent, music or not, with its first items.
fn raw_hubs(o: &mut String, v: &Value) {
    let hubs = v["MediaContainer"]["Hub"].as_array().cloned().unwrap_or_default();
    if hubs.is_empty() {
        let keys: Vec<String> = v["MediaContainer"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        let _ = writeln!(o, "  no hubs; MediaContainer has: {}", keys.join(", "));
        return;
    }
    for h in &hubs {
        let entries: Vec<&Value> = ["Metadata", "Directory"]
            .iter()
            .flat_map(|k| h[*k].as_array().into_iter().flatten())
            .collect();
        if entries.is_empty() {
            continue;
        }
        let _ = writeln!(
            o,
            "  hub \"{}\" type={} id={} size={}",
            text(&h["title"]),
            text(&h["type"]),
            text(&h["hubIdentifier"]),
            entries.len()
        );
        for m in entries.iter().take(SHOW) {
            let by = ["grandparentTitle", "parentTitle"].iter().map(|k| text(&m[*k])).find(|s| !s.is_empty()).unwrap_or_default();
            let _ = writeln!(
                o,
                "    {} \"{}\"{} key={} section={}{}",
                text(&m["type"]),
                text(&m["title"]),
                if by.is_empty() { String::new() } else { format!(" by \"{}\"", by) },
                text(&m["ratingKey"]),
                text(&m["librarySectionID"]),
                if m["Media"].is_array() { " +media" } else { "" }
            );
        }
    }
    let empty: Vec<String> = hubs
        .iter()
        .filter(|h| h["Metadata"].as_array().is_none_or(|a| a.is_empty()) && h["Directory"].as_array().is_none_or(|a| a.is_empty()))
        .map(|h| text(&h["hubIdentifier"]))
        .collect();
    if !empty.is_empty() {
        let _ = writeln!(o, "  empty hubs: {}", empty.join(", "));
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        x => x.to_string(),
    }
}
