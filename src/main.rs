mod app;
mod config;
mod decode;
mod download;
mod log;
mod output;
mod player;
mod plex;
mod report;

#[cfg(test)]
mod bitperfect_tests;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, LibraryConfig, ServerConfig};
use plex::{Plex, Server};
use std::io::{self, Write};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "plext", version, about = "A terminal music player for Plex with bit-perfect playback")]
struct Cli {
    /// ALSA device to play through for this session (e.g. hw:CARD=DAC,DEV=0)
    #[arg(short, long, global = true)]
    device: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to Plex and choose a server and music library
    Login,
    /// Forget the saved Plex sign-in
    Logout,
    /// List output devices, or set the default one: `plext device hw:CARD=DAC,DEV=0`
    Device { name: Option<String> },
    /// Show the current configuration
    Status,
    /// Show what the server reports about an artist's releases (for troubleshooting)
    Inspect { artist: String },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("plext: {:#}", e);
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load()?;

    match cli.command {
        Some(Command::Login) => {
            login(&mut cfg)?;
            println!("\nAll set. Run `plext` to start playing.");
            Ok(())
        }
        Some(Command::Logout) => {
            cfg.account_token = None;
            cfg.server = None;
            cfg.library = None;
            cfg.save()?;
            println!("Signed out.");
            Ok(())
        }
        Some(Command::Device { name }) => device_cmd(&mut cfg, name),
        Some(Command::Status) => {
            println!("Config file: {}", config::path()?.display());
            match &cfg.server {
                Some(s) => println!("Server:      {} ({})", s.name, s.url),
                None => println!("Server:      not set — run `plext login`"),
            }
            match &cfg.library {
                Some(l) => println!("Library:     {}", l.title),
                None => println!("Library:     not set"),
            }
            if let Some(p) = log::path() {
                println!("Log file:    {}", p.display());
            }
            match &cfg.device {
                Some(d) => println!("Device:      {}", d),
                None => println!(
                    "Device:      not set (auto: {})",
                    output::guess_device().unwrap_or_else(|| "default".into())
                ),
            }
            Ok(())
        }
        Some(Command::Inspect { artist }) => inspect(&mut cfg, &artist),
        None => run(&mut cfg, cli.device),
    }
}

fn run(cfg: &mut Config, device_override: Option<String>) -> Result<()> {
    if cfg.account_token.is_none() || cfg.server.is_none() || cfg.library.is_none() {
        println!("Welcome to Plext! Let's connect to your Plex server first.\n");
        login(cfg)?;
        println!();
    }
    let plex_client = Plex::new(&cfg.client_id);
    let server_cfg = connect(&plex_client, cfg)?;
    let library = cfg.library.clone().context("no library chosen; run `plext login`")?;

    let device = device_override
        .or_else(|| cfg.device.clone())
        .or_else(output::guess_device)
        .unwrap_or_else(|| "default".to_string());

    let server = Server::new(plex_client, &server_cfg.url, &server_cfg.token, &server_cfg.machine_id);
    let header = app::HeaderInfo {
        server_name: server_cfg.name.clone(),
        library_title: library.title.clone(),
    };

    let mut terminal = ratatui::init();
    let result = app::App::new(
        server,
        library.key,
        header,
        device,
        cfg.report_playback.unwrap_or(true),
        cfg.group_releases.unwrap_or(true),
    ).run(&mut terminal);
    ratatui::restore();
    result
}

/// Makes sure the saved server URL still works, rediscovering it if needed.
fn connect(plex_client: &Plex, cfg: &mut Config) -> Result<ServerConfig> {
    let server = cfg.server.clone().context("no server chosen; run `plext login`")?;
    if plex_client.ping(&server.url, &server.token).is_ok() {
        return Ok(server);
    }
    print!("Reconnecting to {}… ", server.name);
    io::stdout().flush().ok();
    let token = cfg.account_token.clone().context("not signed in; run `plext login`")?;
    let resources = plex_client.servers(&token)?;
    let res = resources
        .iter()
        .find(|r| r.machine_id == server.machine_id)
        .with_context(|| format!("server \"{}\" is no longer on your account", server.name))?;
    let url = plex_client
        .find_working_url(res)
        .with_context(|| format!("couldn't reach \"{}\" on any of its addresses", server.name))?;
    println!("ok");
    let updated = ServerConfig {
        url,
        token: res.token.clone(),
        ..server
    };
    cfg.server = Some(updated.clone());
    cfg.save()?;
    Ok(updated)
}

