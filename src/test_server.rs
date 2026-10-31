//! A stand-in Plex server for tests: records each request ("GET /path?query")
//! and answers from a list of routes (the first whose method and path prefix
//! match), or with an empty 200. A route's answer `{"__seq": [a, b, …]}`
//! gives a different answer each time.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub type Log = Arc<Mutex<Vec<String>>>;

/// Starts the server; returns its base URL and the request log.
pub fn start(routes: Vec<(&'static str, &'static str, Value)>) -> (String, Log) {
    start_slow(routes, Vec::new())
}

/// Like `start`, but requests whose path contains one of the `slow`
/// patterns wait that many milliseconds before the answer.
pub fn start_slow(routes: Vec<(&'static str, &'static str, Value)>, slow: Vec<(&'static str, u64)>) -> (String, Log) {
    let slow = Arc::new(slow);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let routes = Arc::new(routes);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (routes, seen, slow) = (routes.clone(), seen.clone(), slow.clone());
            std::thread::spawn(move || answer(conn, &routes, &seen, &slow));
        }
    });
    (base, log)
}

fn answer(conn: std::net::TcpStream, routes: &[(&'static str, &'static str, Value)], seen: &Log, slow: &[(&'static str, u64)]) {
    {
        {
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
            if let Some((_, ms)) = slow.iter().find(|(part, _)| path.contains(part)) {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
            }
            let route = routes.iter().find(|(m, p, _)| *m == method && path.starts_with(p));
            let body = match route {
                // {"__seq": [a, b, …]}: answer a the first time, b the next, and so
                // on, repeating the last.
                Some((m, p, v)) if v["__seq"].is_array() => {
                    let n = seen
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|l| l.strip_prefix(&format!("{} ", m)).map(|x| x.starts_with(p)).unwrap_or(false))
                        .count();
                    let seq = v["__seq"].as_array().unwrap();
                    seq.get(n.saturating_sub(1)).or(seq.last()).map(|x| x.to_string()).unwrap_or_default()
                }
                Some((_, _, v)) => v.to_string(),
                None => String::new(),
            };
            let mut w = conn;
            let _ = write!(
                w,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    }
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
