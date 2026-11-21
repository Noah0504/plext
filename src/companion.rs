//! Opt-in LAN-only Plex Companion receiver. Discovery uses GDM.
//! Network work stays off the UI/audio threads.
//! Protocol: https://github.com/plexinc/plex-media-player/wiki/Remote-control-API

use crate::player::{Cmd, PlayState, QueueHandle, Status};
use crate::plex::{Server, VERSION};
use crate::queue::Repeat;
use crate::volume::Mixer;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, StatusCode};
use url::Url;

const CAPABILITIES: &str = "timeline,playback,playqueues";
const TTL: Duration = Duration::from_secs(90);
const MAX_CONTROLLERS: usize = 64;
const GDM_GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 0, 0, 250), 32413);

pub struct Settings {
    pub port: u16,
    pub name: String,
    pub address: Ipv4Addr,
    pub client_id: String,
}

impl Settings {
    pub fn new(port: u16, name: String, address: Ipv4Addr, client_id: String) -> Result<Self> {
        if port == 0 {
            bail!("--companion-port must be between 1 and 65535");
        }
        if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            bail!("--companion-name must be 1–128 bytes without control characters");
        }
        if address.is_unspecified() || address.is_multicast() || address.is_broadcast() {
            bail!("--companion-address must be this machine's reachable IPv4 address");
        }
        Ok(Self {
            port,
            name,
            address,
            client_id,
        })
    }
    pub fn uri(&self) -> String {
        format!("http://{}:{}", self.address, self.port)
    }
}

/// Select an interface by routing a UDP socket, without sending packets.
pub fn local_address(server: &str) -> Result<Ipv4Addr> {
    let url = Url::parse(server)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let routed = url
        .host_str()
        .zip(url.port_or_known_default())
        .is_some_and(|(h, p)| socket.connect((h, p)).is_ok());
    if !routed {
        socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80))?;
    }
    match socket.local_addr()?.ip() {
        IpAddr::V4(ip) => Ok(ip),
        _ => bail!("could not choose an IPv4 address; supply --companion-address"),
    }
}

type Dispatch = Arc<dyn Fn(Cmd) -> Result<()> + Send + Sync>;
type Params = HashMap<String, String>;

struct Controller {
    command_id: Option<u64>,
    attempted_id: Option<u64>,
    failed: bool,
    seen: Instant,
    callback: Option<String>,
    last_body: String,
    last_push: Instant,
    last_event: String,
}

struct State {
    settings: Settings,
    server: Server,
    status: Arc<Mutex<Status>>,
    queue: QueueHandle,
    mixer: Option<Mixer>,
    dispatch: Dispatch,
    controllers: Mutex<HashMap<String, Controller>>,
    /// Serialize mutations and duplicate checks across HTTP workers.
    commands: Mutex<()>,
    stop: Arc<AtomicBool>,
}

