//! A stand-in Plex server for tests: records each request ("GET /path?query")
//! and answers from a list of routes (the first whose method and path prefix
//! match), or with an empty 200.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub type Log = Arc<Mutex<Vec<String>>>;

/// Starts the server; returns its base URL and the request log.
pub fn start(routes: Vec<(&'static str, &'static str, Value)>) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
            seen.lock().unwrap().push(format!("{} {}", method, path));
            let body = routes
                .iter()
                .find(|(m, p, _)| *m == method && path.starts_with(p))
                .map(|(_, _, v)| v.to_string())
                .unwrap_or_default();
            let mut w = conn;
            let _ = write!(
                w,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    (base, log)
}

/// Waits (up to 5 s) until `pred` holds for the log.
pub fn wait_for(log: &Log, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let l = log.lock().unwrap().clone();
        if pred(&l) || std::time::Instant::now() > deadline {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
