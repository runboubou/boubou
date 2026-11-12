//! `boubou serve`: a page in a browser is the conversation's microphone and speaker. This
//! computer opens it over loopback. Other devices on the local network open it over HTTPS,
//! which browsers require before they share a microphone: by a name browsers trust, from
//! [`names`], or by address with a certificate Boubou issues to itself. Robots and programs
//! that speak the OpenAI Realtime API, or send text to read aloud as they would to OpenAI,
//! connect to the same door and are carried to the model server's own implementation. One
//! visitor at a time has the model: one that holds the key from a printed link, or a robot or
//! program that cannot send the key and that the person let in from the terminal.

use crate::{
    audio::{CaptureFrame, Remote},
    progress, tui,
};
use anyhow::{Context, Result, ensure};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::PrivatePkcs8KeyDer,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
    fs::{self, File},
    io::{self, ErrorKind, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use tungstenite::{
    WebSocket,
    handshake::derive_accept_key,
    protocol::{Role, WebSocketConfig},
};

mod names;

const PAGE: &str = include_str!("serve.html");
/// The pictured steps for connecting a robot or an app.
const GUIDE: &str = "https://runboubou.com/connect/robots/";
/// Far above a 20 ms frame or a playback report.
const MAX_MESSAGE: usize = 1 << 16;
/// Requests answered at once; a flood beyond this is turned away.
const MAX_REQUESTS: usize = 32;
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// Apple's browsers reject server certificates valid for more than 398 days. The first of
/// these days is yesterday, for devices whose clocks run behind.
const CERTIFICATE_DAYS: u32 = 397;
/// How long a robot or program without the key waits for the person to let it in. Tests wait
/// out an unanswered question.
const ASKING: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 60 });

/// A connection from this computer, or an encrypted one from the network.
trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

type Socket = WebSocket<Box<dyn Stream>>;

/// What answering a request takes.
struct Desk {
    key: String,
    /// Set while someone is talking, or a text is being read aloud.
    seated: AtomicBool,
    /// Proves the network address to browsers.
    identity: Option<Arc<ServerConfig>>,
    /// The model server's own API for robots and programs, where it has one.
    api: Option<SocketAddr>,
    allowed: Arc<Allowed>,
    /// Robots and programs without the key, for the person to answer.
    asks: Sender<Ask>,
}

impl Desk {
    /// Whether a robot or program at `peer` that came without the key may talk: one the person
    /// let in before may, and another once the person agrees.
    fn lets_in(&self, peer: IpAddr) -> bool {
        if self.allowed.contains(peer) {
            return true;
        }
        let (answer, answered) = mpsc::channel();
        let until = Instant::now() + ASKING;
        self.asks
            .send(Ask {
                peer,
                until,
                answer,
            })
            .is_ok()
            && answered.recv_timeout(ASKING).unwrap_or(false)
    }
}

/// A robot or program without the key, waiting `until` for the person's answer.
struct Ask {
    peer: IpAddr,
    until: Instant,
    answer: Sender<bool>,
}

/// Addresses of the robots and programs the person let in without the key. They are kept
/// between starts, so a robot's settings keep working. Deleting the file forgets them.
struct Allowed {
    file: PathBuf,
    addresses: Mutex<HashSet<IpAddr>>,
}

impl Allowed {
    fn load(cache: &Path) -> Result<Self> {
        let file = cache.join("serve").join("allowed");
        let addresses = match fs::read_to_string(&file) {
            Ok(text) => text.lines().filter_map(|line| line.parse().ok()).collect(),
            Err(error) if error.kind() == ErrorKind::NotFound => HashSet::new(),
            Err(error) => return Err(error).context(format!("Cannot read {}", file.display())),
        };
        let addresses = Mutex::new(addresses);
        Ok(Self { file, addresses })
    }

    fn contains(&self, peer: IpAddr) -> bool {
        self.addresses.lock().unwrap().contains(&peer)
    }

    fn add(&self, peer: IpAddr) -> Result<()> {
        let mut addresses = self.addresses.lock().unwrap();
        addresses.insert(peer);
        let mut lines: Vec<_> = addresses
            .iter()
            .map(|address| format!("{address}\n"))
            .collect();
        lines.sort();
        fs::write(&self.file, lines.concat())
            .with_context(|| format!("Cannot remember {peer} in {}", self.file.display()))
    }
}

/// Held by whoever has the model. Dropping it lets the next visitor in.
struct Seat(Arc<Desk>);

impl Drop for Seat {
    fn drop(&mut self) {
        self.0.seated.store(false, Ordering::SeqCst);
    }
}

/// Where `boubou serve` listens.
#[derive(clap::Args)]
pub struct Options {
    /// The port that serves the page.
    #[arg(long, default_value_t = 2682)]
    port: u16,
    /// Accept connections from this computer only.
    #[arg(long)]
    local: bool,
    /// Offer the network address with Boubou's own certificate only, which phones warn about
    /// once, and ask lan.runboubou.com for no certificate.
    #[arg(long)]
    self_signed: bool,
}

impl Options {
    /// Contacts no names service, as `--offline` promises no network.
    pub fn offline(mut self) -> Self {
        self.self_signed = true;
        self
    }
}

/// Where to connect. The page's addresses end in the key as a fragment, which browsers never
/// put in a request.
struct Links {
    here: String,
    network: Option<String>,
    /// The network address itself, when `network` is a name: for networks whose router does
    /// not let names point at local addresses.
    fallback: Option<String>,
    /// For robots and programs: encrypted by name, or in the clear by address, since they
    /// cannot check Boubou's own certificate.
    realtime: Option<String>,
    /// Where robots and programs that take only an address and a port connect, in the clear.
    bare: Option<(Ipv4Addr, u16)>,
}

impl Links {
    /// What is printed under the checklist: a section for each way to connect. What a person
    /// scans or types stands out and the long links stay dim. `code` is the QR code of the
    /// network link, empty where the terminal cannot show one.
    fn guide(&self, ink: &Ink, columns: usize, code: &[String]) -> Vec<String> {
        let Ink {
            bold,
            dim,
            reset,
            accent,
        } = ink;
        let rule = "─".repeat(columns.saturating_sub(4).min(56));
        let mut lines = Vec::new();
        let mut section = |title: &str, body: Vec<String>| {
            lines.extend([
                String::new(),
                format!("  {dim}{rule}{reset}"),
                format!("  {bold}{title}{reset}"),
            ]);
            lines.extend(body.into_iter().map(|line| match line.is_empty() {
                true => line,
                false => format!("  {line}{reset}"),
            }));
        };
        if let Some(link) = &self.network {
            let mut body = match code {
                [] => vec![
                    "Open this link on the phone, on the same Wi-Fi:".into(),
                    link.clone(),
                ],
                // On a dark terminal the code is a light card: a line apart from the text.
                code => {
                    let scan = "Scan with the phone's camera, on the same Wi-Fi.";
                    let mut body = vec![scan.to_owned(), String::new()];
                    body.extend_from_slice(code);
                    body.extend([String::new(), format!("{dim}Or open {link}")]);
                    body
                }
            };
            body.push(match &self.fallback {
                Some(address) => format!(
                    "{dim}If the page does not open, use {address} and continue past the browser's warning."
                ),
                None => format!(
                    "{dim}A phone's browser warns once that it does not know Boubou's certificate. Continue to the page."
                ),
            });
            section("Phone", body);
        }
        if let (Some(link), Some((address, port))) = (&self.realtime, &self.bare) {
            section(
                "Robot or app",
                vec![
                    format!("{bold}1{reset}  In its settings, type these two:"),
                    String::new(),
                    format!("   Address   {bold}{accent}{address}"),
                    format!("   Port      {bold}{accent}{port}"),
                    String::new(),
                    format!("{bold}2{reset}  Boubou then asks here before it lets it in. Press y."),
                    String::new(),
                    format!("{dim}Guide with pictures: {GUIDE}"),
                    format!(
                        "{dim}Takes one full address instead? This one carries the key, so Boubou does not ask:"
                    ),
                    format!("{dim}{link}"),
                ],
            );
        }
        section(progress::HERE, vec![self.here.clone()]);
        lines.push(String::new());
        lines
    }
}

