//! `boubou serve`: a page in a browser is the conversation's microphone and speaker. This
//! computer opens it over loopback. Other devices on the local network open it over HTTPS,
//! which browsers require before they share a microphone, with a certificate Boubou issues to
//! itself. Robots and programs that speak the OpenAI Realtime API connect to the same door
//! and are carried to the model server's own implementation of it. One visitor talks at a
//! time, and only one that holds the key from a printed link can.

use crate::{
    audio::{CaptureFrame, Remote},
    progress, tui,
};
use anyhow::{Context, Result, ensure};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use std::{
    cell::Cell,
    fs::{self, File},
    io::{self, ErrorKind, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use tungstenite::{
    WebSocket,
    handshake::derive_accept_key,
    protocol::{Role, WebSocketConfig},
};

const PAGE: &str = include_str!("serve.html");
/// Far above a 20 ms frame or a playback report.
const MAX_MESSAGE: usize = 1 << 16;
/// Requests answered at once; a flood beyond this is turned away.
const MAX_REQUESTS: usize = 32;
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// Apple's browsers reject server certificates valid for more than 398 days. The first of
/// these days is yesterday, for devices whose clocks run behind.
const CERTIFICATE_DAYS: u32 = 397;

/// A connection from this computer, or an encrypted one from the network.
trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

type Socket = WebSocket<Box<dyn Stream>>;

/// What answering a request takes.
struct Desk {
    key: String,
    /// Set while someone is talking.
    seated: AtomicBool,
    /// Proves the network address to browsers.
    identity: Option<Arc<ServerConfig>>,
    /// The model server's own Realtime API, where it has one.
    realtime: Option<SocketAddr>,
}

/// Held by whoever is talking. Dropping it lets the next visitor in.
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
}

/// Where to connect. The page's addresses end in the key as a fragment, which browsers never
/// put in a request.
struct Links {
    here: String,
    network: Option<String>,
    /// For robots and programs, which cannot check Boubou's certificate and so connect
    /// without encryption.
    realtime: Option<String>,
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
}

impl Door {
    /// Starts serving and tells the person where to connect. `cache` keeps the certificate
    /// between starts. `realtime` is the model server's own Realtime API, where it has one.
    pub fn open(options: &Options, cache: &Path, realtime: Option<SocketAddr>) -> Result<Self> {
        let network = network_address().filter(|_| !options.local);
        let (door, links) = Self::bind(options.port, cache, network, realtime)?;
        let note = "Ready. Open the page in a browser.";
        if !progress::finish(note) {
            eprintln!("{note}");
        }
        eprintln!("\n  {:<16}  {}", progress::HERE, links.here);
        if let Some(link) = &links.realtime {
            eprintln!("  {:<16}  {link}", "Robots and apps");
        }
        if let Some(link) = &links.network {
            eprintln!("  {:<16}  {link}", "On your network");
            if tui::interactive(&io::stderr()) && tui::Paint::detect().colored() {
                let code = qrcode::QrCode::new(link)?
                    .render::<qrcode::render::unicode::Dense1x2>()
                    .build();
                // Black on white whatever the terminal's colors, as cameras expect.
                for line in code.lines() {
                    eprintln!("  \x1b[30;107m{line}\x1b[0m");
                }
            }
            eprintln!(
                "  A phone's browser warns once that it does not know Boubou's certificate. Continue to the page."
            );
        }
        eprintln!();
        Ok(door)
    }

    /// Listens on `network` as well as loopback when this computer has such an address.
    fn bind(
        port: u16,
        cache: &Path,
        network: Option<Ipv4Addr>,
        realtime: Option<SocketAddr>,
    ) -> Result<(Self, Links)> {
        let identity = network
            .map(|address| identity(cache, address))
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
        let links = Links {
            here: format!("http://127.0.0.1:{port}/#{key}"),
            network: network.map(|address| format!("https://{address}:{port}/#{key}")),
            realtime: realtime.map(|_| format!("ws://{reachable}:{port}/v1/realtime?key={key}")),
        };
        let desk = Arc::new(Desk {
            key,
            seated: AtomicBool::new(false),
            identity,
            realtime,
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
        };
        Ok((door, links))
    }

    /// Waits for the next browser, calling `idle` meanwhile so the model's session stays alive.
    /// Returns `None` once `stopped` is set.
    pub fn wait(
        &self,
        stopped: &AtomicBool,
        mut idle: impl FnMut() -> Result<()>,
    ) -> Result<Option<Guest>> {
        if self.returning.replace(true) {
            eprintln!("The browser disconnected. Open the link again to continue.");
        }
        while !stopped.load(Ordering::SeqCst) {
            if let Ok(guest) = self.arrivals.recv_timeout(Duration::from_millis(20)) {
                return Ok(Some(guest));
            }
            idle()?;
        }
        Ok(None)
    }
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

/// The certificate and key that prove this address to a browser. They are made once and
/// kept, so a browser that was told to trust the certificate keeps trusting it.
fn identity(cache: &Path, address: Ipv4Addr) -> Result<Arc<ServerConfig>> {
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
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![fs::read(certificate)?.into()], key.into())?;
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
    let trusted = trusted(stream.peer_addr()?.ip(), identity.is_some()).context("Not from here")?;
    let socket = stream.try_clone()?;
    match identity {
        Some(identity) => {
            let session = ServerConnection::new(identity)?;
            answer(StreamOwned::new(session, stream), &socket, trusted, desk)
        }
        None => answer(stream, &socket, trusted, desk),
    }
}

/// Serves the page, gives the seat to a browser that holds the key, or carries a robot or
/// program that holds it to the Realtime API. `socket` is the connection underneath `stream`.
fn answer(
    mut stream: impl Stream + 'static,
    socket: &TcpStream,
    trusted: bool,
    desk: &Arc<Desk>,
) -> Result<Option<Guest>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = Vec::new();
    let (target, accept, bearer, head) = loop {
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
            let target = parsed.path.filter(|_| parsed.method == Some("GET"));
            break (target.unwrap_or_default().to_owned(), accept, bearer, head);
        }
    };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let keyed = query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("key="))
        .chain(bearer.as_deref())
        .any(|given| same(given, &desk.key));
    let mut respond = |status: &str, body: &str| -> Result<Option<Guest>> {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        stream.flush()?;
        Ok(None)
    };
    let (accept, realtime) = match (path, accept, desk.realtime) {
        ("/", None, _) if trusted => return respond("200 OK", PAGE),
        ("/talk", Some(accept), _) if trusted && keyed => (accept, None),
        ("/v1/realtime", Some(accept), Some(realtime)) if keyed => (accept, Some(realtime)),
        _ => return respond("404 Not Found", ""),
    };
    if desk.seated.swap(true, Ordering::SeqCst) {
        return respond("409 Conflict", "");
    }
    // Held from here on, so a handshake that fails frees the seat again.
    let seat = Seat(Arc::clone(desk));
    if let Some(realtime) = realtime {
        return relay(stream, socket, realtime, &request).map(|()| None);
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

/// Carries a connection to the model server's Realtime API byte for byte, from the request
/// already read, until either side closes. The model server answers the handshake itself.
fn relay(
    mut visitor: impl Stream,
    socket: &TcpStream,
    realtime: SocketAddr,
    request: &[u8],
) -> Result<()> {
    let mut server = TcpStream::connect_timeout(&realtime, Duration::from_secs(2))?;
    server.write_all(request)?;
    // Each side is read in turn, for a few milliseconds at a time.
    for side in [socket, &server] {
        side.set_nodelay(true)?;
        side.set_read_timeout(Some(Duration::from_millis(5)))?;
        side.set_write_timeout(Some(Duration::from_secs(5)))?;
    }
    eprintln!("A robot or program connected.");
    let mut buffer = [0; 16 * 1024];
    let mut carry = || -> io::Result<()> {
        while pass(&mut visitor, &mut server, &mut buffer)?
            && pass(&mut server, &mut visitor, &mut buffer)?
        {}
        Ok(())
    };
    let ended = carry();
    eprintln!("The robot or program disconnected.");
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
        let (door, links) = Door::bind(0, &cache, None, None).unwrap();
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let _second = loop {
            match plain(&link, key) {
                Ok(second) => break second,
                refused => assert_eq!(status(refused), 409),
            }
            assert!(Instant::now() < deadline, "The seat was never freed");
            thread::sleep(Duration::from_millis(2));
        };
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
        let (door, links) = Door::bind(0, &cache, Some(network), None).unwrap();
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
        let (_, later) = Door::bind(0, &cache, Some(network), None).unwrap();
        assert_eq!(fs::read(&stored).unwrap(), certificate);
        assert_eq!(key(&later.here), key(&link));
        for secret in ["serve/127.0.0.1/key.der", "serve/key"] {
            let file = fs::metadata(cache.join(secret)).unwrap();
            let mode = std::os::unix::fs::PermissionsExt::mode(&file.permissions());
            assert_eq!(mode & 0o077, 0, "Only the owner may read {secret}");
        }
        fs::write(cache.join("serve/key"), "not a key").unwrap();
        assert!(Door::bind(0, &cache, Some(network), None).is_err());
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn starts_at_the_same_time_agree_on_the_key_and_the_certificate() {
        let cache = cache();
        let address = Ipv4Addr::LOCALHOST;
        let start = || -> Result<String> {
            identity(&cache, address)?;
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
        identity(&cache, address).unwrap();
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn robots_and_programs_are_carried_to_the_realtime_api() {
        let cache = cache();
        let (door, links) = Door::bind(0, &cache, None, Some(model_server())).unwrap();
        fs::remove_dir_all(cache).unwrap();
        let link = links.realtime.unwrap();
        let key = key(&links.here);
        assert_eq!(link.split("key=").nth(1), Some(key));
        assert_eq!(status(realtime(&link, "", None)), 404);
        assert_eq!(status(realtime(&link, "?key=wrong", Some("wrong"))), 404);
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut program = loop {
            match realtime(&link, "", Some(key)) {
                Ok(program) => break program,
                refused => assert_eq!(status(refused), 409),
            }
            assert!(Instant::now() < deadline, "The seat was never freed");
            thread::sleep(Duration::from_millis(2));
        };
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