pub struct Companion {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl Companion {
    pub fn spawn(
        settings: Settings,
        server: Server,
        status: Arc<Mutex<Status>>,
        queue: QueueHandle,
        mixer: Option<Mixer>,
        dispatch: impl Fn(Cmd) -> Result<()> + Send + Sync + 'static,
    ) -> Result<Self> {
        // Discovery is the only way we advertise this receiver. Surface a
        // socket conflict at startup instead of silently running undiscoverable.
        let discovery =
            gdm_socket(settings.address, 32412).context("starting LAN discovery on UDP 32412")?;
        let http = Arc::new(
            tiny_http::Server::http((Ipv4Addr::UNSPECIFIED, settings.port))
                .map_err(|e| anyhow::anyhow!("binding Companion port {}: {}", settings.port, e))?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State {
            settings,
            server,
            status,
            queue,
            mixer,
            dispatch: Arc::new(dispatch),
            controllers: Mutex::new(HashMap::new()),
            commands: Mutex::new(()),
            stop: stop.clone(),
        });
        let mut receiver = Self {
            stop,
            handles: Vec::new(),
        };
        for i in 0..4 {
            let (http, state) = (http.clone(), state.clone());
            receiver.handles.push(
                thread::Builder::new()
                    .name(format!("plext-companion-{}", i))
                    .spawn(move || {
                        while !state.stop.load(Ordering::Relaxed) {
                            match http.recv_timeout(Duration::from_millis(100)) {
                                Ok(Some(request)) => state.handle(request),
                                Ok(None) => {}
                                Err(_) => break,
                            }
                        }
                    })?,
            );
        }
        let s = state.clone();
        receiver.handles.push(
            thread::Builder::new()
                .name("plext-companion-push".into())
                .spawn(move || s.push_timelines())?,
        );
        receiver.handles.push(
            thread::Builder::new()
                .name("plext-companion-gdm".into())
                .spawn(move || gdm(&state, discovery, GDM_GROUP))?,
        );
        Ok(receiver)
    }
}

impl Drop for Companion {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let until = Instant::now() + Duration::from_millis(500);
        while self.handles.iter().any(|h| !h.is_finished()) && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        for h in self.handles.drain(..) {
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl State {
    fn handle(&self, request: Request) {
        let (code, body) = match self.route(&request) {
            Ok(body) => (200, body),
            Err((code, message)) => (
                code,
                format!("<Response code=\"{}\" status=\"{}\"/>", code, xml(&message)),
            ),
        };
        let response = Response::from_string(body)
            .with_status_code(StatusCode(code))
            .with_header(header("Content-Type", "application/xml; charset=utf-8"))
            .with_header(header("X-Plex-Client-Identifier", &self.settings.client_id))
            .with_header(header(
                "Access-Control-Expose-Headers",
                "X-Plex-Client-Identifier",
            ))
            .with_header(header("Cache-Control", "no-store"));
        let _ = request.respond(response);
    }

    fn route(&self, request: &Request) -> std::result::Result<String, (u16, String)> {
        let fail = |code, msg: &str| Err((code, msg.to_owned()));
        let peer = request
            .remote_addr()
            .ok_or_else(|| (403, "missing peer address".into()))?;
        if !local_peer(peer.ip()) {
            return fail(403, "Companion is available on the local network only");
        }
        if request.url().len() > 16_384 {
            return fail(414, "request URL too long");
        }
        if request.method() != &Method::Get {
            return fail(405, "use GET");
        }
        // Native Plexamp doesn't need browser CORS.
        if request.headers().iter().any(|h| h.field.equiv("Origin")) {
            return fail(403, "browser requests are not supported");
        }
        let url = Url::parse(&format!("http://receiver{}", request.url()))
            .map_err(|_| (400, "invalid request URL".into()))?;
        if url.host_str() != Some("receiver") {
            return fail(400, "invalid request target");
        }
        let p: Params = url.query_pairs().into_owned().collect();
        let value = |name: &'static str| {
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv(name))
                .map(|h| h.value.as_str().to_owned())
                .or_else(|| p.get(name).cloned())
        };
        if value("X-Plex-Target-Client-Identifier").is_some_and(|id| id != self.settings.client_id)
        {
            return fail(404, "unknown player");
        }
        if matches!(url.path(), "/resources" | "/" | "/identity") {
            return Ok(self.resources());
        }
        let controller = value("X-Plex-Client-Identifier")
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or_else(|| (400, "X-Plex-Client-Identifier is required".into()))?;
        let command_id = optional_number(&p, "commandID").map_err(bad_request)?;
        if p.get("type").is_some_and(|t| t != "music") {
            return fail(400, "only music is supported");
        }
        match url.path() {
            "/player/timeline/poll" => {
                self.touch(&controller).map_err(bad_request)?;
                // A poll's ID isn't a completed playback command.
                if p.get("wait").is_some_and(|s| s == "1") {
                    let started = Instant::now();
                    let event = self.event_signature();
                    while started.elapsed() < Duration::from_secs(1)
                        && !self.stop.load(Ordering::Relaxed)
                    {
                        thread::sleep(Duration::from_millis(50));
                        if self.event_signature() != event {
                            break;
                        }
                    }
                }
                Ok(self.timeline(self.last_command(&controller).unwrap_or(0), false))
            }
            "/player/timeline/subscribe" => {
                let protocol = p.get("protocol").map(String::as_str).unwrap_or("http");
                if !matches!(protocol, "http" | "https") {
                    return fail(400, "invalid callback protocol");
                }
                let port = required_number(&p, "port").map_err(bad_request)?;
                if port == 0 || port > 65535 {
                    return fail(400, "invalid callback port");
                }
                // Always call back the peer, never a query-supplied host.
                let callback = format!(
                    "{}://{}/:/timeline",
                    protocol,
                    SocketAddr::new(peer.ip(), port as u16)
                );
                let _serial = self.commands.lock().unwrap();
                self.touch(&controller).map_err(bad_request)?;
                let mut controllers = self.controllers.lock().unwrap();
                let c = controllers.get_mut(&controller).unwrap();
                c.command_id = command_id;
                c.attempted_id = command_id;
                c.failed = false;
                c.callback = Some(callback);
                c.last_body.clear();
                c.last_event.clear();
                c.last_push = Instant::now() - Duration::from_secs(1);
                Ok(String::new())
            }
            "/player/timeline/unsubscribe" => {
                self.controllers.lock().unwrap().remove(&controller);
                Ok(String::new())
            }
            path if path.starts_with("/player/playback/") => {
                let _serial = self.commands.lock().unwrap();
                self.touch(&controller).map_err(bad_request)?;
                if let Some(id) = command_id {
                    let clients = self.controllers.lock().unwrap();
                    let c = &clients[&controller];
                    if c.attempted_id.is_some_and(|last| id <= last) {
                        return if c.attempted_id == Some(id) && c.failed {
                            fail(400, "the previous attempt failed; read timeline before sending a new command")
                        } else {
                            Ok(String::new())
                        };
                    }
                }
                let result = self.playback(path, &p);
                // A timed-out dispatch may already have taken effect. Remember
                // failed attempts too, so a retransmission never double-skips.
                if let Some(id) = command_id {
                    if let Some(c) = self.controllers.lock().unwrap().get_mut(&controller) {
                        c.attempted_id = Some(id);
                        c.failed = result.is_err();
                        if result.is_ok() {
                            c.command_id = Some(id);
                        }
                    }
                }
                result.map_err(|_| {
                    crate::log::write("companion: playback request failed (invalid parameters, unavailable queue, or player busy)");
                    (400, "cannot apply request; check the server, music queue and parameters".into())
                })?;
                Ok(String::new())
            }
            _ => fail(404, "unsupported Companion endpoint"),
        }
    }