/// The QR code of `link` as lines for a terminal, as small as a phone's camera reads from a
/// screen: the lowest error correction and two light modules around it. A cell holds two
/// modules, the upper one as the color of `▀` and the lower one as the cell's own, so the code
/// stays solid where a terminal spaces its lines.
fn code(paint: tui::Paint, link: &str) -> Result<Vec<String>> {
    let code = qrcode::QrCode::with_error_correction_level(link, qrcode::EcLevel::L)?;
    let (size, modules) = (code.width() as isize, code.to_colors());
    let tone = |x: isize, y: isize| {
        let inside = (0..size).contains(&x) && (0..size).contains(&y);
        match inside && modules[(y * size + x) as usize] == qrcode::Color::Dark {
            true => tui::CODE_DARK,
            false => tui::CODE_LIGHT,
        }
    };
    let cell = |x, y| format!("{}{}▀", paint.fg(tone(x, y)), paint.bg(tone(x, y + 1)));
    Ok((-2..size + 2)
        .step_by(2)
        .map(|y| (-2..size + 2).map(|x| cell(x, y)).collect())
        .collect())
}

/// Styles for what `serve` prints under the checklist. All empty where stderr takes no escape
/// codes, so the same lines read well in a log.
#[derive(Default)]
struct Ink {
    bold: &'static str,
    dim: &'static str,
    reset: &'static str,
    accent: String,
}

impl Ink {
    fn detect() -> Self {
        if !tui::interactive(&io::stderr()) {
            return Self::default();
        }
        Self {
            bold: tui::BOLD,
            dim: tui::DIM,
            reset: tui::RESET,
            accent: tui::Paint::detect().fg(tui::ACCENT),
        }
    }

    /// One event in the checklist's shape: a mark, then what happened.
    fn event(&self, mark: char, text: &str) -> String {
        let Self { accent, reset, .. } = self;
        format!("  {accent}{mark}{reset}  {text}")
    }
}

/// A browser that has taken the seat. Its audio starts when the conversation is ready for it.
pub struct Guest(Socket, Seat);

impl Guest {
    pub fn start(mut self) -> (Remote, Receiver<CaptureFrame>) {
        // The page has been sending since it connected. What piled up while the conversation
        // was being prepared is stale, and would arrive as one burst; reading stops at the
        // first pause, which a live microphone leaves between frames.
        while self.0.read().is_ok() {}
        Remote::start(self.0, self.1)
    }
}

/// Listens for visitors and admits one at a time.
pub struct Door {
    arrivals: Receiver<Guest>,
    returning: Cell<bool>,
    asks: Receiver<Ask>,
    allowed: Arc<Allowed>,
    /// Addresses the person turned away since this start.
    refused: RefCell<HashSet<IpAddr>>,
    /// Robots and programs from other addresses that came while the person was asked.
    later: RefCell<VecDeque<Ask>>,
}

impl Door {
    /// Starts serving and tells the person where to connect. `cache` keeps the certificate
    /// between starts. `api` is the model server's own API for robots and programs, where it
    /// has one.
    pub fn open(options: &Options, cache: &Path, api: Option<SocketAddr>) -> Result<Self> {
        let network = network_address().filter(|_| !options.local);
        let service = (!options.self_signed).then_some(names::SERVICE);
        let (door, links) = Self::bind(options.port, cache, network, api, service)?;
        let note = "Ready. Choose how to connect below.";
        if !progress::finish(note) {
            eprintln!("{note}");
        }
        let ink = Ink::detect();
        // A terminal that shows colors can show the code.
        let code = match &links.network {
            Some(link) if !ink.accent.is_empty() => code(tui::Paint::detect(), link)?,
            _ => Vec::new(),
        };
        let columns = crossterm::terminal::size().map_or(80, |(columns, _)| columns as usize);
        for line in links.guide(&ink, columns, &code) {
            eprintln!("{line}");
        }
        Ok(door)
    }

