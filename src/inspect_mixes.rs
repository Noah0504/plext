//! `plext inspect mixes`: asks the server what it offers for mixes and
//! stations, and prints what comes back, so the Mixes view can be built
//! around what servers actually return. Plex doesn't document these
//! endpoints; they're the ones Plexamp and python-plexapi use.
//!
//! It only reads. Paths are printed without the sign-in token.

use crate::plex::Server;
use serde_json::Value;
use std::fmt::Write;

/// Items shown per hub or result list.
const SHOW: usize = 5;

/// Runs every check and returns the report.
pub fn report(server: &Server, library_key: &str) -> String {
    let mut out = String::new();
    let o = &mut out;
    let _ = writeln!(o, "Plext {} — mixes check", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(o, "(Read-only. Paste everything from here down into the chat.)\n");

    // ---- the server
    section(o, "Server", "/");
    match server.get_raw("/") {
        Ok(v) => {
            let mc = &v["MediaContainer"];
            field(o, "name", &mc["friendlyName"]);
            field(o, "version", &mc["version"]);
            field(o, "platform", &mc["platform"]);
            field(o, "Plex Pass", &mc["myPlexSubscription"]);
        }
        Err(e) => fail(o, &e),
    }

    // ---- the library and its sonic-analysis setting
    let lib = format!("/library/sections/{}", library_key);
    section(o, "Library", &lib);
    match server.get_raw(&lib) {
        Ok(v) => {
            let d = first(&v["MediaContainer"], &["Directory", "Metadata"]).unwrap_or(&v["MediaContainer"]);
            field(o, "title", &d["title"]);
            field(o, "agent", &d["agent"]);
            field(o, "scanner", &d["scanner"]);
        }
        Err(e) => fail(o, &e),
    }
    let prefs = format!("{}/prefs", lib);
    match server.get_raw(&prefs) {
        Ok(v) => {
            let settings: Vec<&Value> = list(&v["MediaContainer"], "Setting")
                .into_iter()
                .filter(|s| {
                    let id = s["id"].as_str().unwrap_or("").to_lowercase();
                    id.contains("sonic") || id.contains("analysis") || id.contains("loudness")
                })
                .collect();
            if settings.is_empty() {
                let _ = writeln!(o, "  prefs: no sonic-analysis settings listed");
            }
            for s in settings {
                let _ = writeln!(o, "  pref {} = {}", text(&s["id"]), text(&s["value"]));
            }
        }
        Err(e) => {
            let _ = writeln!(o, "  prefs ({}): {}", prefs, e);
        }
    }

    // ---- sample tracks: are they sonically analysed?
    let sample_path = format!("{}/all?type=10&X-Plex-Container-Start=0&X-Plex-Container-Size=50", lib);
    section(o, "Sample of 50 tracks", &sample_path);
    let sample: Vec<Value> = match server.get_raw(&sample_path) {
        Ok(v) => {
            let mc = &v["MediaContainer"];
            field(o, "tracks in library", &mc["totalSize"]);
            list(mc, "Metadata").into_iter().cloned().collect()
        }
        Err(e) => {
            fail(o, &e);
            Vec::new()
        }
    };
    let analysed: Vec<&Value> = sample.iter().filter(|t| !t["musicAnalysisVersion"].is_null()).collect();
    let _ = writeln!(o, "  with musicAnalysisVersion: {} of {}", analysed.len(), sample.len());
    if let Some(t) = analysed.first().copied().or(sample.first()) {
        let _ = writeln!(o, "  example: {} (musicAnalysisVersion {})", describe(t), text(&t["musicAnalysisVersion"]));
    }

    // ---- hubs: "Mixes for You", stations, and whatever else the library home offers
    for path in [
        format!("/hubs/sections/{}?includeStations=1&count={}", library_key, SHOW),
        format!("/hubs/promoted?contentDirectoryID={}&includeStations=1&count={}", library_key, SHOW),
    ] {
        section(o, "Hubs", &path);
        match server.get_raw(&path) {
            Ok(v) => hubs(o, &v),
            Err(e) => fail(o, &e),
        }
    }

    // ---- sonic features on a sample track
    let pick = analysed.first().copied().or(sample.first());
    if let Some(t) = pick {
        let key = text(&t["ratingKey"]);
        let near = format!("/library/metadata/{}/nearest?limit=10&maxDistance=0.25", key);
        section(o, &format!("Sonically similar to {}", describe(t)), &near);
        items_of(o, server, &near);

        let artist = text(&t["grandparentRatingKey"]);
        if !artist.is_empty() {
            let sim = format!("/library/metadata/{}/similar?limit=10", artist);
            section(o, &format!("Artists similar to {}", text(&t["grandparentTitle"])), &sim);
            items_of(o, server, &sim);
            let sonic = format!("/library/metadata/{}/nearest?limit=10&maxDistance=0.25", artist);
            section(o, &format!("Artists sonically similar to {}", text(&t["grandparentTitle"])), &sonic);
            items_of(o, server, &sonic);
        }
    }
    let (from, to) = match (analysed.first(), analysed.last()) {
        (Some(a), Some(b)) if text(&a["ratingKey"]) != text(&b["ratingKey"]) => (Some(*a), Some(*b)),
        _ => (sample.first(), sample.last()),
    };
    if let (Some(a), Some(b)) = (from, to) {
        let path = format!(
            "{}/computePath?startID={}&endID={}",
            lib,
            text(&a["ratingKey"]),
            text(&b["ratingKey"])
        );
        section(o, &format!("Sonic adventure from {} to {}", describe(a), describe(b)), &path);
        items_of(o, server, &path);
    }

    let _ = writeln!(o, "\nDone.");
    out
}

fn section(o: &mut String, title: &str, path: &str) {
    let _ = writeln!(o, "\n== {}  [{}]", title, path);
}

fn field(o: &mut String, name: &str, v: &Value) {
    let _ = writeln!(o, "  {}: {}", name, if v.is_null() { "(not given)".into() } else { text(v) });
}

fn fail(o: &mut String, e: &anyhow::Error) {
    let _ = writeln!(o, "  failed: {}", e);
}

/// A value as plain text (strings without quotes).
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn list<'a>(mc: &'a Value, kind: &str) -> Vec<&'a Value> {
    mc[kind].as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