    fn touch(&self, id: &str) -> Result<()> {
        let mut controllers = self.controllers.lock().unwrap();
        controllers.retain(|_, c| c.seen.elapsed() < TTL);
        if !controllers.contains_key(id) && controllers.len() >= MAX_CONTROLLERS {
            bail!("too many controllers");
        }
        let c = controllers
            .entry(id.to_owned())
            .or_insert_with(|| Controller {
                command_id: None,
                attempted_id: None,
                failed: false,
                seen: Instant::now(),
                callback: None,
                last_body: String::new(),
                last_push: Instant::now(),
                last_event: String::new(),
            });
        c.seen = Instant::now();
        Ok(())
    }
    fn last_command(&self, id: &str) -> Option<u64> {
        self.controllers
            .lock()
            .unwrap()
            .get(id)
            .and_then(|c| c.command_id)
    }

    fn playback(&self, path: &str, p: &Params) -> Result<()> {
        let send = |cmd| (self.dispatch)(cmd);
        match path {
            "/player/playback/play" => send(Cmd::Resume),
            "/player/playback/pause" => send(Cmd::Pause),
            "/player/playback/stop" => send(Cmd::Stop),
            "/player/playback/skipNext" => send(Cmd::Next),
            "/player/playback/skipPrevious" => send(Cmd::Prev),
            "/player/playback/stepBack" => send(Cmd::SeekBy(-15.0)),
            "/player/playback/stepForward" => send(Cmd::SeekBy(30.0)),
            "/player/playback/seekTo" => {
                send(Cmd::SeekTo(required_number(p, "offset")? as f64 / 1000.0))
            }
            "/player/playback/skipTo" => {
                let key = media_id(p.get("key").context("missing key")?)?;
                let wanted = optional_number(p, "playQueueItemID")?;
                let view = self.queue.view();
                let e = view
                    .items
                    .iter()
                    .find(|e| {
                        e.track.key == key && wanted.is_none_or(|i| e.play_queue_item_id == Some(i))
                    })
                    .context("track is not in the queue")?;
                send(Cmd::Jump(e.id))
            }
            "/player/playback/setParameters" => {
                // Validate all fields before applying any changes.
                let volume = optional_number(p, "volume")?;
                if volume.is_some_and(|v| v > 100) {
                    bail!("volume must be 0–100");
                }
                if volume.is_some() && self.mixer.is_none() {
                    bail!("volume is fixed in bit-perfect mode");
                }
                let shuffle = optional_number(p, "shuffle")?;
                if shuffle.is_some_and(|v| v > 1) {
                    bail!("shuffle must be 0 or 1");
                }
                let repeat = optional_number(p, "repeat")?;
                if repeat.is_some_and(|v| v > 2) {
                    bail!("repeat must be 0, 1 or 2");
                }
                if let Some(shuffle) = shuffle {
                    let remote = self.status.lock().unwrap().remote.clone();
                    if let Some(r) = remote {
                        send(Cmd::RefreshRemote(
                            self.server.shuffle_remote_queue(r.info.id, shuffle == 1)?,
                        ))?;
                    } else {
                        send(Cmd::SetShuffle(shuffle == 1))?;
                    }
                }
                if let Some(repeat) = repeat {
                    send(Cmd::SetRepeat(match repeat {
                        1 => Repeat::One,
                        2 => Repeat::All,
                        _ => Repeat::Off,
                    }))?;
                }
                if let Some(v) = volume {
                    self.mixer.as_ref().unwrap().set(v as u32);
                }
                Ok(())
            }
            "/player/playback/refreshPlayQueue" => {
                let id = required_number(p, "playQueueID")?;
                if self
                    .status
                    .lock()
                    .unwrap()
                    .remote
                    .as_ref()
                    .map(|r| r.info.id)
                    != Some(id)
                {
                    bail!("not the active queue");
                }
                send(Cmd::RefreshRemote(self.server.remote_queue(id)?))
            }
            "/player/playback/playMedia" => self.play_media(p),
            _ => bail!("unsupported playback operation"),
        }
    }