    /// Listens on `network` as well as loopback when this computer has such an address, by a
    /// name certified by the names `service` when it is given and answers.
    fn bind(
        port: u16,
        cache: &Path,
        network: Option<Ipv4Addr>,
        api: Option<SocketAddr>,
        service: Option<&str>,
    ) -> Result<(Self, Links)> {
        let names = match (network, service) {
            (Some(_), Some(service)) => Some(names::start(cache, service)?),
            _ => None,
        };
        let identity = network
            .map(|address| identity(cache, address, names.clone()))
            .transpose()
            .context(
                "Cannot prepare the HTTPS certificate. Delete the cache's serve folder and retry.",
            )?;
        let listen = |address: Ipv4Addr| {
            TcpListener::bind((address, port)).with_context(|| {
                format!("Cannot listen on port {port}. Choose another with --port.")
            })
        };
        let mut listener = listen(Ipv4Addr::LOCALHOST)?;
        if network.is_some() {
            // Loopback came first because macOS lets every-address listeners share a port that
            // another program holds on loopback, and the local link would then reach that program.
            drop(listener);
            listener = listen(Ipv4Addr::UNSPECIFIED)?;
        }
        let port = listener.local_addr()?.port();
        let key = key(cache)?;
        // The literal address: `localhost` may resolve to IPv6 first, where nothing listens.
        let reachable = network.unwrap_or(Ipv4Addr::LOCALHOST);
        let named = network.zip(names.filter(|names| names.certificate().is_some()));
        let named = named.map(|(address, names)| names.name(address));
        let by_address = network.map(|address| format!("https://{address}:{port}/#{key}"));
        let links = Links {
            here: format!("http://127.0.0.1:{port}/#{key}"),
            network: match &named {
                Some(name) => Some(format!("https://{name}:{port}/#{key}")),
                None => by_address.clone(),
            },
            fallback: named.as_ref().and(by_address),
            realtime: api.map(|_| match &named {
                Some(name) => format!("wss://{name}:{port}/v1/realtime?key={key}"),
                None => format!("ws://{reachable}:{port}/v1/realtime?key={key}"),
            }),
            bare: api.map(|_| (reachable, port)),
        };
        let allowed = Arc::new(Allowed::load(cache)?);
        let (asks, asked) = mpsc::channel();
        let desk = Arc::new(Desk {
            key,
            seated: AtomicBool::new(false),
            identity,
            api,
            allowed: allowed.clone(),
            asks,
        });
        let (admitted, arrivals) = mpsc::channel();
        thread::Builder::new()
            .name("boubou-serve".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // Every request in progress holds the desk.
                    if Arc::strong_count(&desk) > MAX_REQUESTS {
                        continue;
                    }
                    let (desk, admitted) = (desk.clone(), admitted.clone());
                    // Browsers open connections they leave idle; each waits on its own thread.
                    thread::spawn(move || {
                        // A failed request costs only its own connection.
                        if let Ok(Some(arrival)) = accept(stream, &desk) {
                            let _ = admitted.send(arrival);
                        }
                    });
                }
            })?;
        let door = Self {
            arrivals,
            returning: Cell::new(false),
            asks: asked,
            allowed,
            refused: RefCell::default(),
            later: RefCell::default(),
        };
        Ok((door, links))
    }

    /// Waits for the next browser, calling `idle` meanwhile so the model's session stays alive.
    /// Asks the person about robots and programs that came without the key. Returns `None` once
    /// `stopped` is set.
    pub fn wait(
        &self,
        stopped: &AtomicBool,
        mut idle: impl FnMut() -> Result<()>,
    ) -> Result<Option<Guest>> {
        if self.returning.replace(true) {
            let gone = "The browser disconnected. Open the link again to continue.";
            eprintln!("{}", Ink::detect().event('○', gone));
        }
        while !stopped.load(Ordering::SeqCst) {
            let later = self.later.borrow_mut().pop_front();
            if let Some(ask) = later.or_else(|| self.asks.try_recv().ok()) {
                // Keys answer the question one at a time, without echo, from the first one asked.
                let mut raw = false;
                let settled = self.settle(ask, || {
                    if !raw {
                        // Without a terminal to ask in, the robot is turned away; Boubou goes on.
                        if let Err(error) = crossterm::terminal::enable_raw_mode() {
                            eprint!("\r\nCannot ask in this terminal: {error}.");
                            return Ok(Some(false));
                        }
                        raw = true;
                    }
                    pressed(stopped, &mut idle)
                });
                if raw {
                    crossterm::terminal::disable_raw_mode()?;
                }
                settled?;
                continue;
            }
            if let Ok(guest) = self.arrivals.recv_timeout(Duration::from_millis(20)) {
                return Ok(Some(guest));
            }
            idle()?;
        }
        Ok(None)
    }

    /// Answers a robot or program that came without the key: as the person answered for its
    /// address before, or once `decide` returns their answer. Robots come back when their
    /// handshake times out, so every visit from the address waits for the same answer, and the
    /// question is dropped once none is waiting any more.
    fn settle(&self, ask: Ask, mut decide: impl FnMut() -> Result<Option<bool>>) -> Result<()> {
        let peer = ask.peer;
        let mut waiting = vec![ask];
        let allowed = if self.allowed.contains(peer) {
            true
        } else if self.refused.borrow().contains(&peer) {
            false
        } else {
            let ink = Ink::detect();
            let asks = format!("A robot or program at {peer} asks to talk without the key.");
            eprint!(
                "{}\r\n     {}Let it in? [y/N]{} ",
                ink.event('?', &asks),
                ink.bold,
                ink.reset
            );
            let answer = loop {
                while let Ok(ask) = self.asks.try_recv() {
                    match ask.peer == peer {
                        true => waiting.push(ask),
                        false => self.later.borrow_mut().push_back(ask),
                    }
                }
                // Dropping a visit's answer turns it away.
                waiting.retain(|ask| Instant::now() < ask.until);
                if waiting.is_empty() {
                    break None;
                }
                if let Some(answer) = decide()? {
                    break Some(answer);
                }
            };
            // Lines start over by hand while keys are read raw.
            let (mark, said) = match answer {
                Some(true) => {
                    self.allowed.add(peer)?;
                    ('✓', "can talk without the key from now on")
                }
                Some(false) => {
                    self.refused.borrow_mut().insert(peer);
                    ('✗', "was turned away until Boubou starts again")
                }
                None => (
                    '○',
                    "stopped asking. It is asked about again when it returns",
                ),
            };
            eprint!("\r\n{}\r\n", ink.event(mark, &format!("{peer} {said}.")));
            answer == Some(true)
        };
        for ask in waiting {
            // The robot or program may have stopped waiting.
            let _ = ask.answer.send(allowed);
        }
        Ok(())
    }
}

/// The person's answer when they press a key within a moment: `y` lets the robot in, and any
/// other key turns it away, as does stopping Boubou: by Ctrl+C here, or by a signal from
/// elsewhere, without waiting for a key. Keeps the model's session alive.
fn pressed(stopped: &AtomicBool, idle: &mut impl FnMut() -> Result<()>) -> Result<Option<bool>> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    if stopped.load(Ordering::SeqCst) {
        return Ok(Some(false));
    }
    if event::poll(Duration::from_millis(20))?
        && let Event::Key(key) = event::read()?
    {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            stopped.store(true, Ordering::SeqCst);
            return Ok(Some(false));
        }
        return Ok(Some(matches!(key.code, KeyCode::Char('y' | 'Y'))));
    }
    idle()?;
    Ok(None)
}