fn first<'a>(mc: &'a Value, kinds: &[&str]) -> Option<&'a Value> {
    kinds.iter().find_map(|k| mc[*k].as_array().and_then(|a| a.first()))
}

/// "Title — Artist (track, key 123)"
fn describe(m: &Value) -> String {
    let who = [&m["grandparentTitle"], &m["parentTitle"]]
        .iter()
        .map(|v| text(v))
        .find(|s| !s.is_empty())
        .map(|s| format!(" — {}", s))
        .unwrap_or_default();
    let mut about = Vec::new();
    if !text(&m["type"]).is_empty() {
        about.push(text(&m["type"]));
    }
    if !text(&m["ratingKey"]).is_empty() {
        about.push(format!("key {}", text(&m["ratingKey"])));
    }
    let about = if about.is_empty() { String::new() } else { format!(" ({})", about.join(", ")) };
    format!("\"{}\"{}{}", text(&m["title"]), who, about)
}

/// One line per item, with the fields that say how it would be played.
fn item_line(o: &mut String, m: &Value) {
    let _ = write!(o, "      · {}", describe(m));
    for k in ["key", "playlistType", "radio", "guid", "librarySectionID"] {
        if !m[k].is_null() {
            let _ = write!(o, " {}={}", k, text(&m[k]));
        }
    }
    let _ = writeln!(o);
}