fn inspect(cfg: &mut Config, name: &str) -> Result<()> {
    let plex_client = Plex::new(&cfg.client_id);
    let server_cfg = connect(&plex_client, cfg)?;
    let library = cfg.library.clone().context("no library chosen; run `plext login`")?;
    let server = Server::new(plex_client, &server_cfg.url, &server_cfg.token, &server_cfg.machine_id);
    let artists = server.artists(&library.key)?;
    let wanted = name.to_lowercase();
    let artist = artists
        .iter()
        .find(|a| a.title.to_lowercase() == wanted)
        .or_else(|| artists.iter().find(|a| a.title.to_lowercase().contains(&wanted)))
        .with_context(|| format!("no artist matching \"{}\"", name))?;
    println!("{} (ratingKey {})\n", artist.title, artist.key);
    let albums = server.albums(&library.key, &artist.key)?;
    let raw = server.album_details_raw(&library.key, &artist.key)?;
    for al in &albums {
        println!("{} ({})  → shown as: {}", al.title, al.year.map(|y| y.to_string()).unwrap_or_default(),
            al.kind.label().unwrap_or("Album"));
        if let Some(m) = raw.iter().find(|m| m["ratingKey"].as_str() == Some(al.key.as_str())) {
            if let Some(obj) = m.as_object() {
                for (k, v) in obj {
                    let kl = k.to_lowercase();
                    if kl.contains("format") || kl.contains("type") || kl.contains("release") || kl == "subtype" {
                        println!("    {}: {}", k, v);
                    }
                }
            }
        }
    }
    Ok(())
}

fn login(cfg: &mut Config) -> Result<()> {
    let plex_client = Plex::new(&cfg.client_id);

    let (id, code) = plex_client.create_pin()?;
    println!("To sign in, open  https://plex.tv/link  and enter this code:\n");
    println!("        {}\n", code);
    print!("Waiting for you to approve");
    io::stdout().flush().ok();
    let started = Instant::now();
    let token = loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Some(t) = plex_client.check_pin(id)? {
            break t;
        }
        if started.elapsed() > Duration::from_secs(15 * 60) {
            bail!("the code expired; run `plext login` to try again");
        }
        print!(".");
        io::stdout().flush().ok();
    };
    println!(" signed in.\n");
    cfg.account_token = Some(token.clone());
    cfg.save()?;

    // Server
    let servers = plex_client.servers(&token)?;
    if servers.is_empty() {
        bail!("no Plex Media Servers are available on this account");
    }
    let idx = choose(
        "Which server?",
        &servers
            .iter()
            .map(|s| format!("{}{}", s.name, if s.owned { "" } else { " (shared)" }))
            .collect::<Vec<_>>(),
    )?;
    let res = &servers[idx];
    print!("Connecting to {}… ", res.name);
    io::stdout().flush().ok();
    let url = plex_client
        .find_working_url(res)
        .with_context(|| format!("couldn't reach \"{}\" on any of its addresses", res.name))?;
    println!("ok ({})", url);
    cfg.server = Some(ServerConfig {
        name: res.name.clone(),
        machine_id: res.machine_id.clone(),
        url: url.clone(),
        token: res.token.clone(),
    });

    // Library
    let server = Server::new(plex_client, &url, &res.token, &res.machine_id);
    let libs = server.music_libraries()?;
    if libs.is_empty() {
        bail!("\"{}\" has no music libraries", res.name);
    }
    let li = choose(
        "Which music library?",
        &libs.iter().map(|l| l.title.clone()).collect::<Vec<_>>(),
    )?;
    cfg.library = Some(LibraryConfig {
        key: libs[li].key.clone(),
        title: libs[li].title.clone(),
    });
    cfg.save()?;

    if cfg.device.is_none() {
        println!();
        device_cmd(cfg, None)?;
    }
    Ok(())
}

fn device_cmd(cfg: &mut Config, name: Option<String>) -> Result<()> {
    if let Some(n) = name {
        cfg.device = Some(n.clone());
        cfg.save()?;
        println!("Output device set to {}", n);
        return Ok(());
    }
    let devices = output::list_devices();
    if devices.is_empty() {
        println!("No ALSA hardware playback devices found.");
        return Ok(());
    }
    println!("Hardware output devices (bit-perfect capable):\n");
    for d in &devices {
        let mark = if cfg.device.as_deref() == Some(d.name.as_str()) { "*" } else { " " };
        println!(" {} {:<28} {}", mark, d.name, d.desc);
    }
    match &cfg.device {
        Some(d) => println!("\nCurrent: {}", d),
        None => println!(
            "\nNot set — using {} automatically.",
            output::guess_device().unwrap_or_else(|| "default (not bit-perfect)".into())
        ),
    }
    println!("Choose one with:  plext device <name>");
    Ok(())
}

fn choose(prompt: &str, options: &[String]) -> Result<usize> {
    if options.len() == 1 {
        println!("{} {}", prompt, options[0]);
        return Ok(0);
    }
    println!("{}", prompt);
    for (i, o) in options.iter().enumerate() {
        println!("  {}) {}", i + 1, o);
    }
    loop {
        print!("Enter 1-{}: ", options.len());
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            bail!("cancelled");
        }
        if let Ok(n) = line.trim().parse::<usize>() {
            if (1..=options.len()).contains(&n) {
                return Ok(n - 1);
            }
        }
    }
}
