//! Streams a media file from Plex into a temporary file while it is being decoded.
//!
//! The decoder reads from a `StreamingFile`, which blocks until the bytes it asks
//! for have arrived. Keeping the data on disk rather than in memory keeps memory
//! use flat even for large 24/192 files.
//!
//! The files live in ~/.cache/plext/stream (private to the user), not /tmp:
//! on many systems, Debian 13 and the Raspberry Pi OS built on it among them,
//! /tmp is in memory, and a hi-res track plus the next one prefetched can fill
//! it on a small machine. Each file is deleted as soon as it's open, so it
//! takes no space once Plext is done with it, even after a crash.

use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Default)]
struct Progress {
    written: u64,
    done: bool,
    error: Option<String>,
}

struct Shared {
    progress: Mutex<Progress>,
    cond: Condvar,
    cancel: AtomicBool,
    total: AtomicU64, // 0 = unknown
    /// For reading, with positioned reads, so any number of readers can
    /// share it. Its name is already gone from the folder.
    file: File,
    /// The file's name, if deleting it straight away failed: deleted then
    /// when the last reader goes instead.
    leftover: Option<PathBuf>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(p) = &self.leftover {
            let _ = fs::remove_file(p);
        }
    }
}

/// ~/.cache/plext/stream, created private to the user; the system's temp
/// folder if there is no cache folder.
fn stream_dir() -> PathBuf {
    match dirs::cache_dir() {
        Some(c) => c.join("plext").join("stream"),
        None => std::env::temp_dir(),
    }
}

/// Handle to a download in progress. Dropping it cancels the download and,
/// once all readers are gone, removes the temporary file.
pub struct Download {
    shared: Arc<Shared>,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl Download {
    /// Starts copying `open()`'s reader into a temp file on a background thread.
    /// `open(offset)` must return a reader positioned at byte `offset` of the
    /// file, plus the file's total size if known. It is called again to resume
    /// if the connection drops.
    pub fn start<F>(ext: &str, open: F) -> Result<Download>
    where
        F: Fn(u64) -> Result<(Box<dyn Read + Send>, Option<u64>)> + Send + 'static,
    {
        Self::start_in(&stream_dir(), ext, open)
    }

    /// Like `start`, with the temporary file in `dir`.
    pub fn start_in<F>(dir: &Path, ext: &str, open: F) -> Result<Download>
    where
        F: Fn(u64) -> Result<(Box<dyn Read + Send>, Option<u64>)> + Send + 'static,
    {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ext: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let path = dir.join(format!(
            "plext-{}-{}.{}",
            std::process::id(),
            n,
            if ext.is_empty() { "bin" } else { &ext }
        ));
        // A new file only (never one that's already there), readable by the
        // user alone.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let reader = File::open(&path)?;
        // Both handles keep the data; the name can go now.
        let leftover = fs::remove_file(&path).err().map(|_| path);

        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress::default()),
            cond: Condvar::new(),
            cancel: AtomicBool::new(false),
            total: AtomicU64::new(0),
            file: reader,
            leftover,
        });

        let s = shared.clone();
        thread::Builder::new()
            .name("plext-download".into())
            .spawn(move || {
                let result = copy(&s, file, open);
                let mut p = s.progress.lock().unwrap();
                p.done = true;
                if let Err(e) = result {
                    p.error = Some(e.to_string());
                }
                s.cond.notify_all();
            })?;

        Ok(Download { shared })
    }

    /// A reader over the file that blocks until data is available.
    pub fn reader(&self) -> Result<StreamingFile> {
        Ok(StreamingFile {
            pos: 0,
            shared: self.shared.clone(),
        })
    }

    pub fn is_complete(&self) -> bool {
        let p = self.shared.progress.lock().unwrap();
        p.done && p.error.is_none()
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.cond.notify_all();
    }
}

/// Attempts in a row without receiving any data before giving up.
const MAX_ATTEMPTS: u32 = 6;

