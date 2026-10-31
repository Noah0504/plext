//! What every `plext inspect …` command has in common.
//!
//! The inspect commands are for troubleshooting: they ask the server
//! something and print what came back, next to what Plext makes of it, so
//! anyone can tell whether a problem is their server's or Plext's, and
//! paste the output into an issue. Every one of them follows these rules,
//! and new ones should too:
//!
//! - It only reads. Nothing on the server or in the config changes.
//! - It starts with `header`, which says so and where to report problems.
//!   The wording is for any user, not for a particular conversation.
//! - It never prints the sign-in token (paths are shown without it).
//! - Progress ("Asking Edison…") goes to stderr; the report goes to stdout,
//!   so `plext inspect … > report.txt` holds just the report.
//! - Its output may change between versions: it isn't part of what 1.0
//!   promises to keep stable (the command names are).

/// The opening lines of an inspect report: "Plext 0.7.0 — <what>", that it
/// only reads, and where to report problems.
pub fn header(what: &str) -> String {
    format!(
        "Plext {} — {}\n\
         Read-only: this only asks your Plex server, and nothing is changed. It shows names from\n\
         your library, but never your sign-in token.\n\
         Reporting a problem? Include everything below in an issue: {}/issues\n\n",
        env!("CARGO_PKG_VERSION"),
        what,
        env!("CARGO_PKG_REPOSITORY"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_for_anyone() {
        let h = header("search check for \"free\"");
        assert!(h.starts_with(&format!("Plext {} — search check for \"free\"\n", env!("CARGO_PKG_VERSION"))));
        assert!(h.contains("Read-only"));
        assert!(h.contains("https://github.com/Noah0504/plext/issues"));
        assert!(!h.to_lowercase().contains("chat"));
        assert!(h.ends_with("\n\n"));
    }
}
