//! Asks the terminal what color its background is, for `theme = "auto"`.
//!
//! Plext writes two questions to the terminal: "what's your background
//! color?" (OSC 11, which most terminals and tmux answer) and "what are
//! you?" (DA1, which every terminal answers). Answers come back in order,
//! so once the second arrives there's nothing more to wait for: a terminal
//! that ignores the first question costs one round trip, not a timeout.
//! The whole exchange is capped at `TIMEOUT` regardless.
//!
//! The terminal must be in raw mode (Plext's screen is), or the answers
//! would wait for Enter and echo on screen. If the terminal can't be asked,
//! `COLORFGBG` (set by some terminals, e.g. Konsole and rxvt) is the
//! fallback.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_millis(300);

/// Whether the terminal's background is light, if it can tell.
pub fn terminal_is_light() -> Option<bool> {
    match query_background() {
        Some((r, g, b)) => {
            let light = crate::theme::is_light(r, g, b);
            crate::log::write(&format!(
                "theme: terminal background #{:02x}{:02x}{:02x} ({})",
                r, g, b, if light { "light" } else { "dark" }
            ));
            Some(light)
        }
        None => {
            let from_env = std::env::var("COLORFGBG").ok().and_then(|v| colorfgbg_is_light(&v));
            crate::log::write(&format!(
                "theme: the terminal didn't say its background color; COLORFGBG says {}",
                match from_env {
                    Some(true) => "light",
                    Some(false) => "dark",
                    None => "nothing (using dark)",
                }
            ));
            from_env
        }
    }
}

/// The background color, by asking the terminal (OSC 11).
fn query_background() -> Option<(u8, u8, u8)> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    tty.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    tty.flush().ok()?;
    let fd = tty.as_raw_fd();
    let deadline = Instant::now() + TIMEOUT;
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while !answered(&got) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, for the duration of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as libc::c_int) };
        if ready <= 0 {
            break;
        }
        match tty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    parse_osc11(&got)
}

/// The DA1 answer ("ESC [ ? … c") has arrived, so the OSC 11 answer, if
/// any, came before it.
fn answered(got: &[u8]) -> bool {
    got.windows(3)
        .position(|w| w == b"\x1b[?")
        .map(|i| got[i + 3..].contains(&b'c'))
        .unwrap_or(false)
}

/// "ESC ] 11 ; rgb:RRRR/GGGG/BBBB" (each part 1 to 4 hex digits), ended by
/// BEL or ESC \.
fn parse_osc11(got: &[u8]) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(got);
    let start = text.find("]11;")? + 4;
    let rest = &text[start..];
    let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
    let spec = rest[..end].trim();
    let spec = spec.strip_prefix("rgb:").or_else(|| spec.strip_prefix("rgba:"))?;
    let mut parts = spec.split('/').map(|p| {
        let n = p.len();
        if n == 0 || n > 4 {
            return None;
        }
        let v = u32::from_str_radix(p, 16).ok()?;
        let max = (1u32 << (4 * n)) - 1;
        Some((v * 255 / max) as u8)
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// `COLORFGBG` is "fg;bg" (sometimes "fg;default;bg"), in terminal color
/// numbers: 7 and 9–15 are light backgrounds, 0–6 and 8 dark.
fn colorfgbg_is_light(v: &str) -> Option<bool> {
    let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
    Some(bg == 7 || bg >= 9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_terminals_answers() {
        // xterm, kitty, foot…: 16 bits a channel, ended by ESC \, then DA1.
        let a = b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\\x1b[?62;22c";
        assert!(answered(a));
        assert_eq!(parse_osc11(a), Some((0xfd, 0xf6, 0xe3)));
        // 8 bits a channel, ended by BEL.
        assert_eq!(parse_osc11(b"\x1b]11;rgb:28/28/28\x07"), Some((0x28, 0x28, 0x28)));
        // 4 bits a channel.
        assert_eq!(parse_osc11(b"\x1b]11;rgb:f/f/f\x07"), Some((255, 255, 255)));
        // Only DA1: the terminal ignored the color question.
        let b = b"\x1b[?1;2c";
        assert!(answered(b));
        assert_eq!(parse_osc11(b), None);
        assert!(!answered(b"\x1b]11;rgb:28/28/28\x07"));
    }

    #[test]
    fn colorfgbg_says_light_or_dark() {
        assert_eq!(colorfgbg_is_light("15;0"), Some(false));
        assert_eq!(colorfgbg_is_light("0;15"), Some(true));
        assert_eq!(colorfgbg_is_light("0;default;7"), Some(true));
        assert_eq!(colorfgbg_is_light("junk"), None);
    }
}