fn copy<F>(s: &Shared, mut file: File, open: F) -> Result<()>
where
    F: Fn(u64) -> Result<(Box<dyn Read + Send>, Option<u64>)>,
{
    let mut buf = vec![0u8; 256 * 1024];
    let mut written: u64 = 0;
    let mut failures: u32 = 0;
    loop {
        if s.cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        let before = written;
        let result: Result<()> = (|| {
            let (mut src, total) = open(written)?;
            if let Some(t) = total {
                s.total.store(t, Ordering::Relaxed);
            }
            loop {
                if s.cancel.load(Ordering::Relaxed) {
                    anyhow::bail!("cancelled");
                }
                let n = match src.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                };
                file.write_all(&buf[..n])?;
                written += n as u64;
                let mut p = s.progress.lock().unwrap();
                p.written = written;
                s.cond.notify_all();
            }
            // A connection that closes early looks like a normal end of data.
            let total = s.total.load(Ordering::Relaxed);
            if total > 0 && written < total {
                anyhow::bail!("connection closed after {} of {} bytes", written, total);
            }
            Ok(())
        })();
        match result {
            Ok(()) => break,
            Err(e) => {
                if s.cancel.load(Ordering::Relaxed) {
                    return Err(e);
                }
                // Any progress means the server is reachable: start counting afresh.
                failures = if written > before { 1 } else { failures + 1 };
                if failures >= MAX_ATTEMPTS {
                    return Err(e.context(format!("download failed after {} attempts", failures)));
                }
                crate::log::write(&format!(
                    "download interrupted at byte {} ({}); resuming",
                    written, e
                ));
                std::thread::sleep(Duration::from_millis(250 * (1 << failures.min(4))));
            }
        }
    }
    file.flush()?;
    Ok(())
}

/// Read + Seek view of a download in progress.
pub struct StreamingFile {
    pos: u64,
    shared: Arc<Shared>,
}

impl StreamingFile {
    pub fn len(&self) -> Option<u64> {
        match self.shared.total.load(Ordering::Relaxed) {
            0 => {
                let p = self.shared.progress.lock().unwrap();
                if p.done {
                    Some(p.written)
                } else {
                    None
                }
            }
            n => Some(n),
        }
    }

    /// Waits until at least `want` bytes are on disk (or the download ends).
    /// Returns how many bytes are available.
    fn wait_for(&self, want: u64) -> io::Result<u64> {
        let mut p = self.shared.progress.lock().unwrap();
        loop {
            if self.shared.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "download cancelled"));
            }
            if p.written >= want || p.done {
                if p.written < want {
                    if let Some(e) = &p.error {
                        return Err(io::Error::new(io::ErrorKind::Other, e.clone()));
                    }
                }
                return Ok(p.written);
            }
            p = self
                .shared
                .cond
                .wait_timeout(p, Duration::from_millis(200))
                .unwrap()
                .0;
        }
    }
}

impl Read for StreamingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let available = self.wait_for(self.pos + 1)?;
        if available <= self.pos {
            return Ok(0); // end of file
        }
        let n = ((available - self.pos) as usize).min(buf.len());
        let got = self.shared.file.read_at(&mut buf[..n], self.pos)?;
        self.pos += got as u64;
        Ok(got)
    }
}

impl Seek for StreamingFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => {
                let len = match self.len() {
                    Some(l) => l,
                    // Unknown length: wait for the whole file.
                    None => self.wait_for(u64::MAX)?,
                };
                len as i64 + d
            }
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for StreamingFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn streams_through_a_private_file_that_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("plext-stream-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let body = data.clone();
        let dl = Download::start_in(&dir, "flac", move |off| {
            let rest = body[off as usize..].to_vec();
            Ok((Box::new(io::Cursor::new(rest)) as Box<dyn Read + Send>, Some(body.len() as u64)))
        })
        .unwrap();
        // The folder is the user's alone, and the file's name is gone at once.
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        // Two readers at once (the decoder, and a seek elsewhere) each see it all.
        let (mut a, mut b) = (dl.reader().unwrap(), dl.reader().unwrap());
        b.seek(SeekFrom::Start(500_000)).unwrap();
        let mut tail = Vec::new();
        b.read_to_end(&mut tail).unwrap();
        let mut all = Vec::new();
        a.read_to_end(&mut all).unwrap();
        assert!(all == data, "read back {} of {} bytes", all.len(), data.len());
        assert!(tail[..] == data[500_000..]);
        assert!(dl.is_complete());
        drop((a, b, dl));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