    fn play_media(&self, p: &Params) -> Result<()> {
        if p.get("machineIdentifier")
            .is_some_and(|id| id != &self.server.machine_id)
        {
            bail!("use the configured server");
        }
        let offset = optional_number(p, "offset")?.unwrap_or(0) as f64 / 1000.0;
        let key = p.get("key").map(|k| media_id(k)).transpose()?;
        if let Some(container) = p.get("containerKey") {
            let container = container.split('?').next().unwrap_or(container);
            if let Some(id) = container.strip_prefix("/playQueues/") {
                let mut pq = self.server.remote_queue(digits(id)?.parse()?)?;
                if pq.tracks.is_empty() {
                    bail!("empty cast queue");
                }
                if let Some(item) = optional_number(p, "playQueueItemID")? {
                    pq.selected = pq
                        .item_ids
                        .iter()
                        .position(|i| *i == item)
                        .context("item not in queue")?;
                } else if let Some(key) = &key {
                    if pq.tracks[pq.selected].key != *key {
                        pq.selected = pq
                            .tracks
                            .iter()
                            .position(|t| t.key == *key)
                            .context("track not in queue")?;
                    }
                }
                return (self.dispatch)(Cmd::PlayRemote { queue: pq, offset });
            }
            if let Some(album) = container
                .strip_prefix("/library/metadata/")
                .and_then(|s| s.strip_suffix("/children"))
            {
                let tracks = self.server.tracks(digits(album)?)?;
                let index = key
                    .as_ref()
                    .and_then(|k| tracks.iter().position(|t| t.key == *k))
                    .unwrap_or(0);
                if tracks.is_empty() {
                    bail!("empty album");
                }
                return (self.dispatch)(Cmd::PlayAt {
                    queue: tracks,
                    index,
                    offset,
                });
            }
            bail!("unsupported container; use a server play queue");
        }
        let track = self
            .server
            .track(key.as_deref().context("missing media key")?)?;
        (self.dispatch)(Cmd::PlayAt {
            queue: vec![track],
            index: 0,
            offset,
        })
    }

    fn resources(&self) -> String {
        format!("<MediaContainer size=\"1\"><Player title=\"{}\" name=\"{}\" machineIdentifier=\"{}\" clientIdentifier=\"{}\" product=\"Plext\" version=\"{}\" platform=\"Linux\" deviceClass=\"stb\" protocol=\"plex\" protocolVersion=\"1\" protocolCapabilities=\"{}\" provides=\"player\"/></MediaContainer>",
            xml(&self.settings.name), xml(&self.settings.name), xml(&self.settings.client_id), xml(&self.settings.client_id), VERSION, CAPABILITIES)
    }

