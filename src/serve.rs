//! `boubou serve`: a page in a browser is the conversation's microphone and speaker. This
//! computer opens it over loopback. Other devices on the local network open it over HTTPS,
//! which browsers require before they share a microphone, with a certificate Boubou issues to
//! itself. One page talks at a time, and only a page opened from a printed link can.

use crate::{
    audio::{CaptureFrame, Remote},
    progress, tui,
};
use anyhow::{Context, Result, ensure};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use std::{
    cell::Cell,
    fs::{self, File},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream},
    os::unix::fs::OpenOptionsExt,
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

/// Held by the browser that is talking. Dropping it lets the next one in.
struct Seat(Arc<AtomicBool>);

impl Drop for Seat {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Where the page is. Each address ends in the key, as a fragment, which browsers never put
/// in a request.
struct Links {
    here: String,
    network: Option<String>,
}

/// Listens for browsers and admits one at a time.
pub struct Door {
    arrivals: Receiver<(Socket, Seat)>,
    returning: Cell<bool>,
}

impl Door {
    /// Starts serving the page and tells the person where to open it. `cache` keeps the
    /// certificate between starts.
    pub fn open(port: u16, cache: &Path) -> Result<Self> {
        let (door, links) = Self::bind(port, cache, network_address())?;
        let note = "Ready. Open the page in a browser.";
        if !progress::finish(note) {
            eprintln!("{note}");
        }
        eprintln!("\n  {:<16}  {}", progress::HERE, links.here);
        if let Some(link) = &links.network {
            eprintln!("  {:<16}  {link}", "On your network");
            if tui::Paint::detect().colored() {
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
    fn bind(port: u16, cache: &Path, network: Option<Ipv4Addr>) -> Result<(Self, Links)> {
        let identity = network
            .map(|address| identity(cache, address))
            .transpose()?;
        let interface = if network.is_some() {
            Ipv4Addr::UNSPECIFIED
        } else {
            Ipv4Addr::LOCALHOST
        };
        let listener = TcpListener::bind((interface, port)).with_context(|| {
            format!("Cannot listen on port {port}. Choose another with --port.")
        })?;
        let port = listener.local_addr()?.port();
        let mut key = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut key)?;
        let key: Arc<str> = crate::assets::hex(&key).into();
        let links = Links {
            // The literal address: `localhost` may resolve to IPv6 first, where nothing listens.
            here: format!("http://127.0.0.1:{port}/#{key}"),
            network: network.map(|address| format!("https://{address}:{port}/#{key}")),
        };
        let (admitted, arrivals) = mpsc::channel();
        let seated = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("boubou-serve".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // Every request in progress holds the key.
                    if Arc::strong_count(&key) > MAX_REQUESTS {
                        continue;
                    }
                    let (key, seated, admitted) = (key.clone(), seated.clone(), admitted.clone());
                    let identity = identity.clone();
                    // Browsers open connections they leave idle; each waits on its own thread.
                    thread::spawn(move || {
                        // A failed request costs only its own connection.
                        if let Ok(Some(arrival)) = accept(stream, identity, &key, &seated) {
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
    ) -> Result<Option<(Remote, Receiver<CaptureFrame>)>> {
        if self.returning.replace(true) {
            eprintln!("The browser disconnected. Open the link again to continue.");
        }
        while !stopped.load(Ordering::SeqCst) {
            if let Ok((socket, seat)) = self.arrivals.recv_timeout(Duration::from_millis(20)) {
                return Ok(Some(Remote::start(socket, seat)));
            }
            idle()?;
        }
        Ok(None)
    }
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
        fs::create_dir_all(&folder)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        options.open(&key)?.write_all(&pair.serialize_der())?;
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

/// Whether to answer a connection: any from this computer, and encrypted ones from private
/// addresses. Plain HTTP from the network would carry the key and the voice in the clear.
fn welcome(peer: IpAddr, encrypted: bool) -> bool {
    peer.is_loopback() || encrypted && matches!(peer, IpAddr::V4(peer) if peer.is_private())
}

/// Takes a new connection as HTTPS or plain HTTP and answers its request.
fn accept(
    stream: TcpStream,
    identity: Option<Arc<ServerConfig>>,
    key: &str,
    seated: &Arc<AtomicBool>,
) -> Result<Option<(Socket, Seat)>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut first = [0];
    ensure!(stream.peek(&mut first)? == 1, "The connection closed");
    // A TLS connection opens with a handshake record, type 22.
    let identity = identity.filter(|_| first[0] == 22);
    ensure!(
        welcome(stream.peer_addr()?.ip(), identity.is_some()),
        "Not from here"
    );
    let socket = stream.try_clone()?;
    match identity {
        Some(identity) => {
            let session = ServerConnection::new(identity)?;
            answer(StreamOwned::new(session, stream), &socket, key, seated)
        }
        None => answer(stream, &socket, key, seated),
    }
}

/// Serves the page, or gives the seat to a browser that holds the key. `socket` is the
/// connection underneath `stream`.
fn answer(
    mut stream: impl Stream + 'static,
    socket: &TcpStream,
    key: &str,
    seated: &Arc<AtomicBool>,
) -> Result<Option<(Socket, Seat)>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = Vec::new();
    let (target, accept, head) = loop {
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
            let accept = parsed
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case("Sec-WebSocket-Key"))
                .map(|header| derive_accept_key(header.value));
            let target = parsed.path.filter(|_| parsed.method == Some("GET"));
            break (target.unwrap_or_default().to_owned(), accept, head);
        }
    };
    let (status, body) = match (target.as_str(), accept) {
        ("/", None) => ("200 OK", PAGE),
        (target, Some(accept))
            if target
                .strip_prefix("/talk?key=")
                .is_some_and(|given| same(given, key)) =>
        {
            if seated.swap(true, Ordering::SeqCst) {
                ("409 Conflict", "")
            } else {
                // Held from here on, so a handshake that fails frees the seat again.
                let seat = Seat(Arc::clone(seated));
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
                let socket = WebSocket::from_partially_read(
                    stream,
                    request.split_off(head),
                    Role::Server,
                    Some(config),
                );
                return Ok(Some((socket, seat)));
            }
        }
        _ => ("404 Not Found", ""),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(None)
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

    /// A door that only this computer can reach.
    fn local() -> (Door, String) {
        let (door, links) = Door::bind(0, Path::new("unused"), None).unwrap();
        assert!(links.network.is_none());
        (door, links.here)
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
        let url = format!("ws://{}/talk?key={key}", address(link));
        tungstenite::client(url, stream)
            .map(|(socket, _)| socket)
            .map_err(|error| match error {
                tungstenite::HandshakeError::Failure(error) => error,
                tungstenite::HandshakeError::Interrupted(_) => unreachable!("blocking socket"),
            })
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
        let cache = std::env::temp_dir().join(format!("boubou-serve-{}", std::process::id()));
        // Loopback stands in for the Wi-Fi address.
        let network = Ipv4Addr::LOCALHOST;
        let (door, links) = Door::bind(0, &cache, Some(network)).unwrap();
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
            .unwrap();
        remote.enqueue(&[1, 2]).unwrap();
        assert_eq!(page.read().unwrap().into_data().as_ref(), [1, 0, 2, 0]);
        page.send(tungstenite::Message::Binary(vec![0; 640].into()))
            .unwrap();
        let heard = captured.recv_timeout(Duration::from_secs(5));
        assert!(matches!(heard, Ok(CaptureFrame::Samples(frame)) if frame.samples == [0; 320]));

        // This computer still reaches the same door without encryption.
        assert_eq!(key(&links.here), key(&link));
        assert!(request(&links.here, "GET / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200 "));

        // A later start presents the same certificate, so a browser's trust carries over.
        Door::bind(0, &cache, Some(network)).unwrap();
        assert_eq!(fs::read(&stored).unwrap(), certificate);
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(stored.with_file_name("key.der"))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o077, 0, "Only the owner may read the key");
        fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn the_network_is_welcome_only_encrypted_and_only_from_private_addresses() {
        let welcome = |peer: &str, encrypted| welcome(peer.parse().unwrap(), encrypted);
        assert!(welcome("127.0.0.1", false) && welcome("127.0.0.1", true));
        for private in ["192.168.1.7", "10.0.0.9", "172.16.3.4"] {
            assert!(welcome(private, true), "{private}");
            assert!(!welcome(private, false), "{private} without encryption");
        }
        for elsewhere in ["8.8.8.8", "100.100.1.1", "198.18.0.1", "2001:db8::1"] {
            assert!(!welcome(elsewhere, true), "{elsewhere}");
        }
        if let Some(address) = network_address() {
            assert!(address.is_private());
        }
    }
}