/// Makes `folder` and holds it until the returned file is dropped, so that two starts at once
/// do not both write a secret there.
fn hold(folder: &Path) -> Result<File> {
    fs::create_dir_all(folder)?;
    let lock = File::create(folder.join("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

/// An empty file that only its owner can read, also when it replaces one that others could.
fn private(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let file = options.open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// The key that lets a visitor talk. It is made once and kept, so a bookmarked link or a
/// robot's settings keep working across starts. Deleting its file replaces it.
fn key(cache: &Path) -> Result<String> {
    let folder = cache.join("serve");
    let _held = hold(&folder)?;
    let stored = folder.join("key");
    if !stored.exists() {
        let mut fresh = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut fresh)?;
        private(&stored)?.write_all(crate::assets::hex(&fresh).as_bytes())?;
    }
    let key = fs::read_to_string(&stored)?.trim().to_owned();
    ensure!(
        key.len() == 32 && key.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "The key in {} is damaged. Delete that file and retry.",
        stored.display()
    );
    Ok(key)
}

/// This computer's address on a local network such as Wi-Fi or Ethernet, if it has one.
fn network_address() -> Option<Ipv4Addr> {
    let mut list = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut next = list;
    while let Some(interface) = unsafe { next.as_ref() } {
        next = interface.ifa_next;
        // Wi-Fi and Ethernet broadcast; VPN tunnels are point-to-point.
        let wanted = libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_BROADCAST;
        let flags = interface.ifa_flags as libc::c_int;
        let address = interface.ifa_addr;
        if flags & (wanted | libc::IFF_LOOPBACK | libc::IFF_POINTOPOINT) != wanted
            || address.is_null()
            || libc::c_int::from(unsafe { (*address).sa_family }) != libc::AF_INET
        {
            continue;
        }
        let address = unsafe { address.cast::<libc::sockaddr_in>().read_unaligned() };
        let address = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if address.is_private() {
            found.get_or_insert(address);
        }
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

/// Chooses the certificate a browser is shown: the one for this computer's names when it
/// asked for one of them, and Boubou's own for its address otherwise.
#[derive(Debug)]
struct Certificates {
    own: Arc<CertifiedKey>,
    names: Option<Arc<names::Names>>,
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let asked = hello.server_name().zip(self.names.as_ref());
        let named = asked.and_then(|(name, names)| names.certificate_for(name));
        Some(named.unwrap_or_else(|| self.own.clone()))
    }
}

/// HTTPS for this computer's names, through `names`, and for `address`, with a certificate and
/// key Boubou makes for it. Those are made once and kept, so a browser that was told to trust
/// the certificate keeps trusting it.
fn identity(
    cache: &Path,
    address: Ipv4Addr,
    names: Option<Arc<names::Names>>,
) -> Result<Arc<ServerConfig>> {
    let folder = cache.join("serve").join(address.to_string());
    let (certificate, key) = (folder.join("certificate.der"), folder.join("key.der"));
    // Until the pair is read: two starts at once must not mix one's key with the other's
    // certificate.
    let _held = hold(&folder)?;
    let current = key.is_file()
        && fs::metadata(&certificate)
            .and_then(|file| file.modified())
            .is_ok_and(|made| made.elapsed().unwrap_or(Duration::MAX) < DAY * 365);
    if !current {
        let pair = rcgen::KeyPair::generate()?;
        let mut request = rcgen::CertificateParams::new([address.to_string()])?;
        request.distinguished_name = rcgen::DistinguishedName::new();
        request
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Boubou");
        request.not_before = (SystemTime::now() - DAY).into();
        request.not_after = (SystemTime::now() + DAY * (CERTIFICATE_DAYS - 1)).into();
        request.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let made = request.self_signed(&pair)?;
        private(&key)?.write_all(&pair.serialize_der())?;
        // Written last: its presence marks a complete pair.
        fs::write(&certificate, made.der())?;
    }
    let key = PrivatePkcs8KeyDer::from(fs::read(key)?);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // Checks that the stored key belongs to the stored certificate.
    let own = CertifiedKey::from_der(vec![fs::read(certificate)?.into()], key.into(), &provider)?;
    let certificates = Certificates {
        own: Arc::new(own),
        names,
    };
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(certificates));
    Ok(Arc::new(config))
}

/// Whether a connection may use the page: any from this computer, and encrypted ones from the
/// network. Plain HTTP would carry a browser's key and voice across the network in the clear.
/// `None` for peers that get no answer at all: only private addresses are on the local network.
fn trusted(peer: IpAddr, encrypted: bool) -> Option<bool> {
    let private = matches!(peer, IpAddr::V4(peer) if peer.is_private());
    (peer.is_loopback() || private).then_some(peer.is_loopback() || encrypted)
}

/// Takes a new connection as HTTPS or plain HTTP and answers its request.
fn accept(stream: TcpStream, desk: &Arc<Desk>) -> Result<Option<Guest>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut first = [0];
    ensure!(stream.peek(&mut first)? == 1, "The connection closed");
    // A TLS connection opens with a handshake record, type 22.
    let identity = desk.identity.clone().filter(|_| first[0] == 22);
    let peer = stream.peer_addr()?.ip();
    let trusted = trusted(peer, identity.is_some()).context("Not from here")?;
    let socket = stream.try_clone()?;
    match identity {
        Some(identity) => {
            let session = ServerConnection::new(identity)?;
            answer(
                StreamOwned::new(session, stream),
                &socket,
                trusted,
                peer,
                desk,
            )
        }
        None => answer(stream, &socket, trusted, peer, desk),
    }
}

/// Serves the page, gives the seat to a browser that holds the key, or carries a robot or
/// program that holds it, or that the person let in, to the model server's API. `socket` is
/// the connection underneath `stream`, from `peer`.
fn answer(
    mut stream: impl Stream + 'static,
    socket: &TcpStream,
    trusted: bool,
    peer: IpAddr,
    desk: &Arc<Desk>,
) -> Result<Option<Guest>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = Vec::new();
    let (method, target, accept, bearer, head) = loop {
        let mut chunk = [0; 1024];
        let read = stream.read(&mut chunk)?;
        ensure!(
            read > 0 && request.len() + read <= 8192 && Instant::now() < deadline,
            "Not an HTTP request"
        );
        request.extend_from_slice(&chunk[..read]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        if let httparse::Status::Complete(head) = parsed.parse(&request)? {
            let header = |name: &str| {
                let mut headers = parsed.headers.iter();
                headers.find(|header| header.name.eq_ignore_ascii_case(name))
            };
            let accept = header("Sec-WebSocket-Key").map(|key| derive_accept_key(key.value));
            // Programs built for OpenAI send the key as their API key.
            let bearer = header("Authorization")
                .and_then(|sent| sent.value.strip_prefix(b"Bearer "))
                .map(|key| String::from_utf8_lossy(key).into_owned());
            let method = parsed.method.unwrap_or_default().to_owned();
            let target = parsed.path.unwrap_or_default().to_owned();
            break (method, target, accept, bearer, head);
        }
    };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let keyed = query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("key="))
        .chain(bearer.as_deref())
        .any(|given| same(given, &desk.key));
    // Robots whose settings take only an address and a port cannot send the key.
    let robot = matches!(
        (method.as_str(), path, accept.is_some()),
        ("GET", "/v1/realtime", true) | ("POST", "/v1/audio/speech", false)
    );
    let allowed = keyed || (robot && desk.api.is_some() && desk.lets_in(peer) && !closed(socket));
    let mut respond = |status: &str, body: &str| -> Result<Option<Guest>> {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        stream.flush()?;
        Ok(None)
    };
    let (accept, api) = match (method.as_str(), path, accept, desk.api) {
        ("GET", "/", None, _) if trusted => return respond("200 OK", PAGE),
        ("GET", "/talk", Some(accept), _) if trusted && keyed => (accept, None),
        ("GET", "/v1/realtime", Some(accept), Some(api)) if allowed => (accept, Some(api)),
        // A text to read aloud, which opens no socket.
        ("POST", "/v1/audio/speech", None, Some(api)) if allowed => (String::new(), Some(api)),
        _ => return respond("404 Not Found", ""),
    };
    if desk.seated.swap(true, Ordering::SeqCst) {
        return respond("409 Conflict", "");
    }
    // Held from here on, so a handshake that fails frees the seat again.
    let seat = Seat(Arc::clone(desk));
    if let Some(api) = api {
        return relay(stream, socket, api, request, method == "POST").map(|()| None);
    }
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )?;
    stream.flush()?;
    socket.set_nodelay(true)?;
    // The device sends queued speech between reads.
    socket.set_read_timeout(Some(Duration::from_millis(10)))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let stream: Box<dyn Stream> = Box::new(stream);
    let socket =
        WebSocket::from_partially_read(stream, request.split_off(head), Role::Server, Some(config));
    Ok(Some(Guest(socket, seat)))
}