    fn timeline(&self, command_id: u64, disconnected: bool) -> String {
        let st = self.status.lock().unwrap().clone();
        let state = match st.state {
            PlayState::Stopped => "stopped",
            PlayState::Loading => "buffering",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
        };
        let mut controllable =
            "playPause,stop,shuffle,repeat,seekTo,skipPrevious,skipNext,stepBack,stepForward"
                .to_owned();
        if self.mixer.is_some() {
            controllable.push_str(",volume");
        }
        let mut attrs = format!("type=\"music\" state=\"{}\" controllable=\"{}\" time=\"{}\" shuffle=\"{}\" repeat=\"{}\"",
            state, controllable, milliseconds(st.pos), u8::from(st.shuffle), match st.repeat { Repeat::Off => 0, Repeat::One => 1, Repeat::All => 2 });
        if let Some(dur) = st.dur {
            attrs.push_str(&format!(" duration=\"{}\"", milliseconds(dur)));
        }
        if let Some(v) = st.volume.filter(|_| self.mixer.is_some()) {
            attrs.push_str(&format!(
                " volume=\"{}\" mute=\"{}\"",
                v.percent.min(100),
                u8::from(v.muted)
            ));
        }
        if st.state != PlayState::Stopped {
            if let Some(t) = st.track {
                attrs.push_str(&format!(
                    " ratingKey=\"{}\" key=\"/library/metadata/{}\" machineIdentifier=\"{}\"",
                    xml(&t.key),
                    xml(&t.key),
                    xml(&self.server.machine_id)
                ));
                if let Ok(url) = Url::parse(&self.server.base) {
                    attrs.push_str(&format!(
                        " address=\"{}\" port=\"{}\" protocol=\"{}\"",
                        xml(url.host_str().unwrap_or("")),
                        url.port_or_known_default().unwrap_or(32400),
                        xml(url.scheme())
                    ));
                }
                if let Some(q) = st.remote {
                    attrs.push_str(&format!(" containerKey=\"/playQueues/{}\" playQueueID=\"{}\" playQueueItemID=\"{}\" playQueueVersion=\"{}\"", q.info.id, q.info.id, q.item_id, q.info.version));
                }
            }
        }
        format!("<MediaContainer commandID=\"{}\" location=\"fullScreenMusic\"{}><Timeline {}/><Timeline type=\"video\" state=\"stopped\"/><Timeline type=\"photo\" state=\"stopped\"/></MediaContainer>", command_id, if disconnected { " disconnected=\"1\"" } else { "" }, attrs)
    }

    fn event_signature(&self) -> String {
        let s = self.status.lock().unwrap();
        format!(
            "{:?}",
            (
                s.state,
                s.started,
                s.seek_seq,
                s.queue_version,
                s.shuffle,
                s.repeat,
                s.volume
            )
        )
    }

    fn push_timelines(&self) {
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_millis(500))
            .timeout(Duration::from_secs(1))
            .build();
        loop {
            let quitting = self.stop.load(Ordering::Relaxed);
            let targets: Vec<_> = {
                let mut clients = self.controllers.lock().unwrap();
                clients.retain(|_, c| c.seen.elapsed() < TTL);
                clients
                    .iter()
                    .filter_map(|(id, c)| {
                        c.callback.as_ref().map(|url| {
                            (
                                id.clone(),
                                url.clone(),
                                c.command_id.unwrap_or(0),
                                c.last_body.clone(),
                                c.last_push,
                                c.last_event.clone(),
                            )
                        })
                    })
                    .collect()
            };
            for (id, url, command, previous, pushed, previous_event) in targets {
                let body = self.timeline(command, quitting);
                let event = self.event_signature();
                // Initial state immediately; at least one update per second
                // while playing, without sending a request every audio tick.
                if !quitting && previous == body {
                    continue;
                }
                if !quitting
                    && pushed.elapsed() < Duration::from_secs(1)
                    && (previous.is_empty() || previous_event == event)
                {
                    continue;
                }
                let result = agent
                    .post(&url)
                    .set("Content-Type", "application/xml")
                    .set("X-Plex-Client-Identifier", &self.settings.client_id)
                    .send_string(&body);
                if let Some(c) = self.controllers.lock().unwrap().get_mut(&id) {
                    c.last_push = Instant::now();
                    c.last_event = event;
                    if result.is_ok() {
                        c.last_body = body;
                    }
                }
            }
            if quitting {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid Companion header")
}
fn bad_request(e: anyhow::Error) -> (u16, String) {
    (400, e.to_string())
}
fn optional_number(p: &Params, name: &str) -> Result<Option<u64>> {
    p.get(name)
        .map(|s| {
            s.parse::<u64>()
                .with_context(|| format!("invalid {}", name))
        })
        .transpose()
}
fn required_number(p: &Params, name: &str) -> Result<u64> {
    optional_number(p, name)?.with_context(|| format!("missing {}", name))
}
fn digits(s: &str) -> Result<&str> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        bail!("invalid media ID");
    }
    Ok(s)
}
fn media_id(key: &str) -> Result<String> {
    Ok(digits(key.strip_prefix("/library/metadata/").unwrap_or(key))?.to_owned())
}
fn milliseconds(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0) as u64
}
fn local_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v| local_peer(IpAddr::V4(v)))
        }
    }
}
fn xml(s: &str) -> String {
    s.chars()
        .filter(|c| matches!(*c, '\t' | '\n' | '\r') || *c >= ' ')
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&apos;".to_owned(),
            _ => c.to_string(),
        })
        .collect()
}

