//! Quitting cleanly when told to from outside: the terminal closing (an SSH
//! session dropping sends SIGHUP), `kill` (SIGTERM) or SIGINT. Without this
//! the process just dies, and the queue isn't saved.
//!
//! The handler only records which signal came. On SIGTERM or SIGINT the UI
//! loop notices within a moment and quits the normal way; when the terminal
//! goes away, `watch_hangup` takes over (see there for why).

use std::sync::atomic::{AtomicI32, Ordering};

static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    // Storing to an atomic is all a signal handler may safely do.
    RECEIVED.store(sig, Ordering::Relaxed);
}

/// Starts catching SIGHUP, SIGTERM and SIGINT.
pub fn install() {
    for sig in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
        // SAFETY: a zeroed sigaction is valid; the handler is async-signal-safe.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// The signal that asked Plext to quit, if one has.
pub fn received() -> Option<i32> {
    match RECEIVED.load(Ordering::Relaxed) {
        0 => None,
        s => Some(s),
    }
}

/// True when the terminal went away (so there's nowhere to draw or print).
pub fn hung_up() -> bool {
    received() == Some(libc::SIGHUP)
}

/// Whether the terminal on standard input has gone away (its window closed,
/// or the SSH connection dropped), signal or not. False when standard input
/// isn't a terminal.
fn terminal_gone() -> bool {
    // SAFETY: plain calls on fd 0 with one valid pollfd.
    unsafe {
        if libc::isatty(0) != 1 {
            // isatty fails on a hung-up terminal too; tell the two apart.
            return *libc::__errno_location() == libc::EIO;
        }
        let mut pfd = libc::pollfd { fd: 0, events: 0, revents: 0 };
        libc::poll(&mut pfd, 1, 0) > 0 && pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    }
}

/// Watches for the terminal going away and then runs `save` and ends the
/// process.
///
/// This can't be left to the UI loop: when the terminal goes, crossterm's
/// input reader spins forever on the closed terminal (it only stops reading
/// on "would block", and a hung-up terminal answers end-of-file or an error
/// instead), so the loop never gets to check. Nor can it wait for a SIGHUP:
/// one doesn't always come (it goes to the session leader, which may not
/// pass it on).
pub fn watch_hangup(save: impl Fn() + Send + 'static) {
    // SAFETY: isatty on fd 0.
    let watching_terminal = unsafe { libc::isatty(0) } == 1;
    let _ = std::thread::Builder::new().name("plext-hangup".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if hung_up() || (watching_terminal && terminal_gone()) {
            crate::log::write("the terminal went away: saving the queue and quitting");
            save();
            // Stream files are already deleted from the folder, and the
            // system closes the audio device: nothing is left behind.
            std::process::exit(0);
        }
    });
}