fn hubs(o: &mut String, v: &Value) {
    let hubs = list(&v["MediaContainer"], "Hub");
    let _ = writeln!(o, "  {} hubs", hubs.len());
    for h in hubs {
        let _ = write!(o, "  - \"{}\"", text(&h["title"]));
        for k in ["hubIdentifier", "context", "type", "style", "size", "more", "promoted", "key"] {
            if !h[k].is_null() {
                let _ = write!(o, " {}={}", k, text(&h[k]));
            }
        }
        let _ = writeln!(o);
        let mut entries = list(h, "Metadata");
        entries.extend(list(h, "Directory"));
        for m in entries.into_iter().take(SHOW) {
            item_line(o, m);
        }
    }
}

/// Fetches `path` and lists what it returns.
fn items_of(o: &mut String, server: &Server, path: &str) {
    match server.get_raw(path) {
        Ok(v) => {
            let mc = &v["MediaContainer"];
            let mut entries = list(mc, "Metadata");
            entries.extend(list(mc, "Directory"));
            let _ = writeln!(o, "  {} items", entries.len());
            for m in entries.into_iter().take(SHOW) {
                item_line(o, m);
            }
        }
        Err(e) => fail(o, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex::Plex;
    use serde_json::json;

    #[test]
    fn reports_what_the_server_returns_and_what_failed() {
        let track = |k: &str, t: &str, analysed: bool| {
            let mut m = json!({"ratingKey": k, "title": t, "type": "track", "grandparentTitle": "Cat Power", "grandparentRatingKey": "77"});
            if analysed {
                m["musicAnalysisVersion"] = json!("1");
            }
            m
        };
        let (base, log) = crate::test_server::start(vec![
            ("GET", "/library/sections/3/prefs", json!({"MediaContainer": {"Setting": [
                {"id": "enableSonicAnalysis", "value": true}, {"id": "hidden", "value": 0}]}})),
            ("GET", "/library/sections/3/all?type=10", json!({"MediaContainer": {"totalSize": 9000, "Metadata": [
                track("1", "Free", true), track("2", "Werewolf", false), track("3", "Fool", true)]}})),
            ("GET", "/library/sections/3/computePath", json!({"MediaContainer": {"Metadata": [track("1", "Free", true), track("3", "Fool", true)]}})),
            ("GET", "/library/sections/3", json!({"MediaContainer": {"Directory": [{"title": "Music", "agent": "tv.plex.agents.music"}]}})),
            ("GET", "/hubs/sections/3", json!({"MediaContainer": {"Hub": [
                {"title": "Mixes for You", "hubIdentifier": "music.mixes", "type": "playlist", "size": 1,
                 "Metadata": [{"title": "Cat Power Mix", "type": "playlist", "key": "/library/metadata/77/station/abc", "radio": 1}]},
                {"title": "Stations", "hubIdentifier": "music.stations", "Directory": [{"title": "Library Radio", "key": "/library/sections/3/stations/1"}]},
            ]}})),
            ("GET", "/library/metadata/1/nearest", json!({"MediaContainer": {"Metadata": [track("9", "Similar one", true)]}})),
            ("GET", "/", json!({"MediaContainer": {"friendlyName": "Home", "version": "1.41.0", "myPlexSubscription": true}})),
        ]);
        let server = Server::new(Plex::new("plext-test"), &base, "secret-token", "M");
        let r = report(&server, "3");
        for want in [
            "name: Home",
            "version: 1.41.0",
            "Plex Pass: true",
            "pref enableSonicAnalysis = true",
            "tracks in library: 9000",
            "with musicAnalysisVersion: 2 of 3",
            "\"Mixes for You\" hubIdentifier=music.mixes",
            "\"Cat Power Mix\" (playlist) key=",
            "key=/library/metadata/77/station/abc radio=1",
            "\"Library Radio\"",
            "\"Similar one\"",
            "Sonic adventure from \"Free\"",
            "computePath?startID=1&endID=3",
        ] {
            assert!(r.contains(want), "missing {:?} in:\n{}", want, r);
        }
        // An endpoint that isn't there is reported, not fatal.
        assert!(r.contains("Artists similar to Cat Power"), "{}", r);
        // Never prints the token.
        assert!(!r.contains("secret-token"));
        assert!(log.lock().unwrap().iter().all(|l| l.starts_with("GET")), "read-only");
    }
}
