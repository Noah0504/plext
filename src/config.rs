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
        fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
        // The file holds a Plex token, so keep it private to the user.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
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
    fn output_mode_is_written_in_lowercase_and_defaults_to_bitperfect() {
        let old: Config = toml::from_str("client_id = \"x\"\ndevice = \"hw:1,0\"\n").unwrap();
        assert_eq!(old.output_mode(), OutputMode::Bitperfect);
        let cfg = Config { output: Some(OutputMode::System), ..Config::default() };
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("output = \"system\""), "{}", text);
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.output_mode(), OutputMode::System);
    }
}
