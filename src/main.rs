mod app;
mod config;
mod covers;
mod decode;
mod download;
mod inspect_mixes;
mod log;
mod mpris;
mod output;
mod pixelart;
mod player;
mod plex;
mod pulse;
mod queue;
mod report;
#[cfg(test)]
mod test_server;

#[cfg(test)]
mod bitperfect_tests;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, LibraryConfig, OutputMode, ServerConfig};
use output::Target;
use plex::{Plex, Server};
use std::io::{self, Write};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "plext", version, about = "A terminal music player for Plex with bit-perfect playback")]
struct Cli {
    /// Device to play through for this session: an ALSA device in bit-perfect
    /// mode (e.g. hw:CARD=DAC,DEV=0), a PipeWire/PulseAudio output in system mode
    #[arg(short, long, global = true)]
    device: Option<String>,

    /// How to play for this session: bitperfect or system
    #[arg(short, long, global = true, value_enum)]
    output: Option<OutputMode>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to Plex and choose a server and music library
    Login,
    /// Forget the saved Plex sign-in
    Logout,
    /// Choose how to play: `bitperfect` or `system` (PipeWire/PulseAudio); asks if not given
    Output {
        #[arg(value_enum)]
        mode: Option<OutputMode>,
    },
    /// List output devices, or set the default one: `plext device hw:CARD=DAC,DEV=0`
    Device { name: Option<String> },
    /// Show the current configuration
    Status,
    /// Show what the server reports about an artist's releases, or with
    /// `mixes`, what it offers for mixes and stations (for troubleshooting)
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
        Some(Command::Output { mode: None }) => setup_output(&mut cfg),
        Some(Command::Output { mode: Some(m) }) => {
            cfg.output = Some(m);
            cfg.save()?;
            println!("Output set to {}.", mode_label(m));
            println!("Now playing through {}. Change it with `plext device`.", target_for(&cfg, m, None).describe());
            Ok(())
        }
        Some(Command::Device { name }) => device_cmd(&mut cfg, cli.output, name),
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
            let mode = cli.output.unwrap_or(cfg.output_mode());
            match mode {
                OutputMode::Bitperfect => {
                    println!("Output:      bit-perfect (ALSA)");
                    match &cfg.device {
                        Some(d) => println!("Device:      {}", d),
                        None => println!(
                            "Device:      not set (auto: {})",
                            output::guess_device().unwrap_or_else(|| "default".into())
                        ),
                    }
                }
                OutputMode::System => {
                    let server = pulse::query();
                    match &server {
                        Ok(s) => println!("Output:      system sound ({})", s.kind),
                        Err(e) => println!("Output:      system sound — {}", e),
                    }
                    match (&cfg.sink, &server) {
                        (Some(sk), _) => println!("Device:      {}", sk),
                        (None, Ok(s)) => println!(
                            "Device:      desktop default (now: {})",
                            s.default_sink.as_deref().map(|d| sink_desc(s, d)).unwrap_or_else(|| "none".into())
                        ),
                        (None, Err(_)) => println!("Device:      desktop default"),
                    }
                }
            }
            Ok(())
        }
        Some(Command::Inspect { artist }) if artist.eq_ignore_ascii_case("mixes") => inspect_mixes(&mut cfg),
        // An artist actually called "Mixes": `plext inspect "artist:Mixes"`.
        Some(Command::Inspect { artist }) => inspect(&mut cfg, artist.strip_prefix("artist:").unwrap_or(&artist)),
        None => run(&mut cfg, cli.output, cli.device),
    }
}

/// Where to play in `mode`, honoring a one-off `--device`.
fn target_for(cfg: &Config, mode: OutputMode, device_override: Option<String>) -> Target {
    match mode {
        OutputMode::Bitperfect => Target::Alsa(
            device_override
                .or_else(|| cfg.device.clone())
                .or_else(output::guess_device)
                .unwrap_or_else(|| "default".to_string()),
        ),
        OutputMode::System => Target::SoundServer {
            sink: match device_override {
                Some(d) if d == "default" => None,
                Some(d) => Some(d),
                None => cfg.sink.clone(),
            },
        },
    }
}

fn mode_label(m: OutputMode) -> &'static str {
    match m {
        OutputMode::Bitperfect => "bit-perfect (straight to your DAC through ALSA)",
        OutputMode::System => "system sound (PipeWire/PulseAudio)",
    }
}

