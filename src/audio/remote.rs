//! A browser as the audio device. It records and plays through its own echo canceller and
//! exchanges PCM16 with Boubou over a WebSocket: 20 ms frames at 16 kHz in, 24 kHz speech out.
//! Boubou marks the frames that hold the person's speech, as the native devices do.

use super::{CaptureFrame, CapturedAudio, Device, Gate, Playback, Speech};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{ErrorKind, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

/// 20 ms at 16 kHz.
const FRAME_BYTES: usize = 640;
/// Speech goes out in 100 ms messages, the resolution at which the browser reports playback.
const CHUNK: usize = 2_400;
/// A connected page streams its microphone and reports playback continuously.
const UNRESPONSIVE: Duration = Duration::from_secs(5);

/// Owns one browser connection; dropping it closes the connection.
pub struct Remote {
    /// Messages for the page: binary speech, or the playback epoch that replaces what it holds.
    outgoing: Sender<Message>,
    playback: Arc<Mutex<Playback>>,
    gone: Arc<AtomicBool>,
}

impl Remote {
    /// `socket` must time out its reads within a few milliseconds, so that queued speech is
    /// sent promptly. `held` is dropped when the connection ends.
    pub fn start<S: Read + Write + Send + 'static>(
        socket: WebSocket<S>,
        held: impl Send + 'static,
    ) -> (Self, Receiver<CaptureFrame>) {
        let (outgoing, queued) = mpsc::channel();
        let (frames, captured) = mpsc::sync_channel(super::CAPTURE_QUEUE);
        let playback = Arc::new(Mutex::new(Playback::default()));
        let gone = Arc::new(AtomicBool::new(false));
        let (progress, ended) = (Arc::clone(&playback), Arc::clone(&gone));
        thread::spawn(move || {
            let _held = held;
            if let Err(reason) = exchange(socket, &queued, &frames, &progress) {
                crate::trace("remote.left", format_args!("{reason:#}"));
            }
            // Set before the channels close, so the conversation always finds the cause.
            ended.store(true, Ordering::SeqCst);
        });
        let remote = Self {
            outgoing,
            playback,
            gone,
        };
        (remote, captured)
    }

    /// True once the browser is gone, whether it closed the page or stopped responding.
    pub fn left(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

impl Device for Remote {
    fn enqueue(&mut self, pcm: &[i16]) -> Result<()> {
        self.playback.lock().expect("audio playback lock").pending += pcm.len();
        for chunk in pcm.chunks(CHUNK) {
            let bytes: Vec<u8> = chunk.iter().copied().flat_map(i16::to_le_bytes).collect();
            // A browser that left is reported by `check_error`.
            let _ = self.outgoing.send(Message::Binary(bytes.into()));
        }
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        let epoch = self.playback.lock().expect("audio playback lock").clear();
        let clear = json!({"clear": epoch}).to_string();
        let _ = self.outgoing.send(Message::Text(clear.into()));
        Ok(())
    }

    fn pending_samples(&self) -> usize {
        self.playback.lock().expect("audio playback lock").pending
    }

    fn check_error(&self) -> Result<()> {
        ensure!(!self.left(), "The browser disconnected.");
        Ok(())
    }
}

/// Sends queued speech and reads microphone frames and playback reports until either side leaves.
fn exchange<S: Read + Write>(
    mut socket: WebSocket<S>,
    outgoing: &Receiver<Message>,
    frames: &SyncSender<CaptureFrame>,
    playback: &Mutex<Playback>,
) -> Result<()> {
    let (mut speech, mut gate) = (Speech::default(), Gate::default());
    let mut heard = Instant::now();
    let mut played = Instant::now();
    loop {
        loop {
            match outgoing.try_recv() {
                Ok(message) => socket.send(message)?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    let _ = socket.flush();
                    return Ok(());
                }
            }
        }
        let playing = playback.lock().expect("audio playback lock").pending > 0;
        if !playing {
            played = Instant::now();
        }
        ensure!(
            heard.elapsed() < UNRESPONSIVE && played.elapsed() < UNRESPONSIVE,
            "the browser stopped responding"
        );
        match socket.read() {
            Ok(Message::Binary(bytes)) => {
                ensure!(
                    bytes.len() == FRAME_BYTES,
                    "the browser sent a {}-byte audio frame",
                    bytes.len()
                );
                heard = Instant::now();
                let samples: Vec<i16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .copied()
                    .map(i16::from_le_bytes)
                    .collect();
                // The frame spans two of the gate's 10 ms chunks of what the browser plays.
                gate.render(playing);
                gate.render(playing);
                let frame = CapturedAudio {
                    near_end_speech: gate.near_end(speech.push(&samples)),
                    samples,
                    captured_at: heard,
                };
                match frames.try_send(CaptureFrame::Samples(frame)) {
                    Ok(()) => {}
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                    Err(TrySendError::Full(_)) => {
                        bail!("Boubou fell more than five seconds behind the browser's microphone")
                    }
                }
            }
            Ok(Message::Text(text)) => {
                let report: Value = serde_json::from_str(&text)?;
                let (Some(epoch), Some(count)) =
                    (report["epoch"].as_u64(), report["played"].as_u64())
                else {
                    bail!("the browser sent an unknown message");
                };
                played = Instant::now();
                playback
                    .lock()
                    .expect("audio playback lock")
                    .acknowledge(epoch, count as usize);
            }
            Ok(Message::Close(_)) => {
                // Sends the closing reply that reading queued, so the page closes cleanly.
                let _ = socket.flush();
                bail!("the browser closed the page")
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    type Socket = WebSocket<TcpStream>;

    /// A started device and the browser's end of its connection.
    fn connect() -> (Remote, Receiver<CaptureFrame>, Socket) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let browser = thread::spawn(move || {
            let stream = TcpStream::connect(address).unwrap();
            tungstenite::client(format!("ws://{address}/"), stream)
                .unwrap()
                .0
        });
        let stream = listener.accept().unwrap().0;
        let socket = tungstenite::accept(stream).unwrap();
        let timeout = Some(Duration::from_millis(5));
        socket.get_ref().set_read_timeout(timeout).unwrap();
        let (remote, captured) = Remote::start(socket, ());
        (remote, captured, browser.join().unwrap())
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "Timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn report(browser: &mut Socket, epoch: u64, played: usize) {
        let report = json!({"epoch": epoch, "played": played}).to_string();
        browser.send(Message::Text(report.into())).unwrap();
    }

    #[test]
    fn microphone_frames_arrive_whole_and_silence_is_not_speech() {
        let (_remote, captured, mut browser) = connect();
        let frame: Vec<u8> = (0..320_i16).flat_map(i16::to_le_bytes).collect();
        browser.send(Message::Binary(frame.into())).unwrap();
        browser
            .send(Message::Binary(vec![0; FRAME_BYTES].into()))
            .unwrap();
        let mut frames = Vec::new();
        until("two frames", || {
            frames.extend(captured.try_iter().map(|frame| match frame {
                CaptureFrame::Samples(frame) => frame,
                CaptureFrame::Error(error) => panic!("{error}"),
            }));
            frames.len() == 2
        });
        assert_eq!(frames[0].samples, (0..320).collect::<Vec<i16>>());
        assert_eq!(frames[1].samples, vec![0; 320]);
        assert!(!frames[1].near_end_speech);
        assert!(frames[1].captured_at.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn frames_wait_through_a_pause_in_the_conversation() {
        let (remote, captured, mut browser) = connect();
        // Two seconds of microphone arrive while the conversation takes none of it.
        for index in 0..100_i16 {
            let frame: Vec<u8> = [index; 320]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect();
            browser.send(Message::Binary(frame.into())).unwrap();
        }
        thread::sleep(Duration::from_millis(300));
        let mut firsts = Vec::new();
        until("the waiting frames", || {
            firsts.extend(captured.try_iter().map(|frame| match frame {
                CaptureFrame::Samples(frame) => frame.samples[0],
                CaptureFrame::Error(error) => panic!("{error}"),
            }));
            firsts.len() == 100
        });
        assert_eq!(firsts, (0..100).collect::<Vec<i16>>());
        remote.check_error().unwrap();
    }

    #[test]
    fn speech_plays_in_order_and_only_current_playback_is_acknowledged() {
        let (mut remote, _captured, mut browser) = connect();
        let speech: Vec<i16> = (0..3_000).collect();
        remote.enqueue(&speech).unwrap();
        assert_eq!(remote.pending_samples(), 3_000);
        let mut received = Vec::new();
        for expected in [CHUNK, 3_000 - CHUNK] {
            let Message::Binary(bytes) = browser.read().unwrap() else {
                panic!("Speech must arrive as binary PCM");
            };
            assert_eq!(bytes.len(), expected * 2);
            received.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
        }
        assert_eq!(received, speech);
        report(&mut browser, 0, CHUNK);
        until("the first report", || {
            remote.pending_samples() == 3_000 - CHUNK
        });

        // Interrupting drops what is queued; a late report for it must not retire new speech.
        remote.clear().unwrap();
        assert_eq!(remote.pending_samples(), 0);
        assert_eq!(
            browser.read().unwrap(),
            Message::Text(r#"{"clear":1}"#.into())
        );
        remote.enqueue(&[7; 480]).unwrap();
        report(&mut browser, 0, 600);
        report(&mut browser, 1, 480);
        until("the current report", || remote.pending_samples() == 0);
        remote.check_error().unwrap();
    }

    #[test]
    fn a_closed_page_or_a_malformed_frame_ends_the_device() {
        let (mut remote, _captured, mut browser) = connect();
        browser.close(None).unwrap();
        until("the page to close", || remote.left());
        // Speech for a browser that left is dropped; `check_error` reports the departure.
        remote.enqueue(&[0; 10]).unwrap();
        assert!(remote.check_error().is_err());

        let (remote, captured, mut browser) = connect();
        browser.send(Message::Binary(vec![0; 100].into())).unwrap();
        until("the frame to be rejected", || remote.left());
        assert!(captured.try_recv().is_err());
    }

    #[test]
    fn a_page_that_stops_streaming_or_never_plays_is_released() {
        let (silent, _captured, _browser) = connect();
        // This page keeps its microphone streaming but never reports the speech as played.
        let (mut stuck, captured, mut browser) = connect();
        stuck.enqueue(&[1; 480]).unwrap();
        let began = Instant::now();
        while !(silent.left() && stuck.left()) {
            assert!(
                began.elapsed() < UNRESPONSIVE + Duration::from_secs(3),
                "An unresponsive page kept its connection"
            );
            let _ = browser.send(Message::Binary(vec![0; FRAME_BYTES].into()));
            captured.try_iter().for_each(drop);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(began.elapsed() >= UNRESPONSIVE - Duration::from_millis(100));
    }

    #[test]
    fn dropping_the_device_closes_the_page() {
        let (remote, _captured, mut browser) = connect();
        drop(remote);
        assert!(matches!(browser.read(), Ok(Message::Close(_)) | Err(_)));
    }
}
