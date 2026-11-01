//! Persistent settings stored at ~/.config/plext/config.toml.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Random identifier for this install, sent to Plex as X-Plex-Client-Identifier.
    pub client_id: String,
    /// plex.tv account token obtained through the link-code sign-in.
    pub account_token: Option<String>,
    /// The chosen Plex Media Server.
    pub server: Option<ServerConfig>,
    /// The chosen music library (section key) on that server.
    pub library: Option<LibraryConfig>,
    /// How to play: bit-perfect through ALSA, or through the desktop's sound
    /// server. Unset means bit-perfect (what Plext did before this was asked).
    pub output: Option<OutputMode>,
    /// ALSA output device for bit-perfect mode, e.g. "hw:CARD=DAC,DEV=0" or "hw:1,0".
    pub device: Option<String>,
    /// PipeWire/PulseAudio output (sink name) for system mode. Unset follows
    /// the desktop's default output.
    pub sink: Option<String>,
    /// Report playback to Plex (now playing, play counts, webhooks). Default: on.
    pub report_playback: Option<bool>,
    /// Group an artist's releases under headings (Albums, EPs, Live…). Default: on.
    pub group_releases: Option<bool>,
    /// Show in the desktop's media controls and obey media keys (MPRIS). Default: on.
    pub mpris: Option<bool>,
    /// Show the album cover as pixel art beside what's playing. Default: on.
    pub album_art: Option<bool>,
    /// Color theme: a theme's name ("nord", "gruvbox-light"…), "terminal"
    /// for the terminal's own colors, or "auto" to follow the terminal's
    /// background. Default: Plext's own. `T` in Plext picks one.
    pub theme: Option<String>,
    /// Named themes paint their own background. Default: on.
    pub theme_background: Option<bool>,
    /// What `theme = "auto"` uses on dark and light terminals. Default:
    /// "plext" and "plext-light".
    pub theme_dark: Option<String>,
    pub theme_light: Option<String>,
    /// Single colors changed on top of the theme, e.g. `accent = "#e5a00d"`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub colors: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum OutputMode {
    /// Straight to the DAC through an ALSA `hw:` device, exclusively.
    Bitperfect,
    /// Through PipeWire or PulseAudio, alongside other apps.
    System,
}

impl OutputMode {
    pub fn name(self) -> &'static str {
        match self {
            OutputMode::Bitperfect => "bitperfect",
            OutputMode::System => "system",
        }
    }
}

impl Config {
    pub fn output_mode(&self) -> OutputMode {
        self.output.unwrap_or(OutputMode::Bitperfect)
    }

    /// The settings that decide the color theme.
    pub fn theme_settings(&self) -> crate::theme::Settings {
        crate::theme::Settings {
            theme: self.theme.clone(),
            background: self.theme_background,
            dark: self.theme_dark.clone(),
            light: self.theme_light.clone(),
            colors: self.colors.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub machine_id: String,
    /// Base URL of the connection that worked last, e.g. "http://192.168.1.10:32400".
    pub url: String,
    /// Access token for this server (may differ from the account token for shared servers).
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryConfig {
    pub key: String,
    pub title: String,
}

pub fn path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("could not find a config directory (is $HOME set?)")?
        .join("plext");
    Ok(dir.join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Config> {
        let p = path()?;
        let mut cfg: Config = if p.exists() {
            let text = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
        } else {
            Config::default()
        };
        if cfg.client_id.is_empty() {
            cfg.client_id = new_client_id();
            cfg.save()?;
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let p = path()?;
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        write_private(&p, text.as_bytes()).with_context(|| format!("writing {}", p.display()))
    }
}

/// Replaces `path` with `bytes`, readable by the user alone (the config
/// holds a Plex token) from the moment it exists. Written to a new file
/// that then takes the old one's place, so a crash part way leaves the old
/// config whole rather than half a file.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("toml.tmp");
    // A leftover from a crash could have looser permissions: start afresh.
    let _ = fs::remove_file(&tmp);
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn new_client_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut seed = nanos ^ ((std::process::id() as u128) << 64);
    // Mix with whatever randomness the kernel offers, if any.
    if let Ok(bytes) = fs::read("/proc/sys/kernel/random/uuid") {
        for b in bytes {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(b as u128);
        }
    }
    format!("plext-{:032x}", seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_private_from_the_start_and_replaced_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("plext-config-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        // An old config readable by anyone, and a loose leftover from a crash.
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(p.with_extension("toml.tmp"), "junk").unwrap();
        fs::set_permissions(p.with_extension("toml.tmp"), fs::Permissions::from_mode(0o666)).unwrap();
        write_private(&p, b"account_token = \"t\"\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "account_token = \"t\"\n");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!p.with_extension("toml.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_mode_is_written_in_lowercase_and_defaults_to_bitperfect() {
        let old: Config = toml::from_str("client_id = \"x\"\ndevice = \"hw:1,0\"\n").unwrap();
        assert_eq!(old.output_mode(), OutputMode::Bitperfect);
        let cfg = Config { output: Some(OutputMode::System), ..Config::default() };
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("output = \"system\""), "{}", text);
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.output_mode(), OutputMode::System);
    }

    #[test]
    fn theme_settings_read_and_write_with_a_colors_table() {
        let text = "client_id = \"x\"\ntheme = \"auto\"\ntheme_light = \"catppuccin-latte\"\ntheme_background = false\n\n[colors]\naccent = \"#ff8800\"\n";
        let cfg: Config = toml::from_str(text).unwrap();
        let s = cfg.theme_settings();
        assert!(s.is_auto());
        assert_eq!(s.light.as_deref(), Some("catppuccin-latte"));
        assert_eq!(s.background, Some(false));
        assert_eq!(s.colors.get("accent").map(String::as_str), Some("#ff8800"));
        // Written back (after the server table, which comes first), it reads the same.
        let mut cfg = cfg;
        cfg.server = Some(ServerConfig { name: "Edison".into(), machine_id: "m".into(), url: "http://x".into(), token: "t".into() });
        let out = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&out).unwrap();
        assert_eq!(back.colors, cfg.colors);
        assert_eq!(back.theme.as_deref(), Some("auto"));
        // Without colors, no empty table.
        let plain = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!plain.contains("[colors]"), "{}", plain);
    }
}