fn gdm_socket(address: Ipv4Addr, port: u16) -> Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
    socket.join_multicast_v4(GDM_GROUP.ip(), &address)?;
    // std exposes multicast membership but not the outgoing IPv4 interface.
    // Use the advertised address for both, so --companion-address also works
    // on a machine with several NICs after removing cloud address publication.
    let interface = libc::in_addr {
        s_addr: u32::from_ne_bytes(address.octets()),
    };
    // SAFETY: interface is a live in_addr with the exact size required by
    // IP_MULTICAST_IF, and socket owns a valid descriptor throughout this call.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&interface as *const libc::in_addr).cast(),
            std::mem::size_of_val(&interface) as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("selecting GDM network interface");
    }
    socket.set_multicast_ttl_v4(1)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(socket)
}

fn gdm(state: &State, socket: UdpSocket, destination: SocketAddrV4) {
    let s = &state.settings;
    let fields = format!("Content-Type: plex/media-player\r\nName: {}\r\nPort: {}\r\nProduct: Plext\r\nProtocol: plex\r\nProtocol-Version: 1\r\nProtocol-Capabilities: {}\r\nResource-Identifier: {}\r\nDevice-Class: stb\r\nVersion: {}\r\n\r\n", s.name, s.port, CAPABILITIES, s.client_id, VERSION);
    let announce = |verb| {
        socket.send_to(
            format!("{} * HTTP/1.0\r\n{}", verb, fields).as_bytes(),
            destination,
        )
    };
    let _ = announce("HELLO");
    let mut last = Instant::now();
    let mut buf = [0u8; 4096];
    while !state.stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, peer)) => {
                if local_peer(peer.ip()) && buf[..n].starts_with(b"M-SEARCH * HTTP/1.") {
                    let _ =
                        socket.send_to(format!("HTTP/1.0 200 OK\r\n{}", fields).as_bytes(), peer);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                crate::log::write(
                    "companion: LAN discovery socket failed; restart with --companion",
                );
                break;
            }
        }
        if last.elapsed() >= Duration::from_secs(30) {
            let _ = announce("HELLO");
            last = Instant::now();
        }
    }
    let _ = announce("BYE");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Target;
    use crate::player::Player;
    use crate::plex::Plex;
    use serde_json::json;
    use std::sync::mpsc::{self, Receiver};

    struct Harness {
        state: Arc<State>,
        base: String,
        commands: Receiver<Cmd>,
        _player: Player,
        worker: Option<JoinHandle<()>>,
    }

    impl Harness {
        fn new(server: Server) -> Self {
            let http = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", http.server_addr());
            let player = Player::spawn(server.clone(), Target::Alsa("null".into()));
            let (tx, commands) = mpsc::channel();
            let state = Arc::new(State {
                settings: Settings::new(
                    32500,
                    "Pi & DAC".into(),
                    Ipv4Addr::LOCALHOST,
                    "receiver-id".into(),
                )
                .unwrap(),
                server,
                status: player.status_handle(),
                queue: player.queue_handle(),
                mixer: None,
                dispatch: Arc::new(move |cmd| tx.send(cmd).map_err(|_| anyhow::anyhow!("closed"))),
                controllers: Mutex::new(HashMap::new()),
                commands: Mutex::new(()),
                stop: Arc::new(AtomicBool::new(false)),
            });
            let s = state.clone();
            let worker = thread::spawn(move || {
                while !s.stop.load(Ordering::Relaxed) {
                    if let Some(r) = http.recv_timeout(Duration::from_millis(20)).unwrap() {
                        s.handle(r);
                    }
                }
            });
            Self {
                state,
                base,
                commands,
                _player: player,
                worker: Some(worker),
            }
        }

        fn get(&self, path: &str) -> ureq::Request {
            ureq::get(&format!("{}{}", self.base, path))
                .set("X-Plex-Client-Identifier", "phone")
                .set("X-Plex-Target-Client-Identifier", "receiver-id")
                .timeout(Duration::from_secs(3))
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.state.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn server(base: &str) -> Server {
        Server::new(Plex::new("test"), base, "secret", "pms-id")
    }
    fn empty() -> Harness {
        Harness::new(server("http://127.0.0.1:9"))
    }
    fn status(error: ureq::Error) -> u16 {
        match error {
            ureq::Error::Status(s, _) => s,
            other => panic!("{}", other),
        }
    }

    #[test]
    fn resources_identity_timeline_and_invalid_requests() {
        let h = empty();
        let r = h.get("/resources").call().unwrap();
        assert_eq!(r.header("X-Plex-Client-Identifier"), Some("receiver-id"));
        let body = r.into_string().unwrap();
        assert!(body.contains("title=\"Pi &amp; DAC\""));
        assert!(body.contains("protocolCapabilities=\"timeline,playback,playqueues\""));
        let body = h
            .get("/player/timeline/poll?commandID=88")
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(
            body.contains("commandID=\"0\""),
            "poll doesn't acknowledge an unexecuted command"
        );
        assert!(body.contains("type=\"music\" state=\"stopped\""));
        assert!(!body.contains(",volume"));
        assert!(!body.contains("secret"));
        assert_eq!(
            status(
                ureq::get(&format!("{}/resources", h.base))
                    .set("X-Plex-Target-Client-Identifier", "wrong")
                    .call()
                    .unwrap_err()
            ),
            404
        );
        assert_eq!(
            status(
                ureq::get(&format!("{}/player/playback/pause", h.base))
                    .call()
                    .unwrap_err()
            ),
            400
        );
        assert_eq!(
            status(
                h.get("/player/playback/pause?type=video")
                    .call()
                    .unwrap_err()
            ),
            400
        );
        assert_eq!(
            status(
                h.get("/player/playback/pause")
                    .set("Origin", "https://example.org")
                    .call()
                    .unwrap_err()
            ),
            403
        );
        assert!(h.commands.try_recv().is_err());
    }

    #[test]
    fn playback_ids_are_deduplicated_per_controller_and_seek_uses_milliseconds() {
        let h = empty();
        for _ in 0..2 {
            h.get("/player/playback/skipNext?type=music&commandID=0")
                .call()
                .unwrap();
        }
        assert!(matches!(
            h.commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            Cmd::Next
        ));
        assert!(h.commands.try_recv().is_err());
        ureq::get(&format!("{}/player/playback/skipNext?commandID=0", h.base))
            .set("X-Plex-Client-Identifier", "other-phone")
            .call()
            .unwrap();
        assert!(matches!(
            h.commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            Cmd::Next
        ));
        h.get("/player/playback/seekTo?offset=12500&commandID=1")
            .call()
            .unwrap();
        assert!(
            matches!(h.commands.recv_timeout(Duration::from_secs(2)).unwrap(), Cmd::SeekTo(s) if s == 12.5)
        );
        h.get("/player/playback/pause?commandID=2").call().unwrap();
        assert!(matches!(
            h.commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            Cmd::Pause
        ));
        let body = h
            .get("/player/timeline/poll?commandID=99")
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(body.contains("commandID=\"2\""));
        // A bad compound request must not apply the otherwise valid shuffle.
        assert_eq!(
            status(
                h.get("/player/playback/setParameters?shuffle=1&volume=101&commandID=3")
                    .call()
                    .unwrap_err()
            ),
            400
        );
        assert!(h.commands.try_recv().is_err());
        for (id, value, expected) in [
            (4, 1, Repeat::One),
            (5, 2, Repeat::All),
            (6, 0, Repeat::Off),
        ] {
            h.get(&format!(
                "/player/playback/setParameters?repeat={}&commandID={}",
                value, id
            ))
            .call()
            .unwrap();
            assert!(
                matches!(h.commands.recv_timeout(Duration::from_secs(2)).unwrap(), Cmd::SetRepeat(r) if r == expected)
            );
        }
    }

    #[test]
    fn casting_uses_trusted_server_and_selected_queue_item_even_for_duplicate_tracks() {
        let (base, log) = crate::test_server::start(vec![(
            "GET",
            "/playQueues/7?",
            json!({"MediaContainer": {
                "playQueueVersion": 4, "playQueueTotalCount": 2, "playQueueSelectedItemID": 102,
                "Metadata": [{"type":"track", "ratingKey":"42", "title":"first", "playQueueItemID":101},
                             {"type":"track", "ratingKey":"42", "title":"duplicate", "playQueueItemID":102}]
            }}),
        )]);
        let h = Harness::new(server(&base));
        h.get("/player/playback/playMedia?machineIdentifier=pms-id&key=%2Flibrary%2Fmetadata%2F42&containerKey=%2FplayQueues%2F7%3Fown%3D1&offset=8500&address=example.org&token=untrusted&commandID=1")
            .call().unwrap();
        match h.commands.recv_timeout(Duration::from_secs(2)).unwrap() {
            Cmd::PlayRemote { queue, offset } => {
                assert_eq!(queue.selected, 1);
                assert_eq!(queue.item_ids, [101, 102]);
                assert_eq!(queue.info.version, 4);
                assert_eq!(offset, 8.5);
            }
            _ => panic!("expected cast queue"),
        }
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(
            status(
                h.get("/player/playback/playMedia?machineIdentifier=other&key=42&commandID=2")
                    .call()
                    .unwrap_err()
            ),
            400
        );
        assert_eq!(status(h.get("/player/playback/playMedia?key=42&containerKey=http%3A%2F%2Fexample.org&commandID=3").call().unwrap_err()), 400);
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "no arbitrary server/URL requests"
        );
    }

    #[test]
    fn subscription_pushes_initial_state_and_unsubscribe_removes_it() {
        let h = empty();
        let callback = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = callback.server_addr().to_ip().unwrap().port();
        h.get(&format!(
            "/player/timeline/subscribe?protocol=http&port={}&commandID=8",
            port
        ))
        .call()
        .unwrap();
        let s = h.state.clone();
        let notifier = thread::spawn(move || s.push_timelines());
        let mut r = callback
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(r.method(), &Method::Post);
        assert_eq!(r.url(), "/:/timeline");
        let mut body = String::new();
        r.as_reader().read_to_string(&mut body).unwrap();
        assert!(body.contains("commandID=\"8\""));
        r.respond(Response::empty(200)).unwrap();
        h.get("/player/timeline/unsubscribe").call().unwrap();
        assert!(h.state.controllers.lock().unwrap().is_empty());
        h.state.stop.store(true, Ordering::Relaxed);
        notifier.join().unwrap();
    }

    #[test]
    fn gdm_announces_and_answers_local_discovery_without_account_registration() {
        let h = empty();
        let receiver = gdm_socket(Ipv4Addr::LOCALHOST, 0).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let receiver_address = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            receiver.local_addr().unwrap().port(),
        );
        let controller = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        controller
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let destination = match controller.local_addr().unwrap() {
            SocketAddr::V4(address) => address,
            _ => unreachable!(),
        };
        let state = h.state.clone();
        let worker = thread::spawn(move || gdm(&state, receiver, destination));
        let mut buf = [0u8; 4096];
        let (n, from) = controller.recv_from(&mut buf).unwrap();
        assert_eq!(from, receiver_address);
        let hello = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(hello.starts_with("HELLO * HTTP/1.0\r\n"));
        assert!(hello.contains("Resource-Identifier: receiver-id\r\n"));
        assert!(hello.contains("Port: 32500\r\n"));
        assert!(hello.contains("Protocol-Capabilities: timeline,playback,playqueues\r\n"));
        assert!(!hello.contains("secret"));
        controller
            .send_to(b"M-SEARCH * HTTP/1.0\r\n\r\n", receiver_address)
            .unwrap();
        let (n, from) = controller.recv_from(&mut buf).unwrap();
        assert_eq!(
            from, receiver_address,
            "reply comes from the discovery socket"
        );
        let reply = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(reply.starts_with("HTTP/1.0 200 OK\r\n"));
        assert!(reply.contains("Name: Pi & DAC\r\n"));
        h.state.stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        let (n, _) = controller.recv_from(&mut buf).unwrap();
        assert!(buf[..n].starts_with(b"BYE * HTTP/1.0\r\n"));
        assert!(
            h.commands.try_recv().is_err(),
            "discovery doesn't control playback"
        );
    }

    #[test]
    fn receiver_settings_reject_invalid_advertisements() {
        assert!(Settings::new(0, "Pi".into(), Ipv4Addr::LOCALHOST, "id".into()).is_err());
        assert!(Settings::new(
            32500,
            "Pi\r\nPort: 80".into(),
            Ipv4Addr::LOCALHOST,
            "id".into()
        )
        .is_err());
        assert!(Settings::new(32500, "Pi".into(), Ipv4Addr::UNSPECIFIED, "id".into()).is_err());
    }
}
