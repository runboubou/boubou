//! `boubou serve`: a page in a browser on this computer is the conversation's microphone and
//! speaker. One page talks at a time, and only a page opened from the printed link can.

use crate::{
    audio::{CaptureFrame, Remote},
    progress,
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::Cell,
    fs::File,
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    WebSocket,
    handshake::derive_accept_key,
    protocol::{Role, WebSocketConfig},
};

const PAGE: &str = include_str!("serve.html");
/// Far above a 20 ms frame or a playback report.
const MAX_MESSAGE: usize = 1 << 16;

type Socket = WebSocket<TcpStream>;

/// Held by the browser that is talking. Dropping it lets the next one in.
struct Seat(Arc<AtomicBool>);

impl Drop for Seat {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Listens on loopback and admits one browser at a time.
pub struct Door {
    arrivals: Receiver<(Socket, Seat)>,
    returning: Cell<bool>,
}

impl Door {
    /// Starts serving the page and tells the person where to open it.
    pub fn open(port: u16) -> Result<Self> {
        let (door, link) = Self::bind(port)?;
        let note = format!(
            "Ready. Open this link in a browser {}:",
            progress::HERE.replacen('O', "o", 1)
        );
        if !progress::finish(&note) {
            eprintln!("{note}");
        }
        eprintln!("\n  {link}\n");
        Ok(door)
    }

    /// Returns the door and the page's address. The address ends in the key, as a fragment,
    /// which browsers never put in a request.
    fn bind(port: u16) -> Result<(Self, String)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| {
            format!("Cannot listen on port {port}. Choose another with --port.")
        })?;
        let mut key = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut key)?;
        let key: Arc<str> = crate::assets::hex(&key).into();
        // The literal address: `localhost` may resolve to IPv6 first, where nothing listens.
        let link = format!("http://{}/#{key}", listener.local_addr()?);
        let (admitted, arrivals) = mpsc::channel();
        let seated = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("boubou-serve".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (key, seated, admitted) = (key.clone(), seated.clone(), admitted.clone());
                    // Browsers open connections they leave idle; each waits on its own thread.
                    thread::spawn(move || {
                        // A failed request costs only its own connection.
                        if let Ok(Some(arrival)) = answer(stream, &key, &seated) {
                            let _ = admitted.send(arrival);
                        }
                    });
                }
            })?;
        let door = Self {
            arrivals,
            returning: Cell::new(false),
        };
        Ok((door, link))
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

/// Serves the page, or gives the seat to a browser that holds the key.
fn answer(
    mut stream: TcpStream,
    key: &str,
    seated: &Arc<AtomicBool>,
) -> Result<Option<(Socket, Seat)>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
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
                stream.set_nodelay(true)?;
                // The device sends queued speech between reads.
                stream.set_read_timeout(Some(Duration::from_millis(10)))?;
                let config = WebSocketConfig::default()
                    .max_message_size(Some(MAX_MESSAGE))
                    .max_frame_size(Some(MAX_MESSAGE));
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

    fn request(link: &str, text: &str) -> String {
        let mut stream = TcpStream::connect(address(link)).unwrap();
        // A refused request may be cut off while it is still being written.
        let _ = stream.write_all(text.as_bytes());
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn address(link: &str) -> &str {
        let address = link.strip_prefix("http://").unwrap();
        address.split('/').next().unwrap()
    }

    fn key(link: &str) -> &str {
        link.split('#').nth(1).unwrap()
    }

    fn talk(link: &str, key: &str) -> tungstenite::Result<Socket> {
        let stream = TcpStream::connect(address(link)).unwrap();
        let url = format!("ws://{}/talk?key={key}", address(link));
        tungstenite::client(url, stream)
            .map(|(socket, _)| socket)
            .map_err(|error| match error {
                tungstenite::HandshakeError::Failure(error) => error,
                tungstenite::HandshakeError::Interrupted(_) => unreachable!("blocking socket"),
            })
    }

    fn status(result: tungstenite::Result<Socket>) -> u16 {
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
        let (_door, link) = Door::bind(0).unwrap();
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
        let (door, link) = Door::bind(0).unwrap();
        let key = key(&link);
        assert_eq!(key.len(), 32);
        assert_ne!(key, self::key(&Door::bind(0).unwrap().1));
        for wrong in [
            "",
            "0",
            &key[1..],
            &format!("{key}0"),
            &key.replace(&key[..1], "g"),
        ] {
            assert_eq!(status(talk(&link, wrong)), 404, "{wrong:?}");
        }

        let mut first = talk(&link, key).unwrap();
        let mut remote = wait(&door);
        assert_eq!(status(talk(&link, key)), 409);
        remote.enqueue(&[1, 2, 3]).unwrap();
        assert_eq!(
            first.read().unwrap().into_data().as_ref(),
            [1, 0, 2, 0, 3, 0]
        );

        // The seat frees once the first browser's connection has ended.
        first.close(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let _second = loop {
            match talk(&link, key) {
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
        let (door, _link) = Door::bind(0).unwrap();
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
}