/// Carries a connection to the model server's API byte for byte, from the request already
/// read, until either side closes. The model server answers a robot's handshake itself. A
/// text to read aloud is asked for `once`: the model server closes when it has answered,
/// which frees the seat.
fn relay(
    mut visitor: impl Stream,
    socket: &TcpStream,
    api: SocketAddr,
    mut request: Vec<u8>,
    once: bool,
) -> Result<()> {
    if once {
        let line = request.iter().position(|&byte| byte == b'\n');
        let headers = line.map_or(0, |end| end + 1);
        request.splice(headers..headers, *b"Connection: close\r\n");
    }
    let mut server = TcpStream::connect_timeout(&api, Duration::from_secs(2))?;
    server.write_all(&request)?;
    // Each side is read in turn, for a few milliseconds at a time.
    for side in [socket, &server] {
        side.set_nodelay(true)?;
        side.set_read_timeout(Some(Duration::from_millis(5)))?;
        side.set_write_timeout(Some(Duration::from_secs(5)))?;
    }
    if !once {
        let connected = "A robot or program connected.";
        eprintln!("{}", Ink::detect().event('●', connected));
    }
    let mut buffer = [0; 16 * 1024];
    let mut carry = || -> io::Result<()> {
        while pass(&mut visitor, &mut server, &mut buffer)?
            && pass(&mut server, &mut visitor, &mut buffer)?
        {}
        Ok(())
    };
    let ended = carry();
    if !once {
        let gone = "The robot or program disconnected.";
        eprintln!("{}", Ink::detect().event('○', gone));
    }
    Ok(ended?)
}

