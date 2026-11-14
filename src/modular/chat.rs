//! Conversations with the modular Python runtime. The upstream pipeline serves the Realtime API
//! on loopback and owns turn-taking. Boubou owns the microphone, speakers and echo cancellation.

use crate::voice::{Segment, Voice};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, ErrorKind, Read},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

/// Boubou captures 20 ms frames of echo-cancelled 16 kHz PCM16, the server's default input.
const FRAME: usize = 320;
/// One append per 100 ms bounds the message rate without noticeably delaying turns.
const APPEND_SAMPLES: usize = 1_600;
/// Replies arrive faster than real time. Feed the 30-second native queue a few seconds ahead.
const PLAYBACK_AHEAD: usize = 24_000 * 5;
const OUTPUT_LINES: usize = 40;

/// The runner logs conversation text, so its output is kept only in memory for failures.
pub(super) struct Server {
    address: SocketAddr,
    child: Child,
    output: Arc<Mutex<VecDeque<String>>>,
    readers: Vec<thread::JoinHandle<()>>,
}

impl Server {
    pub(super) fn start(
        mut command: Command,
        patience: Duration,
        stopped: &AtomicBool,
    ) -> Result<(Self, Session)> {
        let progress = crate::progress::Progress::new("Starting the local voice engine", None);
        let address = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?;
        let mut child = command
            .args(["--host", "127.0.0.1", "--port", &address.port().to_string()])
            .env("PYTHONUNBUFFERED", "1")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Cannot start the modular voice runner")?;
        let output = Arc::new(Mutex::new(VecDeque::new()));
        let pipes: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|pipe| Box::new(pipe) as _),
            child.stderr.take().map(|pipe| Box::new(pipe) as _),
        ];
        let readers = pipes
            .into_iter()
            .flatten()
            .map(|pipe| {
                let output = output.clone();
                thread::spawn(move || {
                    for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                        let mut lines = output.lock().expect("runner output lock");
                        if lines.len() == OUTPUT_LINES {
                            lines.pop_front();
                        }
                        lines.push_back(line);
                    }
                })
            })
            .collect();
        let mut server = Self {
            address,
            child,
            output,
            readers,
        };
        let deadline = Instant::now() + patience;
        progress.status(&format!(
            "Loading voice models{} (up to {}s)",
            if patience > Duration::from_secs(300) {
                "; custom models may download"
            } else {
                " locally"
            },
            patience.as_secs()
        ));
        loop {
            crate::check_cancel(stopped)?;
            server.check()?;
            // The port opens once the models are loaded.
            if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
                progress.status("Models loaded; connecting the voice session");
                let session = Session::connect(stream, address, stopped)?;
                return Ok((server, session));
            }
            ensure!(
                Instant::now() < deadline,
                "The modular voice runner did not become ready within {} seconds.{}",
                patience.as_secs(),
                server.output()
            );
            thread::sleep(Duration::from_millis(200));
        }
    }

    /// A new conversation. The runner holds one at a time and takes a moment to let go of the
    /// last one, so this keeps trying for a while.
    fn session(&mut self, stopped: &AtomicBool) -> Result<Session> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.check()?;
            let session = TcpStream::connect_timeout(&self.address, Duration::from_secs(1))
                .map_err(anyhow::Error::from)
                .and_then(|stream| Session::connect(stream, self.address, stopped));
            match session {
                Err(error) if !error.is::<crate::Cancelled>() && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(200));
                }
                session => return session,
            }
        }
    }

    pub(super) fn check(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            // Collect its final lines, unless a descendant still holds the pipes.
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.readers.iter().any(|reader| !reader.is_finished())
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
            bail!(
                "The modular voice runner stopped ({status}).{}",
                self.output()
            );
        }
        Ok(())
    }

    fn output(&self) -> String {
        let lines = self.output.lock().expect("runner output lock");
        if lines.is_empty() {
            return String::new();
        }
        let lines: Vec<_> = lines.iter().map(String::as_str).collect();
        format!("\nLast runner output:\n{}", lines.join("\n"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Stop the runner's whole process group, including descendants left behind
        // if the Python process failed at startup.
        let group = -(self.child.id() as libc::pid_t);
        unsafe { libc::kill(group, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                unsafe { libc::kill(group, libc::SIGKILL) };
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Event {
    Heard(String),
    Text(String),
    Audio(Vec<i16>),
    /// The server heard speech and cancels its reply; drop whatever is still queued.
    SpeechStarted,
    Done,
}

pub(super) struct Session {
    socket: WebSocket<TcpStream>,
    outgoing: Vec<i16>,
    response: Option<String>,
    /// A reply the user talked over. Its late audio and text are discarded.
    muted: Option<String>,
}

impl Session {
    fn connect(stream: TcpStream, address: SocketAddr, stopped: &AtomicBool) -> Result<Self> {
        stream.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut handshake = tungstenite::client(format!("ws://{address}/v1/realtime"), stream);
        let (socket, _) = loop {
            crate::check_cancel(stopped)?;
            ensure!(
                Instant::now() < deadline,
                "Modular runtime WebSocket handshake timed out"
            );
            match handshake {
                Ok(connected) => break connected,
                Err(tungstenite::HandshakeError::Failure(error)) => {
                    return Err(error)
                        .context("The modular voice runner rejected the voice session");
                }
                Err(tungstenite::HandshakeError::Interrupted(pending)) => {
                    thread::sleep(Duration::from_millis(10));
                    handshake = pending.handshake();
                }
            }
        };
        socket.get_ref().set_nonblocking(false)?;
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(10)))?;
        socket
            .get_ref()
            .set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut session = Self {
            socket,
            outgoing: Vec::new(),
            response: None,
            muted: None,
        };
        session.wait_for("session.created", stopped)?;
        session.send_event(
            json!({"type":"session.update","session":{"type":"realtime","audio":{
                "input":{"turn_detection":{"type":"server_vad","interrupt_response":true}},
                "output":{"format":{"type":"audio/pcm","rate":24000}}
            }}}),
        )?;
        let updated = session.wait_for("session.updated", stopped)?;
        ensure!(
            updated["session"]["audio"]["output"]["format"]["rate"] == 24000,
            "The modular voice runner did not accept 24 kHz speech output"
        );
        Ok(session)
    }

    pub(super) fn send(&mut self, pcm: &[i16]) -> Result<()> {
        self.outgoing.extend_from_slice(pcm);
        while self.outgoing.len() >= APPEND_SAMPLES {
            let bytes: Vec<u8> = self
                .outgoing
                .drain(..APPEND_SAMPLES)
                .flat_map(i16::to_le_bytes)
                .collect();
            self.send_event(
                json!({"type":"input_audio_buffer.append","audio":STANDARD.encode(bytes)}),
            )?;
        }
        Ok(())
    }

    /// Cancels the reply being generated, if any. Playback is cleared by the caller.
    pub(super) fn interrupt(&mut self) -> Result<bool> {
        let Some(id) = self.response.clone() else {
            return Ok(false);
        };
        if self.muted.as_ref() == Some(&id) {
            return Ok(false);
        }
        self.muted = Some(id);
        self.send_event(json!({"type":"response.cancel"}))?;
        Ok(true)
    }

    pub(super) fn poll(&mut self) -> Result<Option<Event>> {
        while let Some(event) = self.read()? {
            if let Some(event) = self.process(&event)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    fn process(&mut self, event: &Value) -> Result<Option<Event>> {
        let id = event["response_id"]
            .as_str()
            .or_else(|| event["response"]["id"].as_str());
        let muted = id.is_some() && id == self.muted.as_deref();
        Ok(match event["type"].as_str().unwrap_or_default() {
            "error" => bail!("Modular runtime: {}", event["error"]["message"]),
            "input_audio_buffer.speech_started" => {
                if self.response.is_some() {
                    self.muted = self.response.clone();
                }
                Some(Event::SpeechStarted)
            }
            "conversation.item.input_audio_transcription.completed" => event["transcript"]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| Event::Heard(text.to_owned())),
            "response.created" => {
                self.response = id.map(str::to_owned);
                None
            }
            "response.output_audio_transcript.delta" if !muted => event["delta"]
                .as_str()
                .map(|text| Event::Text(text.to_owned())),
            "response.output_audio.delta" if !muted => {
                let bytes =
                    STANDARD.decode(event["delta"].as_str().context("Invalid audio delta")?)?;
                ensure!(
                    bytes.len() % 2 == 0,
                    "The modular runtime returned an incomplete PCM16 sample"
                );
                let pcm = bytes.as_chunks::<2>().0.iter().copied();
                Some(Event::Audio(pcm.map(i16::from_le_bytes).collect()))
            }
            "response.done" => {
                let status = event["response"]["status"].as_str().unwrap_or_default();
                ensure!(
                    matches!(status, "completed" | "cancelled" | "incomplete"),
                    "The modular runtime could not answer (status: {status})"
                );
                if self.response.as_deref() == id {
                    self.response = None;
                }
                if muted {
                    self.muted = None;
                    None
                } else {
                    Some(Event::Done)
                }
            }
            _ => None,
        })
    }

    fn wait_for(&mut self, kind: &str, stopped: &AtomicBool) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            crate::check_cancel(stopped)?;
            ensure!(Instant::now() < deadline, "Timed out waiting for {kind}");
            if let Some(event) = self.read()? {
                if event["type"] == "error" {
                    bail!("Modular runtime: {}", event["error"]["message"]);
                }
                if event["type"] == kind {
                    return Ok(event);
                }
            }
        }
    }

    fn read(&mut self) -> Result<Option<Value>> {
        match self.socket.read() {
            Ok(Message::Text(text)) => Ok(Some(serde_json::from_str(&text)?)),
            Ok(Message::Ping(_)) => {
                self.socket.flush()?;
                Ok(None)
            }
            Ok(Message::Close(_)) => bail!("The modular voice runner closed the conversation"),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                Ok(None)
            }
            Err(e) => Err(e).context("Lost connection to the modular voice runner"),
        }
    }

    fn send_event(&mut self, value: Value) -> Result<()> {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .context("Cannot send to the modular voice runner")
    }
}

/// Only speech the audio backend attributes to the local talker reaches the server. Everything
/// else, including the assistant's own voice, is sent as silence so it cannot start a turn.
fn gate(voice: &mut Voice, pcm: Vec<i16>, near_end_speech: bool) -> Result<(Vec<i16>, bool)> {
    Ok(match voice.push(pcm, near_end_speech)? {
        Some(Segment::Start(pcm)) => (pcm, true),
        Some(Segment::Continue(pcm) | Segment::End(pcm)) => (pcm, false),
        None => (vec![0; FRAME], false),
    })
}

fn refill(backlog: &mut VecDeque<i16>, queued: usize) -> Option<Vec<i16>> {
    let room = PLAYBACK_AHEAD.saturating_sub(queued).min(backlog.len());
    (room > 0).then(|| backlog.drain(..room).collect())
}

/// A conversation through this computer's microphone and speakers.
pub(super) fn microphone(
    server: &mut Server,
    session: &mut Session,
    cache: &std::path::Path,
    stopped: &AtomicBool,
    language: &str,
) -> Result<()> {
    let (mut audio, captured) = {
        let _progress = crate::progress::opening_audio();
        crate::audio::Audio::start(cache, true, stopped)?
    };
    let language = language.trim_end_matches(" (Whisper)");
    if !crate::progress::finish(&crate::progress::ready(language)) {
        eprintln!("Ready. Speak {language}.");
    }
    converse(server, session, &mut audio, &captured, stopped)
}

/// Serves one visitor at a time until Ctrl+C: a browser page, which gets a conversation of its
/// own here, or a robot or program, which the door connects to the runner's own Realtime API.
pub(super) fn from_browser(
    server: &mut Server,
    door: &crate::serve::Options,
    voices: String,
    cache: &std::path::Path,
    stopped: &AtomicBool,
) -> Result<()> {
    let api = crate::serve::Api {
        address: server.address,
        voices,
    };
    let door = crate::serve::Door::open(door, cache, Some(api))?;
    while let Some(guest) = door.wait(stopped, || server.check())? {
        let mut session = match server.session(stopped) {
            Ok(session) => session,
            Err(error) if error.is::<crate::Cancelled>() => return Err(error),
            Err(error) => {
                // Turning this page away frees the seat; a runner that died is reported next.
                eprintln!("Could not start a conversation for the browser: {error:#}");
                continue;
            }
        };
        let (mut browser, captured) = guest.start();
        let result = converse(server, &mut session, &mut browser, &captured, stopped);
        if !browser.left() {
            return result;
        }
    }
    Ok(())
}

fn converse(
    server: &mut Server,
    session: &mut Session,
    audio: &mut dyn crate::audio::Device,
    captured: &std::sync::mpsc::Receiver<crate::audio::CaptureFrame>,
    stopped: &AtomicBool,
) -> Result<()> {
    use crate::{audio::CaptureFrame, talk::Note};
    use crossterm::event::{self as keys, Event as Input, KeyCode, KeyModifiers};
    use std::sync::{atomic::Ordering, mpsc::TryRecvError};

    let _terminal = crate::Terminal::enter()?;
    let mut panel = crate::talk::Panel::new(false);
    panel.note(Note::Intro);
    let mut voice = Voice::new();
    let mut backlog = VecDeque::new();
    let mut listening = true;

    while !stopped.load(Ordering::SeqCst) {
        audio.check_error()?;
        server.check()?;
        if keys::poll(Duration::ZERO)?
            && let Input::Key(key) = keys::read()?
        {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                stopped.store(true, Ordering::SeqCst);
                break;
            }
            if key.code == KeyCode::Esc && listening {
                listening = false;
                voice = Voice::new();
                audio.clear()?;
                panel.silenced();
                backlog.clear();
                session.interrupt()?;
                panel.note(Note::Paused);
            }
            if key.code == KeyCode::Enter && !listening {
                listening = true;
                panel.note(Note::Resumed);
            }
        }
        // Bound each tick so a continuous microphone cannot starve server events.
        for _ in 0..64 {
            let frame = match captured.try_recv() {
                Ok(CaptureFrame::Samples(frame)) => frame,
                Ok(CaptureFrame::Error(error)) => bail!("Audio: {error}"),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => bail!(
                    "The audio device disconnected. Restart after checking the selected devices."
                ),
            };
            if listening {
                panel.mic(&frame.samples);
            }
            let (pcm, started) = if listening {
                gate(&mut voice, frame.samples, frame.near_end_speech)?
            } else {
                (vec![0; FRAME], false)
            };
            if started
                && (session.interrupt()? || audio.pending_samples() > 0 || !backlog.is_empty())
            {
                audio.clear()?;
                panel.silenced();
                backlog.clear();
                panel.note(Note::Interrupted);
            }
            session.send(&pcm)?;
        }
        for _ in 0..64 {
            let Some(event) = session.poll()? else {
                break;
            };
            match event {
                Event::Heard(text) => panel.heard(&text),
                Event::Text(text) => panel.reply("Qwen", &text),
                Event::Audio(pcm) => backlog.extend(pcm),
                Event::SpeechStarted => {
                    if audio.pending_samples() > 0 || !backlog.is_empty() {
                        audio.clear()?;
                        panel.silenced();
                        backlog.clear();
                    }
                }
                Event::Done => panel.reply_close(),
            }
        }
        if let Some(pcm) = refill(&mut backlog, audio.pending_samples()) {
            audio.enqueue(&pcm)?;
            panel.played(&pcm);
        }
        panel.tick(audio.pending_samples());
    }
    audio.clear()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::Ordering};

    #[test]
    fn failed_runner_does_not_leave_its_native_child_running() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 60 >/dev/null 2>&1 & echo $!; exit 7"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let descendant: libc::pid_t = line.trim().parse().unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
        drop(Server {
            address: (Ipv4Addr::LOCALHOST, 0).into(),
            child,
            output: Arc::default(),
            readers: Vec::new(),
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let stopped = unsafe { libc::kill(descendant, 0) } < 0;
        if !stopped {
            unsafe { libc::kill(descendant, libc::SIGKILL) };
        }
        assert!(stopped, "The failed runner left its native child running");
    }

    fn send(socket: &mut WebSocket<TcpStream>, value: Value) {
        socket
            .send(Message::Text(value.to_string().into()))
            .unwrap();
    }

    fn receive(socket: &mut WebSocket<TcpStream>) -> Value {
        serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap()
    }

    fn audio(id: &str, samples: &[i16]) -> Value {
        let bytes: Vec<u8> = samples.iter().copied().flat_map(i16::to_le_bytes).collect();
        json!({"type":"response.output_audio.delta","response_id":id,"delta":STANDARD.encode(bytes)})
    }

    fn heard(text: &str) -> Value {
        json!({"type":"conversation.item.input_audio_transcription.completed","transcript":text})
    }

    fn accept(listener: &TcpListener) -> WebSocket<TcpStream> {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        tungstenite::accept(stream).unwrap()
    }

    /// Completes session setup as the runner does.
    fn set_up(socket: &mut WebSocket<TcpStream>) {
        send(socket, json!({"type":"session.created","session":{}}));
        let update = receive(socket);
        let audio = &update["session"]["audio"];
        assert_eq!(audio["input"]["turn_detection"]["type"], "server_vad");
        assert_eq!(audio["input"]["turn_detection"]["interrupt_response"], true);
        send(
            socket,
            json!({"type":"session.updated","session":{"audio":{"output":audio["output"]}}}),
        );
    }

    /// Accepts one client, completes session setup, then runs `script`.
    fn session(
        script: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static,
    ) -> (Session, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut socket = accept(&listener);
            set_up(&mut socket);
            script(&mut socket);
        });
        let stream = TcpStream::connect(address).unwrap();
        let session = Session::connect(stream, address, &AtomicBool::new(false)).unwrap();
        (session, server)
    }

    fn next(session: &mut Session) -> Event {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(event) = session.poll().unwrap() {
                return event;
            }
            assert!(Instant::now() < deadline, "No event arrived");
        }
    }

    #[test]
    fn only_near_end_speech_leaves_the_gate() {
        let mut voice = Voice::new();
        // Echo that macOS does not attribute to the local talker becomes silence.
        for _ in 0..20 {
            assert_eq!(
                gate(&mut voice, vec![7; FRAME], false).unwrap(),
                (vec![0; FRAME], false)
            );
        }
        for _ in 0..4 {
            assert_eq!(
                gate(&mut voice, vec![9; FRAME], true).unwrap(),
                (vec![0; FRAME], false)
            );
        }
        // The onset is released with its preroll, so the server hears the first word.
        let (onset, started) = gate(&mut voice, vec![9; FRAME], true).unwrap();
        assert!(started);
        assert_eq!(onset.len(), 10 * FRAME);
        assert_eq!(&onset[onset.len() - 5 * FRAME..], &[9; 5 * FRAME][..]);
        for _ in 0..29 {
            assert_eq!(
                gate(&mut voice, vec![3; FRAME], false).unwrap(),
                (vec![3; FRAME], false)
            );
        }
        assert_eq!(
            gate(&mut voice, vec![3; FRAME], false).unwrap(),
            (vec![3; FRAME], false)
        );
        assert_eq!(
            gate(&mut voice, vec![7; FRAME], false).unwrap(),
            (vec![0; FRAME], false)
        );
    }

    #[test]
    fn playback_is_fed_ahead_without_overfilling_the_native_queue() {
        let mut backlog: VecDeque<i16> = std::iter::repeat_n(1, PLAYBACK_AHEAD * 2).collect();
        assert_eq!(refill(&mut backlog, 0).unwrap().len(), PLAYBACK_AHEAD);
        assert!(refill(&mut backlog, PLAYBACK_AHEAD).is_none());
        assert_eq!(refill(&mut backlog, PLAYBACK_AHEAD - 10).unwrap().len(), 10);
        backlog.clear();
        assert!(refill(&mut backlog, 0).is_none());
    }

    #[test]
    fn a_new_conversation_waits_for_the_runner_to_let_go_of_the_last_one() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let runner = thread::spawn(move || {
            for busy in [true, true, false] {
                let mut socket = accept(&listener);
                if busy {
                    let slots = json!({"message":"All 1 session slots are in use."});
                    send(&mut socket, json!({"type":"error","error":slots}));
                } else {
                    set_up(&mut socket);
                }
            }
        });
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut server = Server {
            address,
            child,
            output: Arc::default(),
            readers: Vec::new(),
        };
        server.session(&AtomicBool::new(false)).unwrap();
        runner.join().unwrap();
    }

    #[test]
    fn microphone_audio_is_streamed_in_100_ms_appends_and_replies_decoded() {
        let (mut session, server) = session(|socket| {
            let append = receive(socket);
            assert_eq!(append["type"], "input_audio_buffer.append");
            let bytes = STANDARD.decode(append["audio"].as_str().unwrap()).unwrap();
            assert_eq!(bytes.len(), APPEND_SAMPLES * 2);
            assert_eq!(&bytes[..4], &[5, 0, 5, 0]);
            for event in [
                heard(" What is two plus two? "),
                json!({"type":"response.created","response":{"id":"r1"}}),
                json!({"type":"response.output_audio_transcript.delta","response_id":"r1","delta":"Four."}),
                audio("r1", &[1, -2]),
                json!({"type":"response.done","response":{"id":"r1","status":"completed"}}),
            ] {
                send(socket, event);
            }
        });
        for _ in 0..4 {
            session.send(&[5; FRAME]).unwrap();
        }
        session.send(&[5; FRAME + 10]).unwrap();
        assert_eq!(session.outgoing.len(), 10);
        assert_eq!(
            next(&mut session),
            Event::Heard("What is two plus two?".into())
        );
        assert_eq!(next(&mut session), Event::Text("Four.".into()));
        assert_eq!(next(&mut session), Event::Audio(vec![1, -2]));
        assert_eq!(next(&mut session), Event::Done);
        assert!(
            !session.interrupt().unwrap(),
            "A finished reply cannot be cancelled"
        );
        server.join().unwrap();
    }

    #[test]
    fn interrupted_reply_is_cancelled_and_its_late_output_discarded() {
        let (mut session, server) = session(|socket| {
            send(
                socket,
                json!({"type":"response.created","response":{"id":"r1"}}),
            );
            send(socket, audio("r1", &[1]));
            assert_eq!(receive(socket)["type"], "response.cancel");
            for event in [
                audio("r1", &[2]),
                json!({"type":"response.output_audio_transcript.delta","response_id":"r1","delta":"late"}),
                json!({"type":"response.done","response":{"id":"r1","status":"cancelled"}}),
                json!({"type":"response.created","response":{"id":"r2"}}),
                audio("r2", &[3]),
            ] {
                send(socket, event);
            }
        });
        assert_eq!(next(&mut session), Event::Audio(vec![1]));
        assert!(session.interrupt().unwrap());
        assert!(
            !session.interrupt().unwrap(),
            "Cancel is sent once per reply"
        );
        assert_eq!(next(&mut session), Event::Audio(vec![3]));
        server.join().unwrap();
    }

    #[test]
    fn server_detected_speech_silences_the_active_reply() {
        let (mut session, server) = session(|socket| {
            for event in [
                json!({"type":"response.created","response":{"id":"r1"}}),
                json!({"type":"input_audio_buffer.speech_started"}),
                audio("r1", &[1]),
                json!({"type":"response.done","response":{"id":"r1","status":"cancelled"}}),
                heard("Stop."),
            ] {
                send(socket, event);
            }
        });
        assert_eq!(next(&mut session), Event::SpeechStarted);
        assert_eq!(next(&mut session), Event::Heard("Stop.".into()));
        server.join().unwrap();
    }

    #[test]
    fn runner_errors_and_failed_replies_end_the_session() {
        for (event, message) in [
            (
                json!({"type":"error","error":{"message":"bad event"}}),
                "bad event",
            ),
            (
                json!({"type":"response.done","response":{"id":"r1","status":"failed"}}),
                "status: failed",
            ),
        ] {
            let (mut session, server) = session(move |socket| send(socket, event));
            let deadline = Instant::now() + Duration::from_secs(3);
            let error = loop {
                match session.poll() {
                    Err(error) => break error,
                    Ok(_) => assert!(Instant::now() < deadline, "No error surfaced"),
                }
            };
            assert!(format!("{error:#}").contains(message), "{error:#}");
            server.join().unwrap();
        }
    }

    #[test]
    fn startup_reports_a_runner_that_exits_before_it_is_ready() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo loading; echo 'missing weights' >&2; exit 3"]);
        let error = Server::start(command, Duration::from_secs(30), &AtomicBool::new(false))
            .err()
            .expect("startup must fail");
        let message = format!("{error:#}");
        assert!(message.contains("exit status: 3"), "{message}");
        assert!(
            message.contains("loading\nmissing weights")
                || message.contains("missing weights\nloading"),
            "{message}"
        );
    }

    #[test]
    fn startup_cancellation_stops_a_runner_that_never_becomes_ready() {
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        let cancel = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            signal.store(true, Ordering::SeqCst);
        });
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let began = Instant::now();
        let error = Server::start(command, Duration::from_secs(30), &stopped)
            .err()
            .unwrap();
        cancel.join().unwrap();
        assert!(error.is::<crate::Cancelled>(), "{error:#}");
        assert!(began.elapsed() < Duration::from_secs(5));
    }
}
