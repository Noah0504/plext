//! Streams a media file from Plex into a temporary file while it is being decoded.
//!
//! The decoder reads from a `StreamingFile`, which blocks until the bytes it asks
//! for have arrived. Keeping the data on disk rather than in memory keeps memory
//! use flat even for large 24/192 files.

use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
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
    path: PathBuf,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
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
    pub fn start<F>(ext: &str, open: F) -> Result<Download>
    where
        F: FnOnce() -> Result<(Box<dyn Read + Send>, Option<u64>)> + Send + 'static,
    {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ext: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let path = std::env::temp_dir().join(format!(
            "plext-{}-{}.{}",
            std::process::id(),
            n,
            if ext.is_empty() { "bin" } else { &ext }
        ));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;

        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress::default()),
            cond: Condvar::new(),
            cancel: AtomicBool::new(false),
            total: AtomicU64::new(0),
            path,
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
            file: File::open(&self.shared.path)?,
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

fn copy<F>(s: &Shared, mut file: File, open: F) -> Result<()>
where
    F: FnOnce() -> Result<(Box<dyn Read + Send>, Option<u64>)>,
{
    let (mut src, len) = open()?;
    if let Some(len) = len {
        s.total.store(len, Ordering::Relaxed);
    }
    let mut buf = vec![0u8; 256 * 1024];
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
        let mut p = s.progress.lock().unwrap();
        p.written += n as u64;
        s.cond.notify_all();
    }
    file.flush()?;
    Ok(())
}

/// Read + Seek view of a download in progress.
pub struct StreamingFile {
    file: File,
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
        self.file.seek(SeekFrom::Start(self.pos))?;
        let got = self.file.read(&mut buf[..n])?;
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