/// Moves what `from` has ready to `to`. False once `from` has closed.
fn pass(from: &mut impl Read, to: &mut impl Write, buffer: &mut [u8]) -> io::Result<bool> {
    match from.read(buffer) {
        Ok(0) => Ok(false),
        Ok(read) => {
            to.write_all(&buffer[..read])?;
            to.flush()?;
            Ok(true)
        }
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Whether the other side closed `socket`, as a robot does that stopped waiting to be let in.
fn closed(socket: &TcpStream) -> bool {
    let _ = socket.set_nonblocking(true);
    let closed = matches!(socket.peek(&mut [0]), Ok(0));
    let _ = socket.set_nonblocking(false);
    closed
}

/// Compares without stopping at the first difference.
fn same(given: &str, key: &str) -> bool {
    given.len() == key.len()
        && given
            .bytes()
            .zip(key.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Device;

    type Page<S = TcpStream> = WebSocket<S>;

    /// A cache folder of this test's own.
    fn cache() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let next = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("boubou-serve-{}-{next}", std::process::id()))
    }

    /// A door that only this computer can reach, in front of a model without a Realtime API.
    fn local() -> (Door, String) {
        let cache = cache();
        let (door, links) = Door::bind(0, &cache, None, None, None).unwrap();
        fs::remove_dir_all(cache).unwrap();
        assert!(links.network.is_none() && links.realtime.is_none());
        (door, links.here)
    }

    /// A stand-in for a model server's Realtime API: it greets, then echoes.
    fn model_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut socket = tungstenite::accept(stream).unwrap();
                    socket.send("created".into()).unwrap();
                    while let Ok(message) = socket.read() {
                        if message.is_text() {
                            socket.send(message).unwrap();
                        }
                    }
                });
            }
        });
        address
    }

    /// Connects as a robot or program would, with the key in the address or as an API key.
    fn realtime(link: &str, query: &str, bearer: Option<&str>) -> tungstenite::Result<Page> {
        use tungstenite::client::IntoClientRequest;
        let url = format!("ws://{}/v1/realtime{query}", address(link));
        let mut request = url.into_client_request().unwrap();
        if let Some(bearer) = bearer {
            let value = format!("Bearer {bearer}").parse().unwrap();
            request.headers_mut().insert("Authorization", value);
        }
        handshake(request, TcpStream::connect(address(link)).unwrap())
    }

    fn handshake<S: Read + Write>(
        request: impl tungstenite::client::IntoClientRequest,
        stream: S,
    ) -> tungstenite::Result<Page<S>> {
        tungstenite::client(request, stream)
            .map(|(socket, _)| socket)
            .map_err(|error| match error {
                tungstenite::HandshakeError::Failure(error) => error,
                tungstenite::HandshakeError::Interrupted(_) => unreachable!("blocking socket"),
            })
    }

    fn request(link: &str, text: &str) -> String {
        let mut stream = TcpStream::connect(address(link)).unwrap();
        // A refused request may be cut off while it is still being written.
        let _ = stream.write_all(text.as_bytes());
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn address(link: &str) -> &str {
        link.split('/').nth(2).unwrap()
    }

    fn key(link: &str) -> &str {
        link.split('#').nth(1).unwrap()
    }

    fn talk<S: Read + Write>(link: &str, key: &str, stream: S) -> tungstenite::Result<Page<S>> {
        handshake(format!("ws://{}/talk?key={key}", address(link)), stream)
    }

    fn plain(link: &str, key: &str) -> tungstenite::Result<Page> {
        talk(link, key, TcpStream::connect(address(link)).unwrap())
    }

    fn status(result: tungstenite::Result<Page>) -> u16 {
        match result {
            Err(tungstenite::Error::Http(response)) => response.status().as_u16(),
            other => panic!("Expected an HTTP refusal, got {:?}", other.map(drop)),
        }
    }

    /// Visits again until whoever held the seat has left it.
    fn once_free(mut visit: impl FnMut() -> tungstenite::Result<Page>) -> Page {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match visit() {
                Ok(visitor) => return visitor,
                refused => assert_eq!(status(refused), 409),
            }
            assert!(Instant::now() < deadline, "The seat was never freed");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Visits without the key while the person answers as `decide` says, when asked. Returns
    /// what the visit got and the address the door asked about.
    fn asking<T: Send>(
        door: &Door,
        decide: impl FnMut() -> Result<Option<bool>>,
        visit: impl FnOnce() -> T + Send,
    ) -> (T, IpAddr) {
        thread::scope(|scope| {
            let visitor = scope.spawn(visit);
            let ask = door.asks.recv_timeout(Duration::from_secs(5));
            let ask = ask.expect("Nobody was asked");
            let peer = ask.peer;
            door.settle(ask, decide).unwrap();
            (visitor.join().unwrap(), peer)
        })
    }

    fn wait(door: &Door) -> Remote {
        door.wait(&AtomicBool::new(false), || Ok(()))
            .unwrap()
            .unwrap()
            .start()
            .0
    }

    #[test]
    fn the_page_is_public_and_everything_else_is_missing() {
        let (_door, link) = local();
        let page = request(&link, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
        assert!(page.contains("Cache-Control: no-store"));
        assert!(page.ends_with(PAGE));
        assert!(!page.contains(key(&link)), "The key stays in the link");
        for other in [
            "GET /talk HTTP/1.1\r\n\r\n",
            "POST / HTTP/1.1\r\n\r\n",
            "GET /favicon.ico HTTP/1.1\r\n\r\n",
        ] {
            assert!(
                request(&link, other).starts_with("HTTP/1.1 404 "),
                "{other}"
            );
        }
        let unfinished = format!("GET / HTTP/1.1\r\nCookie: {}", "x".repeat(9000));
        assert_eq!(request(&link, &unfinished), "");
    }

    #[test]
    fn only_the_key_opens_the_seat_and_one_browser_holds_it() {
        let (door, link) = local();
        let key = key(&link);
        assert_eq!(key.len(), 32);
        assert_ne!(key, self::key(&local().1));
        for wrong in [
            "",
            "0",
            &key[1..],
            &format!("{key}0"),
            &key.replace(&key[..1], "g"),
        ] {
            assert_eq!(status(plain(&link, wrong)), 404, "{wrong:?}");
        }

        let mut first = plain(&link, key).unwrap();
        let mut remote = wait(&door);
        assert_eq!(status(plain(&link, key)), 409);
        remote.enqueue(&[1, 2, 3]).unwrap();
        assert_eq!(
            first.read().unwrap().into_data().as_ref(),
            [1, 0, 2, 0, 3, 0]
        );

        // The seat frees once the first browser's connection has ended.
        first.close(None).unwrap();
        let _second = once_free(|| plain(&link, key));
        assert!(remote.left());
        assert!(!wait(&door).left());
    }

    #[test]
    fn audio_sent_before_the_conversation_was_ready_is_dropped() {
        let (door, link) = local();
        let mut page = plain(&link, key(&link)).unwrap();
        let frame =
            |sample: i16| tungstenite::Message::Binary(sample.to_le_bytes().repeat(320).into());
        // Two seconds of microphone pile up while the model prepares the conversation.
        for _ in 0..100 {
            page.send(frame(1)).unwrap();
        }
        let guest = door.wait(&AtomicBool::new(false), || Ok(()));
        thread::sleep(Duration::from_millis(50));
        let (remote, captured) = guest.unwrap().unwrap().start();
        page.send(frame(2)).unwrap();
        let heard = captured.recv_timeout(Duration::from_secs(5));
        assert!(matches!(heard, Ok(CaptureFrame::Samples(frame)) if frame.samples == [2; 320]));
        assert!(!remote.left());
    }

    #[test]
    fn waiting_keeps_the_session_alive_until_stopped() {
        let (door, _link) = local();
        let stopped = AtomicBool::new(false);
        let mut ticks = 0;
        let idle = || {
            ticks += 1;
            stopped.store(ticks == 3, Ordering::SeqCst);
            Ok(())
        };
        assert!(door.wait(&stopped, idle).unwrap().is_none());
        assert_eq!(ticks, 3);
    }

    #[test]
    fn the_network_talks_over_https_with_a_certificate_that_lasts() {
        let cache = cache();
        // Loopback stands in for the Wi-Fi address.
        let network = Ipv4Addr::LOCALHOST;
        let (door, links) = Door::bind(0, &cache, Some(network), None, None).unwrap();
        let link = links.network.unwrap();
        assert!(link.starts_with("https://127.0.0.1:"), "{link}");

        // A client that trusts only this certificate checks its address, dates and purpose.
        let stored = cache.join("serve/127.0.0.1/certificate.der");
        let certificate = fs::read(&stored).unwrap();
        let mut trusted = rustls::RootCertStore::empty();
        trusted.add(certificate.clone().into()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(trusted)
            .with_no_client_auth();
        let session =
            rustls::ClientConnection::new(Arc::new(config), IpAddr::from(network).into()).unwrap();
        let stream = StreamOwned::new(session, TcpStream::connect(address(&link)).unwrap());
        let mut page = talk(&link, key(&link), stream).unwrap();
        let (mut remote, captured) = door
            .wait(&AtomicBool::new(false), || Ok(()))
            .unwrap()
            .unwrap()
            .start();
        remote.enqueue(&[1, 2]).unwrap();
        assert_eq!(page.read().unwrap().into_data().as_ref(), [1, 0, 2, 0]);
        page.send(tungstenite::Message::Binary(vec![0; 640].into()))
            .unwrap();
        let heard = captured.recv_timeout(Duration::from_secs(5));
        assert!(matches!(heard, Ok(CaptureFrame::Samples(frame)) if frame.samples == [0; 320]));

        // This computer still reaches the same door without encryption.
        assert_eq!(key(&links.here), key(&link));
        assert!(request(&links.here, "GET / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200 "));

        // A later start presents the same certificate and key, so a browser's trust and a
        // saved link carry over.
        let (_, later) = Door::bind(0, &cache, Some(network), None, None).unwrap();
        assert_eq!(fs::read(&stored).unwrap(), certificate);
        assert_eq!(key(&later.here), key(&link));
        for secret in ["serve/127.0.0.1/key.der", "serve/key"] {
            let file = fs::metadata(cache.join(secret)).unwrap();
            let mode = std::os::unix::fs::PermissionsExt::mode(&file.permissions());
            assert_eq!(mode & 0o077, 0, "Only the owner may read {secret}");
        }
        fs::write(cache.join("serve/key"), "not a key").unwrap();
        assert!(Door::bind(0, &cache, Some(network), None, None).is_err());
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn starts_at_the_same_time_agree_on_the_key_and_the_certificate() {
        let cache = cache();
        let address = Ipv4Addr::LOCALHOST;
        let start = || -> Result<String> {
            identity(&cache, address, None)?;
            super::key(&cache)
        };
        let keys: Vec<_> = thread::scope(|scope| {
            let starts: Vec<_> = (0..8).map(|_| scope.spawn(start)).collect();
            starts
                .into_iter()
                .map(|start| start.join().unwrap().unwrap())
                .collect()
        });
        assert!(keys.iter().all(|key| *key == keys[0]), "{keys:?}");
        // Loading checks that the stored key belongs to the stored certificate.
        identity(&cache, address, None).unwrap();
        fs::remove_dir_all(cache).unwrap();
    }

    /// A stand-in for the names service: `authority` certifies what each request asks for,
    /// once the request is checked the way the service checks it. Counts the requests.
    fn names_service(authority: rcgen::Issuer<'static, rcgen::KeyPair>) -> (String, Receiver<()>) {
        use rcgen::PublicKeyData;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (asked, requests) = mpsc::channel();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = Vec::new();
                let body = loop {
                    let mut chunk = [0; 4096];
                    let read = stream.read(&mut chunk).unwrap();
                    request.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&request).to_lowercase();
                    let Some(head) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let length = text.split("content-length: ").nth(1).unwrap();
                    let length: usize = length.split("\r\n").next().unwrap().parse().unwrap();
                    if request.len() >= head + 4 + length {
                        break request[head + 4..].to_vec();
                    }
                };
                asked.send(()).unwrap();
                let csr = rcgen::CertificateSigningRequestParams::from_der(&body.into()).unwrap();
                let id = names::id(&csr.public_key.subject_public_key_info());
                let wildcard = format!("*.{id}.{}", names::ZONE).try_into().unwrap();
                assert_eq!(
                    csr.params.subject_alt_names,
                    [rcgen::SanType::DnsName(wildcard)]
                );
                let certificate = csr.signed_by(&authority).unwrap();
                let now = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap();
                let answer = serde_json::json!({
                    "certificate": certificate.pem(),
                    "renew": now.as_secs() + 60 * DAY.as_secs(),
                    "expires": now.as_secs() + 90 * DAY.as_secs(),
                })
                .to_string();
                let length = answer.len();
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n{answer}");
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}/v1/certificate"), requests)
    }

    /// A TLS client that trusts only `root` and asks for `name`.
    fn client(root: &[u8], name: rustls::pki_types::ServerName<'static>, port: u16) -> impl Stream {
        let mut trusted = rustls::RootCertStore::empty();
        trusted.add(root.to_vec().into()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(trusted)
            .with_no_client_auth();
        let session = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        StreamOwned::new(session, TcpStream::connect(("127.0.0.1", port)).unwrap())
    }

    #[test]
    fn phones_reach_the_page_by_a_name_browsers_trust() {
        let cache = cache();
        let network = Ipv4Addr::LOCALHOST;
        let authority_key = rcgen::KeyPair::generate().unwrap();
        let mut authority = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        authority.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let root = authority
            .self_signed(&authority_key)
            .unwrap()
            .der()
            .to_vec();
        let (service, requests) = names_service(rcgen::Issuer::new(authority, authority_key));

        let (door, links) = Door::bind(0, &cache, Some(network), None, Some(&service)).unwrap();
        assert!(requests.try_recv().is_ok());
        let link = links.network.unwrap();
        let name = address(&link).split(':').next().unwrap().to_owned();
        let id = name.split('.').nth(1).unwrap();
        assert_eq!(name, format!("127-0-0-1.{id}.{}", names::ZONE));
        let port: u16 = address(&link).split(':').nth(1).unwrap().parse().unwrap();
        // Where the router will not point names at local addresses, the address still works.
        let fallback = links.fallback.unwrap();
        assert_eq!(
            fallback,
            format!("https://127.0.0.1:{port}/#{}", key(&link))
        );

        // A browser that asks for the name gets the certificate for it, and talks.
        let stream = client(&root, name.clone().try_into().unwrap(), port);
        let mut page = talk(&link, key(&link), stream).unwrap();
        let mut remote = wait(&door);
        remote.enqueue(&[1, 2]).unwrap();
        assert_eq!(page.read().unwrap().into_data().as_ref(), [1, 0, 2, 0]);
        drop((page, remote));
        // One that asks for the address gets Boubou's own certificate for it.
        let own = fs::read(cache.join("serve/127.0.0.1/certificate.der")).unwrap();
        let mut stream = client(&own, IpAddr::from(network).into(), port);
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut page = String::new();
        let _ = stream.read_to_string(&mut page);
        assert!(page.starts_with("HTTP/1.1 200 "), "{page}");

        // A later start uses the kept certificate without asking again, also when the service
        // is gone.
        let gone = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let gone = format!("http://{gone}/v1/certificate");
        let (_, later) = Door::bind(0, &cache, Some(network), None, Some(&gone)).unwrap();
        assert!(requests.try_recv().is_err());
        assert!(later.network.unwrap().contains(&name));
        let kept = fs::metadata(cache.join("serve/name/key.der")).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&kept.permissions()) & 0o077,
            0
        );

        // Without a certificate, and the service gone, phones are sent to the address.
        fs::remove_file(cache.join("serve/name/certificate.json")).unwrap();
        let (_, alone) = Door::bind(0, &cache, Some(network), None, Some(&gone)).unwrap();
        assert!(alone.network.unwrap().starts_with("https://127.0.0.1:"));
        assert!(alone.fallback.is_none());
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn robots_and_programs_are_carried_to_the_realtime_api() {
        let cache = cache();
        let (door, links) = Door::bind(0, &cache, None, Some(model_server()), None).unwrap();
        fs::remove_dir_all(cache).unwrap();
        let link = links.realtime.unwrap();
        let key = key(&links.here);
        assert_eq!(link.split("key=").nth(1), Some(key));
        // Without the key, or with another, the person is asked first.
        let wrong = || realtime(&link, "?key=wrong", Some("wrong"));
        let (refused, asked) = asking(&door, || Ok(Some(false)), wrong);
        assert_eq!(asked, IpAddr::from(Ipv4Addr::LOCALHOST));
        assert_eq!(status(refused), 404);
        let not_again = || panic!("Asked again about a robot that was turned away");
        let (refused, _) = asking(&door, not_again, || realtime(&link, "", None));
        assert_eq!(status(refused), 404);
        // A model without a Realtime API has nothing to offer them.
        let (_other, other) = local();
        let other_key = format!("?key={}", self::key(&other));
        assert_eq!(status(realtime(&other, &other_key, None)), 404);

        let mut robot = realtime(&link, &format!("?model=any&key={key}"), None).unwrap();
        assert_eq!(robot.read().unwrap().to_text().unwrap(), "created");
        robot.send("hello".into()).unwrap();
        assert_eq!(robot.read().unwrap().to_text().unwrap(), "hello");
        assert_eq!(
            status(plain(&links.here, key)),
            409,
            "One visitor at a time"
        );
        robot.close(None).unwrap();
        drop(robot);

        // Once it has left, a program that sends the key as its API key takes the seat.
        let mut program = once_free(|| realtime(&link, "", Some(key)));
        assert_eq!(program.read().unwrap().to_text().unwrap(), "created");
        // Neither took the conversation's audio device.
        let stopped = AtomicBool::new(false);
        let idle = || {
            stopped.store(true, Ordering::SeqCst);
            Ok(())
        };
        assert!(door.wait(&stopped, idle).unwrap().is_none());
    }

    #[test]
    fn the_guide_sets_the_phone_apart_from_what_a_robot_types() {
        let links = Links {
            here: "http://127.0.0.1:2682/#key".into(),
            network: Some("https://name:2682/#key".into()),
            fallback: Some("https://192.168.1.20:2682/#key".into()),
            realtime: Some("wss://name:2682/v1/realtime?key=key".into()),
            bare: Some((Ipv4Addr::new(192, 168, 1, 20), 2682)),
        };
        // In a log: the same sections, without escape codes or a code to scan.
        let plain = links.guide(&Ink::default(), 80, &[]).join("\n");
        assert!(!plain.contains('\x1b'));
        let at = |title: &str| plain.find(&format!("\n  {title}\n")).expect(title);
        assert!(at("Phone") < at("Robot or app") && at("Robot or app") < at(progress::HERE));
        assert!(plain.contains("\n     Address   192.168.1.20\n     Port      2682\n"));

        // On a terminal the code sits under Phone, and the two values to type stand out.
        let ink = Ink {
            bold: "<b>",
            accent: "<accent>".into(),
            ..Ink::default()
        };
        let styled = links.guide(&ink, 80, &["<code>".into()]).join("\n");
        assert!(at("Phone") < styled.find("\n\n  <code>\n\n").unwrap());
        assert!(styled.contains("Address   <b><accent>192.168.1.20\n"));
        assert!(styled.contains("Port      <b><accent>2682\n"));

        // A model without an API for robots offers them nothing.
        let page = Links {
            realtime: None,
            bare: None,
            ..links
        };
        assert!(!page.guide(&ink, 80, &[]).concat().contains("Robot"));
    }

    #[test]
    fn the_code_is_small_and_made_of_whole_cells() {
        let link = "https://192-168-100-120.abcdefghijklmnop.lan.runboubou.com:26820/#00112233445566778899aabbccddeeff";
        let lines = code(tui::Paint(tui::Colors::Basic), link).unwrap();
        // The longest link a network gives still fits 37 modules, with two light ones around.
        assert_eq!(lines.len(), 21);
        let (light, dark) = ("\x1b[97m\x1b[107m▀", "\x1b[30m\x1b[40m▀");
        assert_eq!(lines[0], light.repeat(41));
        // The corner pattern starts dark over dark, after the margin.
        assert!(lines[1].starts_with(&format!("{light}{light}{dark}")));
        assert!(lines.iter().all(|line| line.matches('▀').count() == 41));
    }

    #[test]
    fn a_robot_without_the_key_talks_once_the_person_lets_it_in() {
        let cache = cache();
        let api = model_server();
        let (door, links) = Door::bind(0, &cache, None, Some(api), None).unwrap();
        let link = links.realtime.unwrap();
        let port = address(&link).split(':').nth(1).unwrap();
        assert_eq!(
            links.bare.unwrap(),
            (Ipv4Addr::LOCALHOST, port.parse().unwrap())
        );
        // Settings that take only an address and a port, with an API key of the robot's own.
        let robot = || realtime(&link, "", Some("DUMMY"));

        // Unanswered, it is asked about again when it returns.
        let (unanswered, _) = asking(&door, || Ok(None), robot);
        assert_eq!(status(unanswered), 404);

        // A robot that comes back while the person is asked waits for the same answer, and one
        // that stopped waiting does not take the seat from it.
        let (let_in, asked) = thread::scope(|scope| {
            let mut gave_up = TcpStream::connect(address(&link)).unwrap();
            let request = "GET /v1/realtime HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
            gave_up.write_all(request.as_bytes()).unwrap();
            let ask = door.asks.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(gave_up);
            let back = scope.spawn(robot);
            let asked_at = Instant::now();
            let mut questions = 0;
            let decide = || {
                questions += 1;
                thread::sleep(Duration::from_millis(20));
                Ok((asked_at.elapsed() > Duration::from_millis(300)).then_some(true))
            };
            let peer = ask.peer;
            door.settle(ask, decide).unwrap();
            assert!(questions > 1);
            (back.join().unwrap(), peer)
        });
        assert_eq!(asked, IpAddr::from(Ipv4Addr::LOCALHOST));
        let mut socket = let_in.unwrap();
        assert_eq!(socket.read().unwrap().to_text().unwrap(), "created");
        drop(socket);
        assert_eq!(
            fs::read_to_string(cache.join("serve/allowed")).unwrap(),
            "127.0.0.1\n"
        );

        // A later start lets it in without asking. The page still needs the key.
        let (_later, later) = Door::bind(0, &cache, None, Some(api), None).unwrap();
        let mut socket = realtime(&later.realtime.unwrap(), "", Some("DUMMY")).unwrap();
        assert_eq!(socket.read().unwrap().to_text().unwrap(), "created");
        assert_eq!(status(plain(&later.here, "")), 404);
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn stopping_boubou_answers_an_open_question_without_a_key() {
        let mut idle = || panic!("Kept waiting after Boubou stopped");
        assert_eq!(
            pressed(&AtomicBool::new(true), &mut idle).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn a_text_is_read_aloud_by_the_model_server_while_its_program_holds_the_seat() {
        // A stand-in for the model server's voice. Once let, it answers with what it was sent.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = listener.local_addr().unwrap();
        let (received, arrived) = mpsc::channel();
        let (finish, finished) = mpsc::channel();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut sent = Vec::new();
                while !sent.ends_with(b"}") {
                    let mut chunk = [0; 4096];
                    let read = stream.read(&mut chunk).unwrap();
                    sent.extend_from_slice(&chunk[..read]);
                }
                received.send(()).unwrap();
                // The program, or the test, may have left.
                let _ = finished.recv();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", sent.len());
                let _ = stream.write_all(&[head.as_bytes(), &sent].concat());
            }
        });
        let cache = cache();
        let (door, links) = Door::bind(0, &cache, None, Some(api), None).unwrap();
        fs::remove_dir_all(cache).unwrap();
        let (link, key) = (links.here.as_str(), key(&links.here));
        // More text than the door reads before it decides.
        let body = format!(r#"{{"input":"{}"}}"#, "word ".repeat(1000));
        let post = |key: &str, body: &str| {
            let length = body.len();
            format!(
                "POST /v1/audio/speech HTTP/1.1\r\nAuthorization: Bearer {key}\r\nContent-Length: {length}\r\n\r\n{body}"
            )
        };
        let ask = || {
            let mut program = TcpStream::connect(address(link)).unwrap();
            program.write_all(post(key, &body).as_bytes()).unwrap();
            let asked = arrived.recv_timeout(Duration::from_secs(5));
            asked.expect("The seat was taken, or the model server never asked");
            assert_eq!(status(plain(link, key)), 409, "The voice is busy");
            program
        };

        let (unanswered, _) = asking(&door, || Ok(None), || request(link, &post("wrong", "")));
        assert!(unanswered.starts_with("HTTP/1.1 404 "), "{unanswered}");
        let got = format!("GET /v1/audio/speech?key={key} HTTP/1.1\r\n\r\n");
        assert!(request(link, &got).starts_with("HTTP/1.1 404 "));
        // A model without a voice of its own to ask.
        let (_other, other) = local();
        let unvoiced = request(&other, &post(self::key(&other), ""));
        assert!(unvoiced.starts_with("HTTP/1.1 404 "), "{unvoiced}");

        let mut program = ask();
        finish.send(()).unwrap();
        let mut answer = String::new();
        program.read_to_string(&mut answer).unwrap();
        // The model server was asked to answer once, and its answer came back whole.
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.contains("POST /v1/audio/speech HTTP/1.1\r\nConnection: close\r\nAuth"));
        assert!(answer.ends_with(&body));

        // The answer freed the seat for the next program, which frees it by leaving early.
        drop(ask());
        once_free(|| plain(link, key));
    }

    #[test]
    fn the_network_reaches_the_page_only_encrypted_and_only_from_private_addresses() {
        let trusted = |peer: &str, encrypted| trusted(peer.parse().unwrap(), encrypted);
        assert_eq!(trusted("127.0.0.1", false), Some(true));
        for private in ["192.168.1.7", "10.0.0.9", "172.16.3.4"] {
            assert_eq!(trusted(private, true), Some(true), "{private}");
            // Enough for a robot with the key, not for the page.
            assert_eq!(trusted(private, false), Some(false), "{private}");
        }
        for elsewhere in ["8.8.8.8", "100.100.1.1", "198.18.0.1", "2001:db8::1"] {
            assert_eq!(trusted(elsewhere, true), None, "{elsewhere}");
        }
        if let Some(address) = network_address() {
            assert!(address.is_private());
        }
    }
}