/// A sink's description, or its name when it has none.
fn sink_desc(s: &pulse::ServerSummary, name: &str) -> String {
    s.sinks
        .iter()
        .find(|k| k.name == name && !k.desc.is_empty())
        .map(|k| k.desc.clone())
        .unwrap_or_else(|| name.to_string())
}

fn run(cfg: &mut Config, mode_override: Option<OutputMode>, device_override: Option<String>) -> Result<()> {
    if cfg.account_token.is_none() || cfg.server.is_none() || cfg.library.is_none() {
        println!("Welcome to Plext! Let's connect to your Plex server first.\n");
        login(cfg)?;
        println!();
    }
    let plex_client = Plex::new(&cfg.client_id);
    let server_cfg = connect(&plex_client, cfg)?;
    let library = cfg.library.clone().context("no library chosen; run `plext login`")?;

    let mode = mode_override.unwrap_or(cfg.output_mode());
    let target = target_for(cfg, mode, device_override);
    log::write(&format!("output: {} → {}", mode.name(), target.describe()));

    let server = Server::new(plex_client, &server_cfg.url, &server_cfg.token, &server_cfg.machine_id);
    let header = app::HeaderInfo {
        server_name: server_cfg.name.clone(),
        library_title: library.title.clone(),
    };

    let mut terminal = ratatui::init();
    let mut app = app::App::new(
        server,
        library.key,
        header,
        target,
        cfg.report_playback.unwrap_or(true),
        cfg.group_releases.unwrap_or(true),
        cfg.mpris.unwrap_or(true),
        cfg.album_art.unwrap_or(true),
    );
    app.restore_queue();
    let result = app.run(&mut terminal);
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

fn inspect_mixes(cfg: &mut Config) -> Result<()> {
    let plex_client = Plex::new(&cfg.client_id);
    let server_cfg = connect(&plex_client, cfg)?;
    let library = cfg.library.clone().context("no library chosen; run `plext login`")?;
    let server = Server::new(plex_client, &server_cfg.url, &server_cfg.token, &server_cfg.machine_id);
    eprintln!("Asking {} about mixes and stations (this can take a few seconds)…\n", server_cfg.name);
    print!("{}", inspect_mixes::report(&server, &library.key));
    Ok(())
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

    if cfg.output.is_none() {
        println!();
        setup_output(cfg)?;
    }
    Ok(())
}

/// Asks how to play, then which device to play through, and saves both.
fn setup_output(cfg: &mut Config) -> Result<()> {
    let server = pulse::query().ok();
    let usb = output::guess_device();
    let kind = server.as_ref().map(|s| s.kind).unwrap_or("PipeWire/PulseAudio");
    let options = vec![
        "Bit-perfect: straight to your DAC, untouched. Exclusive while playing; volume on the DAC."
            .to_string(),
        format!(
            "Desktop sound: through {}, to any speakers or headphones, alongside other apps.{}",
            kind,
            if server.is_none() { " (Not running right now.)" } else { "" }
        ),
    ];
    // A USB DAC is the reason to want bit-perfect; without one, desktop sound
    // is what most people expect.
    let recommended = if usb.is_none() && server.is_some() { 1 } else { 0 };
    let current = cfg.output.map(|m| if m == OutputMode::Bitperfect { 0 } else { 1 });
    let pick = choose_default("How should Plext play your music?", &options, current.unwrap_or(recommended))?;
    let mode = if pick == 0 { OutputMode::Bitperfect } else { OutputMode::System };
    cfg.output = Some(mode);

    match mode {
        OutputMode::Bitperfect => {
            let devices = output::list_devices();
            if devices.is_empty() {
                println!(
                    "\nNo ALSA hardware playback devices found, so Plext will use ALSA's default device \
                     (not bit-perfect). Connect your DAC and run `plext device` to choose it."
                );
            } else {
                let names: Vec<String> = devices.iter().map(|d| format!("{:<26} {}", d.name, d.desc)).collect();
                let default = cfg
                    .device
                    .as_deref()
                    .or(usb.as_deref())
                    .and_then(|want| devices.iter().position(|d| d.name == want))
                    .unwrap_or(0);
                println!();
                let i = choose_default("Which device?", &names, default)?;
                cfg.device = Some(devices[i].name.clone());
            }
        }
        OutputMode::System => {
            if let Some(s) = server.as_ref().filter(|s| !s.sinks.is_empty()) {
                let now = s.default_sink.as_deref().map(|d| sink_desc(s, d));
                let mut names = vec![match now {
                    Some(n) => format!("Follow the desktop's default output (now: {})", n),
                    None => "Follow the desktop's default output".to_string(),
                }];
                names.extend(s.sinks.iter().map(|k| if k.desc.is_empty() { k.name.clone() } else { k.desc.clone() }));
                let default = cfg
                    .sink
                    .as_deref()
                    .and_then(|want| s.sinks.iter().position(|k| k.name == want).map(|i| i + 1))
                    .unwrap_or(0);
                println!();
                let i = choose_default("Which output?", &names, default)?;
                cfg.sink = if i == 0 { None } else { Some(s.sinks[i - 1].name.clone()) };
            } else {
                cfg.sink = None;
                println!("\nPlext will play through the desktop's default output once a sound server is running.");
            }
        }
    }
    cfg.save()?;
    println!(
        "\nOutput: {} → {}. Change it any time with `plext output`.",
        mode_label(mode),
        target_for(cfg, mode, None).describe()
    );
    Ok(())
}

fn device_cmd(cfg: &mut Config, mode_override: Option<OutputMode>, name: Option<String>) -> Result<()> {
    if mode_override.unwrap_or(cfg.output_mode()) == OutputMode::System {
        return sink_cmd(cfg, name);
    }
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
    println!("To play through your desktop's sound instead:  plext output system");
    Ok(())
}

/// `plext device` in system mode: the sound server's outputs.
fn sink_cmd(cfg: &mut Config, name: Option<String>) -> Result<()> {
    let server = pulse::query();
    if let Some(n) = name {
        if n == "default" {
            cfg.sink = None;
            cfg.save()?;
            println!("Playing through the desktop's default output.");
            return Ok(());
        }
        if let Ok(s) = &server {
            if !s.sinks.iter().any(|k| k.name == n) {
                bail!("{} has no output named \"{}\"; run `plext device` to list them", s.kind, n);
            }
        }
        cfg.sink = Some(n.clone());
        cfg.save()?;
        println!("Output device set to {}", n);
        return Ok(());
    }
    let s = server?;
    println!("{} outputs:\n", s.kind);
    for k in &s.sinks {
        let mark = if cfg.sink.as_deref() == Some(k.name.as_str()) { "*" } else { " " };
        let default = if s.default_sink.as_deref() == Some(k.name.as_str()) { "  (desktop default)" } else { "" };
        println!(" {} {}{}\n     {}", mark, k.desc, default, k.name);
    }
    match &cfg.sink {
        Some(k) => println!("\nCurrent: {}", k),
        None => println!("\nCurrent: the desktop's default output"),
    }
    println!("Choose one with:  plext device <name>   (or `plext device default` to follow the desktop)");
    println!("For bit-perfect playback straight to a DAC:  plext output bitperfect");
    Ok(())
}

fn choose(prompt: &str, options: &[String]) -> Result<usize> {
    choose_inner(prompt, options, None)
}

/// Like `choose`, but pressing Enter picks `default` (marked with *).
fn choose_default(prompt: &str, options: &[String], default: usize) -> Result<usize> {
    choose_inner(prompt, options, Some(default.min(options.len().saturating_sub(1))))
}

fn choose_inner(prompt: &str, options: &[String], default: Option<usize>) -> Result<usize> {
    if options.len() == 1 {
        println!("{} {}", prompt, options[0]);
        return Ok(0);
    }
    println!("{}", prompt);
    for (i, o) in options.iter().enumerate() {
        let mark = if default == Some(i) { "*" } else { " " };
        println!(" {}{}) {}", mark, i + 1, o);
    }
    loop {
        match default {
            Some(d) => print!("Enter 1-{} [{}]: ", options.len(), d + 1),
            None => print!("Enter 1-{}: ", options.len()),
        }
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            bail!("cancelled");
        }
        if let (Some(d), "") = (default, line.trim()) {
            return Ok(d);
        }
        if let Ok(n) = line.trim().parse::<usize>() {
            if (1..=options.len()).contains(&n) {
                return Ok(n - 1);
            }
        }
    }
}
