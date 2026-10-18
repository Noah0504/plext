//! A small on-disk cache of album covers, in ~/.cache/plext/covers.
//!
//! Covers are handed to other programs (the desktop's media controls) as
//! `file://` paths, so the Plex token never leaves Plext. They are fetched in
//! the background and written atomically, so a reader never sees half a file.

use crate::plex::Server;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

/// Covers are scaled down by the server to at most this many pixels a side.
const SIZE: u32 = 512;
/// Keep at most this many covers; the least recently written go first.
const MAX_FILES: usize = 400;

#[derive(Clone)]
pub struct Covers {
    server: Server,
    dir: PathBuf,
    in_flight: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Covers {
    /// Opens the cache, creating its folder. `None` when there is nowhere to put it.
    pub fn open(server: Server) -> Option<Covers> {
        let dir = dirs::cache_dir()?.join("plext").join("covers");
        fs::create_dir_all(&dir).ok()?;
        prune(&dir);
        Some(Covers {
            server,
            dir,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Where the cover for `thumb` lives (or will live) on disk.
    pub fn path_for(&self, thumb: &str) -> PathBuf {
        let key = fnv1a(&[self.server.machine_id.as_bytes(), b"\0", thumb.as_bytes()]);
        self.dir.join(format!("{:016x}.jpg", key))
    }

    /// The cover's path if it has already been downloaded.
    pub fn cached(&self, thumb: &str) -> Option<PathBuf> {
        let p = self.path_for(thumb);
        p.is_file().then_some(p)
    }

    /// Starts downloading the cover in the background, unless it's already
    /// cached or on its way. Failures are logged and simply leave no cover.
    pub fn request(&self, thumb: &str) {
        let path = self.path_for(thumb);
        if path.is_file() || !self.in_flight.lock().unwrap().insert(path.clone()) {
            return;
        }
        let claimed = path.clone();
        let (server, thumb, in_flight) = (self.server.clone(), thumb.to_string(), self.in_flight.clone());
        let spawned = thread::Builder::new().name("plext-cover".into()).spawn(move || {
            match server.fetch_image(&thumb, SIZE) {
                Ok(bytes) => {
                    let part = path.with_extension("part");
                    let saved = fs::write(&part, &bytes).and_then(|_| fs::rename(&part, &path));
                    if let Err(e) = saved {
                        let _ = fs::remove_file(&part);
                        crate::log::write(&format!("cover: couldn't save {}: {}", path.display(), e));
                    }
                }
                Err(e) => crate::log::write(&format!("cover: couldn't fetch {}: {}", thumb, e)),
            }
            in_flight.lock().unwrap().remove(&path);
        });
        if spawned.is_err() {
            self.in_flight.lock().unwrap().remove(&claimed);
        }
    }
}

/// FNV-1a, 64-bit: a stable hash, so file names stay the same across builds.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.iter() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Deletes the oldest covers once there are more than `MAX_FILES`, and any
/// leftover partial downloads.
fn prune(dir: &PathBuf) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        match p.extension().and_then(|x| x.to_str()) {
            Some("part") => {
                let _ = fs::remove_file(&p);
            }
            Some("jpg") => {
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    files.push((t, p));
                }
            }
            _ => {}
        }
    }
    if files.len() > MAX_FILES {
        files.sort();
        let extra = files.len() - MAX_FILES;
        for (_, p) in files.into_iter().take(extra) {
            let _ = fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_stable() {
        // Reference value for FNV-1a 64 of "a".
        assert_eq!(fnv1a(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(&[b"a", b"b"]), fnv1a(&[b"ab"]));
    }
}
