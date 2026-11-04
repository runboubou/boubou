use crate::profiles::Profile;
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::ErrorKind,
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

const MAX_TURN_SAMPLES: usize = 16_000 * 30;
// Conservative allowances for the fixed prompt, turn delimiters and playback notice.
const SESSION_OVERHEAD_TOKENS: usize = 512;
const TURN_OVERHEAD_TOKENS: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);
// A looping model gives one sentence whatever the user says and does not recover in that session.
// Short answers, such as a name, and a reply the user asks to hear again may legitimately repeat.
const MAX_IDENTICAL_REPLIES: usize = 4;
const MIN_LOOP_WORDS: usize = 4;
#[derive(Debug, PartialEq)]
pub enum Output {
    Text(String),
    Audio(Vec<i16>),
    Done,
    Interrupted,
    ContextCleared,
}

#[derive(Debug)]
enum Phase {
    Idle,
    Renewing {
        worker: JoinHandle<Result<Realtime>>,
        stopped: Arc<AtomicBool>,
    },
    Generating {
        id: Option<String>,
        since: Instant,
    },
    Cancelling {
        id: Option<String>,
        since: Instant,
        last_sent: Instant,
    },
}

pub struct Realtime {
    profile: &'static Profile,
    socket: Option<WebSocket<TcpStream>>,
    address: String,
    phase: Phase,
    context_tokens: usize,
    interrupted: bool,
    user_samples: usize,
    audio_samples: usize,
    reply: String,
    last_reply: String,
    identical_replies: usize,
}

impl Realtime {
    pub fn connect(address: &str, profile: &'static Profile, stopped: &AtomicBool) -> Result<Self> {
        crate::check_cancel(stopped)?;
        let stream = TcpStream::connect_timeout(&address.parse()?, Duration::from_secs(1))
            .context("Cannot connect to the local Liquid backend")?;
        stream.set_nonblocking(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut handshake = tungstenite::client(format!("ws://{address}/v1/realtime"), stream);
        let (socket, _) = loop {
            crate::check_cancel(stopped)?;
            ensure!(
                Instant::now() < deadline,
                "Liquid WebSocket handshake timed out"
            );
            match handshake {
                Ok(connected) => break connected,
                Err(tungstenite::HandshakeError::Failure(error)) => {
                    return Err(error)
                        .context("Liquid backend does not support the realtime protocol");
                }
                Err(tungstenite::HandshakeError::Interrupted(pending)) => {
                    std::thread::sleep(Duration::from_millis(10));
                    handshake = pending.handshake();
                }
            }
        };
        socket.get_ref().set_nonblocking(false)?;
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(10)))?;
        let mut this = Self {
            profile,
            socket: Some(socket),
            address: address.to_owned(),
            phase: Phase::Idle,
            context_tokens: SESSION_OVERHEAD_TOKENS,
            interrupted: false,
            user_samples: 0,
            audio_samples: 0,
            reply: String::new(),
            last_reply: String::new(),
            identical_replies: 0,
        };
        let created = this.wait_for("session.created", stopped)?;
        ensure!(
            created["session"]["audio"]["input"]["format"]["rate"] == 16000,
            "This Liquid profile requires 16 kHz input"
        );
        ensure!(
            created["session"]["audio"]["output"]["format"]["rate"] == 24000,
            "This Liquid profile requires 24 kHz output"
        );
        this.send(json!({"type":"session.update","session":{
            "type":"realtime",
            "instructions":profile.instructions,
            "output_modalities":["text","audio"],
            "max_output_tokens":profile.max_output_tokens
        }}))?;
        this.wait_for("session.updated", stopped)?;
        Ok(this)
    }

    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    pub fn cancelling(&self) -> bool {
        matches!(self.phase, Phase::Cancelling { .. })
    }

    /// Check before taking queued audio: renewal must not consume or split a turn.
    pub fn ready_for_audio(&mut self) -> Result<bool> {
        if self.busy() {
            return Ok(false);
        }
        if self.user_samples > 0 {
            return Ok(true);
        }
        self.prepare_turn(MAX_TURN_SAMPLES.div_ceil(self.profile.audio_samples_per_token))
    }

    fn can_admit(&self, input_tokens: usize) -> bool {
        self.identical_replies < MAX_IDENTICAL_REPLIES
            && self.context_tokens
                + input_tokens
                + TURN_OVERHEAD_TOKENS
                + self.profile.max_output_tokens
                < self.profile.context_tokens
    }

    fn prepare_turn(&mut self, input_tokens: usize) -> Result<bool> {
        if self.busy() {
            return Ok(false);
        }
        ensure!(
            SESSION_OVERHEAD_TOKENS
                + input_tokens
                + TURN_OVERHEAD_TOKENS
                + self.profile.max_output_tokens
                < self.profile.context_tokens,
            "A single turn exceeds Liquid's context capacity"
        );
        if self.can_admit(input_tokens) {
            return Ok(true);
        }
        let mut socket = self.socket.take().context("Liquid connection is missing")?;
        let address = self.address.clone();
        let profile = self.profile;
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        // b11960 has no history-delete event. Closing releases its sole slot;
        // waiting for a fresh session must not block microphone capture or keys.
        let worker = thread::Builder::new()
            .name("boubou-context-renewal".into())
            .spawn(move || {
                let _ = socket.close(None);
                drop(socket);
                Self::connect(&address, profile, &signal).context("Cannot renew Liquid context")
            })
            .context("Cannot start Liquid context renewal")?;
        self.phase = Phase::Renewing { worker, stopped };
        crate::trace(
            "context.renewing",
            format_args!(
                "budget_tokens={} identical_replies={}",
                self.context_tokens, self.identical_replies
            ),
        );
        Ok(false)
    }

    pub fn append_audio(&mut self, samples: &[i16]) -> Result<()> {
        ensure!(
            !self.busy(),
            "Wait for the previous response to stop before sending a new turn"
        );
        ensure!(
            self.user_samples + samples.len() <= MAX_TURN_SAMPLES,
            "A voice turn exceeded 30 seconds. Start a new conversation with shorter turns."
        );
        if samples.is_empty() {
            return Ok(());
        }
        if self.user_samples == 0 {
            ensure!(
                self.can_admit(MAX_TURN_SAMPLES.div_ceil(self.profile.audio_samples_per_token)),
                "Wait for automatic context renewal before sending a new turn"
            );
            self.context_tokens += TURN_OVERHEAD_TOKENS;
        }
        self.begin_user()?;
        let bytes: Vec<u8> = samples.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.send(json!({"type":"input_audio_buffer.append","audio":STANDARD.encode(bytes)}))?;
        let next_samples = self.user_samples + samples.len();
        self.context_tokens += next_samples.div_ceil(self.profile.audio_samples_per_token)
            - self
                .user_samples
                .div_ceil(self.profile.audio_samples_per_token);
        self.user_samples = next_samples;
        Ok(())
    }

    fn begin_user(&mut self) -> Result<()> {
        if self.interrupted {
            // This backend cannot truncate to a played timestamp. Preserve the conversation,
            // but explicitly tell the model not to assume the previous answer was heard.
            self.send(json!({"type":"conversation.item.create","item":{
                "type":"message","role":"user","content":[{"type":"input_text","text":
                "[Playback notice: your previous reply was interrupted. The user may not have heard the remainder. Respond to the following new utterance.]"}]
            }}))?;
            self.interrupted = false;
        }
        Ok(())
    }

    pub fn finish_audio(&mut self) -> Result<()> {
        ensure!(self.user_samples > 0, "Cannot submit an empty audio turn");
        self.request_response()
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn end_input_without_response(&mut self) -> Result<()> {
        if self.user_samples > 0 {
            crate::trace(
                "input.commit_without_response",
                format_args!("samples={}", self.user_samples),
            );
            // This backend encodes audio as it arrives; clear is a no-op. End the
            // partial audio without response.create, so it cannot speak on its own.
            self.send(json!({"type":"input_audio_buffer.commit"}))?;
            self.user_samples = 0;
        }
        Ok(())
    }

    #[cfg(test)]
    fn text_turn(&mut self, text: &str) -> Result<()> {
        // Tests use a byte upper bound; production input is always audio.
        self.text_turn_with_tokens(text, text.len()).map(|_| ())
    }

    #[cfg(test)]
    fn text_turn_with_tokens(&mut self, text: &str, input_tokens: usize) -> Result<bool> {
        ensure!(!self.busy(), "The previous turn is still running");
        let mut renewed = false;
        while !self.prepare_turn(input_tokens)? {
            renewed |= matches!(self.poll()?, Some(Output::ContextCleared));
        }
        self.context_tokens += input_tokens + TURN_OVERHEAD_TOKENS;
        self.begin_user()?;
        self.send(json!({"type":"conversation.item.create","item":{
            "type":"message","role":"user","content":[{"type":"input_text","text":text}]
        }}))?;
        self.request_response()?;
        Ok(renewed)
    }

    fn request_response(&mut self) -> Result<()> {
        ensure!(!self.busy(), "The previous turn is still running");
        crate::trace(
            "response.create.send",
            format_args!("samples={}", self.user_samples),
        );
        self.send(json!({"type":"response.create"}))?;
        self.context_tokens += self.profile.max_output_tokens;
        crate::trace("response.create.sent", format_args!(""));
        self.user_samples = 0;
        self.audio_samples = 0;
        self.reply.clear();
        self.phase = Phase::Generating {
            id: None,
            since: Instant::now(),
        };
        Ok(())
    }

    /// Call even if generation has ended but buffered audio is still playing.
    pub fn interrupt(&mut self) -> Result<bool> {
        if matches!(self.phase, Phase::Renewing { .. }) {
            return Ok(false);
        }
        self.interrupted = true;
        if let Phase::Generating { id, .. } = &self.phase {
            let id = id.clone();
            crate::trace("response.cancel.send", format_args!("id={id:?}"));
            self.send(json!({"type":"response.cancel"}))?;
            crate::trace("response.cancel.sent", format_args!("id={id:?}"));
            let now = Instant::now();
            self.phase = Phase::Cancelling {
                id,
                since: now,
                last_sent: now,
            };
        }
        Ok(true)
    }

    pub fn poll(&mut self) -> Result<Option<Output>> {
        if let Phase::Renewing { worker, .. } = &self.phase {
            if !worker.is_finished() {
                thread::sleep(Duration::from_millis(10));
                return Ok(None);
            }
            let Phase::Renewing { worker, .. } = std::mem::replace(&mut self.phase, Phase::Idle)
            else {
                unreachable!();
            };
            *self = worker
                .join()
                .map_err(|_| anyhow!("Liquid context renewal worker failed"))??;
            crate::trace("context.renewed", format_args!(""));
            return Ok(Some(Output::ContextCleared));
        }
        match &self.phase {
            Phase::Generating { since, .. } => ensure!(
                since.elapsed() < REQUEST_TIMEOUT,
                "Liquid stopped responding. Restart the conversation."
            ),
            Phase::Cancelling {
                since, last_sent, ..
            } => {
                ensure!(
                    since.elapsed() < CANCEL_TIMEOUT,
                    "Liquid did not acknowledge cancellation. The conversation has been stopped."
                );
                // b11960 can ignore cancellation while prefill is queued. Retry only within
                // this response, until its terminal acknowledgement, before admitting new audio.
                if last_sent.elapsed() >= Duration::from_millis(100) {
                    self.send(json!({"type":"response.cancel"}))?;
                    if let Phase::Cancelling { last_sent, .. } = &mut self.phase {
                        *last_sent = Instant::now();
                    }
                }
            }
            Phase::Idle | Phase::Renewing { .. } => {}
        }
        let Some(event) = self.read()? else {
            return Ok(None);
        };
        self.process(event)
    }

    fn process(&mut self, event: Value) -> Result<Option<Output>> {
        let kind = event["type"].as_str().unwrap_or_default();
        if kind == "error" {
            bail!("Liquid: {}", event["error"]["message"]);
        }
        if kind == "response.created" {
            let new_id = event["response"]["id"]
                .as_str()
                .context("Response is missing its ID")?;
            crate::trace(
                "response.created",
                format_args!("id={new_id} cancelling={}", self.cancelling()),
            );
            match &mut self.phase {
                Phase::Generating { id, .. } | Phase::Cancelling { id, .. } => {
                    ensure!(id.is_none(), "Unexpected overlapping Liquid responses");
                    *id = Some(new_id.to_owned());
                }
                Phase::Idle | Phase::Renewing { .. } => {
                    bail!("Received an unsolicited Liquid response");
                }
            }
            return Ok(None);
        }
        let id = event["response_id"]
            .as_str()
            .or_else(|| event["response"]["id"].as_str());
        let expected = match &self.phase {
            Phase::Generating { id, .. } | Phase::Cancelling { id, .. } => id.as_deref(),
            Phase::Idle | Phase::Renewing { .. } => None,
        };
        if expected.is_none() || id != expected {
            if kind == "response.done" {
                crate::trace(
                    "response.drop_mismatched",
                    format_args!("kind={kind} id={id:?} expected={expected:?}"),
                );
            }
            return Ok(None);
        }
        if kind == "response.done" {
            let cancelled = self.cancelling();
            let status = event["response"]["status"].as_str().unwrap_or_default();
            ensure!(
                status == "completed" || status == "cancelled",
                "Liquid response failed (status: {status})"
            );
            if let Some(tokens) = event["response"]["usage"]["output_tokens"]
                .as_u64()
                .filter(|&tokens| tokens <= self.profile.max_output_tokens as u64)
            {
                self.context_tokens -= self.profile.max_output_tokens - tokens as usize;
            }
            if !cancelled {
                // Interrupted text is partial; only finished replies show a loop.
                let reply = normalized_words(&std::mem::take(&mut self.reply));
                self.identical_replies = if reply.split(' ').count() < MIN_LOOP_WORDS {
                    0
                } else if reply == self.last_reply {
                    self.identical_replies + 1
                } else {
                    1
                };
                self.last_reply = reply;
            }
            crate::trace(
                "response.done",
                format_args!(
                    "id={id:?} status={status} cancelling={cancelled} audio_samples={} identical_replies={}",
                    self.audio_samples, self.identical_replies
                ),
            );
            self.phase = Phase::Idle;
            return Ok(Some(if cancelled {
                Output::Interrupted
            } else {
                Output::Done
            }));
        }
        if self.cancelling() {
            return Ok(None);
        }
        match kind {
            "response.output_text.delta" => {
                let delta = event["delta"].as_str().context("Invalid text delta")?;
                self.reply.push_str(delta);
                Ok(Some(Output::Text(delta.to_owned())))
            }
            "response.output_audio.delta" => {
                let bytes =
                    STANDARD.decode(event["delta"].as_str().context("Invalid audio delta")?)?;
                ensure!(
                    bytes.len() % 2 == 0,
                    "Liquid returned an incomplete PCM16 sample"
                );
                if self.audio_samples == 0 {
                    crate::trace("response.first_audio", format_args!("id={id:?}"));
                }
                self.audio_samples += bytes.len() / 2;
                Ok(Some(Output::Audio(
                    bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .copied()
                        .map(i16::from_le_bytes)
                        .collect(),
                )))
            }
            _ => Ok(None),
        }
    }

    fn wait_for(&mut self, kind: &str, stopped: &AtomicBool) -> Result<Value> {
        let started = Instant::now();
        loop {
            crate::check_cancel(stopped)?;
            ensure!(
                started.elapsed() < Duration::from_secs(30),
                "Timed out waiting for {kind}"
            );
            if let Some(event) = self.read()? {
                if event["type"] == "error" {
                    bail!("Liquid: {}", event["error"]["message"]);
                }
                if event["type"] == kind {
                    return Ok(event);
                }
            }
        }
    }

    fn read(&mut self) -> Result<Option<Value>> {
        let socket = self
            .socket
            .as_mut()
            .context("Liquid connection is missing")?;
        match socket.read() {
            Ok(Message::Text(text)) => Ok(Some(serde_json::from_str(&text)?)),
            Ok(Message::Ping(_)) => {
                socket.flush()?;
                Ok(None)
            }
            Ok(Message::Close(_)) => bail!("The Liquid backend closed the conversation"),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                Ok(None)
            }
            Err(e) => Err(e).context("Lost connection to the local Liquid backend"),
        }
    }

    fn send(&mut self, value: Value) -> Result<()> {
        self.socket
            .as_mut()
            .context("Liquid connection is missing")?
            .send(Message::Text(value.to_string().into()))
            .context("Cannot send to Liquid")
    }
}

fn normalized_words(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

impl Drop for Realtime {
    fn drop(&mut self) {
        if let Phase::Renewing { worker, stopped } = std::mem::replace(&mut self.phase, Phase::Idle)
        {
            stopped.store(true, Ordering::SeqCst);
            let _ = worker.join();
        }
        if let Some(socket) = &mut self.socket {
            let _ = socket.close(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::profiles::DEFAULT;
    const MAX_OUTPUT_TOKENS: usize = DEFAULT.max_output_tokens;
    const AUDIO_SAMPLES_PER_TOKEN: usize = DEFAULT.audio_samples_per_token;
    const MAX_AUDIO_INPUT_TOKENS: usize = MAX_TURN_SAMPLES.div_ceil(AUDIO_SAMPLES_PER_TOKEN);
    use super::*;
    use std::{net::TcpListener, thread};

    fn accept_mock(listener: TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "Mock server accepted no connection"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("Mock server could not accept a connection: {error}"),
            }
        }
    }

    #[test]
    fn startup_cancellation_covers_handshake_and_session_initialization() {
        use std::sync::{Arc, atomic::Ordering};
        for stage in 0..3 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let stopped = Arc::new(AtomicBool::new(false));
            let signal = stopped.clone();
            let server = std::thread::spawn(move || {
                let stream = accept_mock(listener);
                if stage == 0 {
                    signal.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                    return;
                }
                let mut socket = tungstenite::accept(stream).unwrap();
                if stage == 2 {
                    socket
                        .send(Message::Text(
                            json!({"type":"session.created","session":{"audio":{
                                "input":{"format":{"rate":16000}},"output":{"format":{"rate":24000}}
                            }}})
                            .to_string()
                            .into(),
                        ))
                        .unwrap();
                    let _ = socket.read().unwrap();
                }
                signal.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
            });
            let began = Instant::now();
            let error = Realtime::connect(&address, &DEFAULT, &stopped)
                .err()
                .expect("Startup must be cancelled");
            server.join().unwrap();
            assert!(error.is::<crate::Cancelled>(), "Stage {stage}: {error:#}");
            assert!(began.elapsed() < Duration::from_secs(1));
        }
    }
    fn mock_server(
        work: impl FnOnce(WebSocket<TcpStream>) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let worker = thread::spawn(move || {
            let stream = accept_mock(listener);
            let mut ws = tungstenite::accept(stream).unwrap();
            context_session_ready(&mut ws, &DEFAULT);
            work(ws);
        });
        (address, worker)
    }

    #[test]
    fn interrupt_before_created_discards_late_audio_and_waits_for_done() {
        let (addr, server) = mock_server(|mut ws| {
            let _ = ws.read().unwrap(); // user text
            let _ = ws.read().unwrap(); // create
            let _ = ws.read().unwrap(); // cancel
            for ev in [
                json!({"type":"response.created","response":{"id":"old"}}),
                json!({"type":"response.output_audio.delta","response_id":"old","delta":STANDARD.encode([1,0])}),
                json!({"type":"response.done","response":{"id":"old","status":"cancelled"}}),
            ] {
                ws.send(Message::Text(ev.to_string().into())).unwrap();
            }
            let notice: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert!(
                notice["item"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("interrupted")
            );
            let _ = ws.read().unwrap(); // actual next user text
            let _ = ws.read().unwrap(); // response.create
            ws.send(Message::Text(
                json!({"type":"response.created","response":{"id":"next"}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
            ws.send(Message::Text(json!({"type":"response.output_audio.delta","response_id":"old","delta":STANDARD.encode([1,0])}).to_string().into())).unwrap();
            ws.send(Message::Text(json!({"type":"response.output_audio.delta","response_id":"next","delta":STANDARD.encode([2,0])}).to_string().into())).unwrap();
            ws.send(Message::Text(
                json!({"type":"response.done","response":{"id":"next","status":"completed"}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        client.text_turn("hello").unwrap();
        client.interrupt().unwrap();
        assert!(client.text_turn("too soon").is_err());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(Instant::now() < deadline);
            if let Some(output) = client.poll().unwrap() {
                assert_eq!(output, Output::Interrupted);
                break;
            }
        }
        client.text_turn("next").unwrap();
        let mut audio = Vec::new();
        while client.busy() {
            assert!(Instant::now() < deadline);
            if let Some(Output::Audio(pcm)) = client.poll().unwrap() {
                audio.extend(pcm);
            }
        }
        assert_eq!(audio, [2]);
        server.join().expect("Mock server assertions failed");
    }

    #[test]
    fn ending_partial_input_does_not_request_a_reply() {
        let (addr, server) = mock_server(|mut ws| {
            let append: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(append["type"], "input_audio_buffer.append");
            let commit: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(commit["type"], "input_audio_buffer.commit");
            ws.get_ref()
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            assert!(matches!(ws.read(), Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        client.append_audio(&[0; 320]).unwrap();
        client.end_input_without_response().unwrap();
        client.end_input_without_response().unwrap();
        assert!(client.finish_audio().is_err());
        assert!(!client.busy());
        server.join().expect("Mock server assertions failed");
    }

    #[test]
    fn odd_pcm_delta_is_rejected() {
        let (addr, server) = mock_server(|_| thread::sleep(Duration::from_millis(200)));
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        client.phase = Phase::Generating {
            id: Some("x".into()),
            since: Instant::now(),
        };
        assert!(client.process(json!({"type":"response.output_audio.delta","response_id":"x","delta":STANDARD.encode([1])})).is_err());
        server.join().expect("Mock server assertions failed");
    }

    fn context_event(ws: &mut WebSocket<TcpStream>, kind: &str) -> Value {
        let event: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(event["type"], kind);
        event
    }

    fn context_session_ready(ws: &mut WebSocket<TcpStream>, profile: &Profile) {
        ws.send(Message::Text(
            json!({"type":"session.created","session":{"audio":{
                "input":{"format":{"rate":16000}},"output":{"format":{"rate":24000}}
            }}})
            .to_string()
            .into(),
        ))
        .unwrap();
        let update = context_event(ws, "session.update");
        assert_eq!(
            update["session"]["max_output_tokens"],
            profile.max_output_tokens
        );
        let instructions = update["session"]["instructions"].as_str().unwrap();
        assert_eq!(instructions, profile.instructions);
        ws.send(Message::Text(
            json!({"type":"session.updated"}).to_string().into(),
        ))
        .unwrap();
    }

    fn context_session(listener: &TcpListener) -> WebSocket<TcpStream> {
        let mut ws = tungstenite::accept(accept_mock(listener.try_clone().unwrap())).unwrap();
        context_session_ready(&mut ws, &DEFAULT);
        ws
    }

    fn context_reply(ws: &mut WebSocket<TcpStream>, id: usize) {
        for event in [
            json!({"type":"response.created","response":{"id":id.to_string()}}),
            json!({"type":"response.done","response":{"id":id.to_string(),"status":"completed",
                "usage":{"output_tokens":MAX_OUTPUT_TOKENS}}}),
        ] {
            ws.send(Message::Text(event.to_string().into())).unwrap();
        }
    }

    fn next_context_output(client: &mut Realtime) -> Output {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                Instant::now() < deadline,
                "No response from the mock session"
            );
            if let Some(output) = client.poll().unwrap() {
                return output;
            }
        }
    }

    #[test]
    fn selected_profile_survives_renewal_and_controls_token_accounting() {
        static PROFILE: Profile = Profile {
            context_tokens: 4096,
            max_output_tokens: 64,
            audio_samples_per_token: 2560,
            instructions: "Use the selected profile.",
            ..DEFAULT
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            for session in 0..2 {
                let mut ws =
                    tungstenite::accept(accept_mock(listener.try_clone().unwrap())).unwrap();
                context_session_ready(&mut ws, &PROFILE);
                if session == 1 {
                    context_event(&mut ws, "input_audio_buffer.append");
                    context_event(&mut ws, "response.create");
                    for event in [
                        json!({"type":"response.created","response":{"id":"selected"}}),
                        json!({"type":"response.done","response":{"id":"selected","status":"completed", "usage":{"output_tokens":1}}}),
                    ] {
                        ws.send(Message::Text(event.to_string().into())).unwrap();
                    }
                }
                assert!(matches!(ws.read().unwrap(), Message::Close(_)));
            }
        });
        let mut client = Realtime::connect(&address, &PROFILE, &AtomicBool::new(false)).unwrap();
        client.context_tokens = PROFILE.context_tokens;
        assert!(!client.ready_for_audio().unwrap());
        assert_eq!(next_context_output(&mut client), Output::ContextCleared);
        assert!(client.ready_for_audio().unwrap());
        client.append_audio(&vec![1; 2561]).unwrap();
        let input_tokens = SESSION_OVERHEAD_TOKENS + TURN_OVERHEAD_TOKENS + 2;
        assert_eq!(client.context_tokens, input_tokens);
        client.finish_audio().unwrap();
        assert_eq!(client.context_tokens, input_tokens + 64);
        assert_eq!(next_context_output(&mut client), Output::Done);
        assert_eq!(client.context_tokens, input_tokens + 1);
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn audio_budget_rounds_each_committed_stream_and_keeps_paused_input() {
        let (addr, server) = mock_server(|mut ws| {
            for kind in [
                "input_audio_buffer.append",
                "input_audio_buffer.append",
                "input_audio_buffer.append",
                "input_audio_buffer.commit",
                "input_audio_buffer.append",
                "input_audio_buffer.commit",
            ] {
                context_event(&mut ws, kind);
            }
            assert!(matches!(ws.read().unwrap(), Message::Close(_)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        assert!(client.ready_for_audio().unwrap());
        client.append_audio(&[]).unwrap();
        assert_eq!(client.context_tokens, SESSION_OVERHEAD_TOKENS);
        client.append_audio(&[1]).unwrap();
        client
            .append_audio(&vec![2; AUDIO_SAMPLES_PER_TOKEN - 1])
            .unwrap();
        assert_eq!(
            client.context_tokens,
            SESSION_OVERHEAD_TOKENS + TURN_OVERHEAD_TOKENS + 1
        );
        client.append_audio(&[3]).unwrap();
        client.end_input_without_response().unwrap();
        assert_eq!(
            client.context_tokens,
            SESSION_OVERHEAD_TOKENS + TURN_OVERHEAD_TOKENS + 2
        );
        assert!(client.ready_for_audio().unwrap());
        client.append_audio(&[4]).unwrap();
        client.end_input_without_response().unwrap();
        assert_eq!(
            client.context_tokens,
            SESSION_OVERHEAD_TOKENS + 2 * TURN_OVERHEAD_TOKENS + 3
        );
        assert!(!client.busy());
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn admission_reserves_a_whole_turn_before_its_first_sample() {
        let (addr, server) = mock_server(|mut ws| {
            context_event(&mut ws, "input_audio_buffer.append");
            context_event(&mut ws, "input_audio_buffer.append");
            context_event(&mut ws, "response.create");
            context_reply(&mut ws, 1);
            assert!(matches!(ws.read().unwrap(), Message::Close(_)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        let reserve = MAX_AUDIO_INPUT_TOKENS + TURN_OVERHEAD_TOKENS + MAX_OUTPUT_TOKENS;
        client.context_tokens = DEFAULT.context_tokens - reserve - 1;
        assert!(client.ready_for_audio().unwrap());
        client.append_audio(&[1]).unwrap();
        assert!(
            client.ready_for_audio().unwrap(),
            "Do not renew halfway through input"
        );
        client.append_audio(&vec![2; MAX_TURN_SAMPLES - 1]).unwrap();
        client.finish_audio().unwrap();
        assert_eq!(client.context_tokens, DEFAULT.context_tokens - 1);
        assert!(!client.ready_for_audio().unwrap());
        assert_eq!(next_context_output(&mut client), Output::Done);
        client.context_tokens = DEFAULT.context_tokens - reserve;
        assert!(
            client.append_audio(&[3]).is_err(),
            "Reject input until renewal is admitted"
        );
        assert!(
            !client.ready_for_audio().unwrap(),
            "Exact capacity must renew before input"
        );
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn only_matching_valid_usage_releases_the_reserved_response_budget() {
        let (addr, server) = mock_server(|mut ws| {
            assert!(matches!(ws.read().unwrap(), Message::Close(_)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        for (usage, retained) in [
            (Value::Null, MAX_OUTPUT_TOKENS),
            (json!({}), MAX_OUTPUT_TOKENS),
            (json!({"output_tokens":-1}), MAX_OUTPUT_TOKENS),
            (json!({"output_tokens":"12"}), MAX_OUTPUT_TOKENS),
            (
                json!({"output_tokens":MAX_OUTPUT_TOKENS + 1}),
                MAX_OUTPUT_TOKENS,
            ),
            (json!({"output_tokens":0}), 0),
            (json!({"output_tokens":17}), 17),
            (
                json!({"output_tokens":MAX_OUTPUT_TOKENS}),
                MAX_OUTPUT_TOKENS,
            ),
        ] {
            for cancelled in [false, true] {
                let now = Instant::now();
                client.context_tokens = SESSION_OVERHEAD_TOKENS + MAX_OUTPUT_TOKENS;
                client.phase = if cancelled {
                    Phase::Cancelling {
                        id: Some("current".into()),
                        since: now,
                        last_sent: now,
                    }
                } else {
                    Phase::Generating {
                        id: Some("current".into()),
                        since: now,
                    }
                };
                let stale = json!({"type":"response.done","response":{"id":"old","status":"completed",
                    "usage":{"output_tokens":0}}});
                assert_eq!(client.process(stale).unwrap(), None);
                assert_eq!(
                    client.context_tokens,
                    SESSION_OVERHEAD_TOKENS + MAX_OUTPUT_TOKENS
                );
                let event = json!({"type":"response.done","response":{"id":"current",
                    "status":if cancelled { "cancelled" } else { "completed" },"usage":usage}});
                assert_eq!(
                    client.process(event.clone()).unwrap(),
                    Some(if cancelled {
                        Output::Interrupted
                    } else {
                        Output::Done
                    })
                );
                assert_eq!(client.context_tokens, SESSION_OVERHEAD_TOKENS + retained);
                assert_eq!(client.process(event).unwrap(), None);
                assert_eq!(client.context_tokens, SESSION_OVERHEAD_TOKENS + retained);
            }
        }
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn repeated_turns_renew_sessions_beyond_the_context_window() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            let mut completed = 0;
            let mut sessions = 0;
            while completed < 60 {
                let mut ws = context_session(&listener);
                let mut used = SESSION_OVERHEAD_TOKENS;
                sessions += 1;
                loop {
                    let message = ws.read().unwrap();
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                    assert_eq!(event["type"], "input_audio_buffer.append");
                    assert!(
                        used + MAX_AUDIO_INPUT_TOKENS + TURN_OVERHEAD_TOKENS + MAX_OUTPUT_TOKENS
                            < DEFAULT.context_tokens
                    );
                    let pcm = STANDARD.decode(event["audio"].as_str().unwrap()).unwrap();
                    assert_eq!(pcm.len(), 640);
                    used += TURN_OVERHEAD_TOKENS + 1 + MAX_OUTPUT_TOKENS;
                    context_event(&mut ws, "response.create");
                    completed += 1;
                    context_reply(&mut ws, completed);
                }
            }
            sessions
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        let mut renewals = 0;
        for _ in 0..60 {
            if !client.ready_for_audio().unwrap() {
                assert_eq!(next_context_output(&mut client), Output::ContextCleared);
                renewals += 1;
                assert!(client.ready_for_audio().unwrap());
            }
            client.append_audio(&[1; 320]).unwrap();
            client.finish_audio().unwrap();
            assert_eq!(next_context_output(&mut client), Output::Done);
        }
        drop(client);
        assert!(renewals >= 2);
        assert_eq!(server.join().unwrap(), renewals + 1);
        assert!(60 * (TURN_OVERHEAD_TOKENS + 1 + MAX_OUTPUT_TOKENS) > 2 * DEFAULT.context_tokens);
    }

    #[test]
    fn delayed_renewal_keeps_capture_responsive_and_queued_turns_separate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (arrived, waiting) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut old = context_session(&listener);
            assert!(matches!(old.read().unwrap(), Message::Close(_)));
            let mut ws = tungstenite::accept(accept_mock(listener)).unwrap();
            arrived.send(()).unwrap();
            resumed.recv_timeout(Duration::from_secs(3)).unwrap();
            context_session_ready(&mut ws, &DEFAULT);
            for (id, expected) in [(1, vec![1_i16, 2]), (2, vec![3_i16, 4])] {
                let input = context_event(&mut ws, "input_audio_buffer.append");
                let bytes: Vec<u8> = expected
                    .iter()
                    .flat_map(|sample| sample.to_le_bytes())
                    .collect();
                assert_eq!(
                    STANDARD.decode(input["audio"].as_str().unwrap()).unwrap(),
                    bytes
                );
                context_event(&mut ws, "response.create");
                context_reply(&mut ws, id);
            }
            assert!(matches!(ws.read().unwrap(), Message::Close(_)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        let mut pending = crate::voice::PendingAudio::default();
        pending.push(crate::voice::Segment::Start(vec![1])).unwrap();
        pending.push(crate::voice::Segment::End(vec![2])).unwrap();
        client.context_tokens = DEFAULT.context_tokens;
        crate::send_pending(&mut pending, &mut client, true).unwrap();
        assert!(client.busy());
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        pending.push(crate::voice::Segment::Start(vec![3])).unwrap();
        assert!(!client.interrupt().unwrap());
        assert!(!client.interrupted, "Renewal is not an interrupted reply");
        assert!(!client.ready_for_audio().unwrap());
        crate::send_pending(&mut pending, &mut client, false).unwrap();
        // The server cannot initialize until this call has returned to capture processing.
        assert_eq!(client.poll().unwrap(), None);
        resume.send(()).unwrap();
        assert_eq!(next_context_output(&mut client), Output::ContextCleared);
        crate::send_pending(&mut pending, &mut client, false).unwrap();
        assert!(
            !client.busy(),
            "Do not reply while the next utterance is active"
        );
        pending.push(crate::voice::Segment::End(vec![4])).unwrap();
        for _ in 0..2 {
            crate::send_pending(&mut pending, &mut client, true).unwrap();
            assert!(client.busy());
            assert_eq!(next_context_output(&mut client), Output::Done);
        }
        assert!(pending.take(true).is_none());
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn renewal_failure_is_reported_and_drop_cancels_its_initialization() {
        for fail in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let (arrived, waiting) = std::sync::mpsc::channel();
            let server = thread::spawn(move || {
                let mut old = context_session(&listener);
                assert!(matches!(old.read().unwrap(), Message::Close(_)));
                let mut ws = tungstenite::accept(accept_mock(listener)).unwrap();
                // Prove the client finished the upgrade before requiring a Close frame.
                ws.send(Message::Ping(vec![1].into())).unwrap();
                assert!(matches!(ws.read().unwrap(), Message::Pong(_)));
                if fail {
                    ws.send(Message::Text(
                        json!({"type":"error","error":{"message":"Renewal rejected"}})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                }
                arrived.send(()).unwrap();
                assert!(matches!(ws.read().unwrap(), Message::Close(_)));
            });
            let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
            client.context_tokens = DEFAULT.context_tokens;
            assert!(!client.ready_for_audio().unwrap());
            waiting.recv_timeout(Duration::from_secs(3)).unwrap();
            if fail {
                let deadline = Instant::now() + Duration::from_secs(3);
                let error = loop {
                    assert!(
                        Instant::now() < deadline,
                        "Renewal failure was not reported"
                    );
                    match client.poll() {
                        Err(error) => break error,
                        Ok(output) => assert_eq!(output, None),
                    }
                };
                assert!(format!("{error:#}").contains("Renewal rejected"));
            }
            let began = Instant::now();
            drop(client);
            assert!(
                began.elapsed() < Duration::from_secs(1),
                "Drop waited for the session timeout"
            );
            server.join().unwrap();
        }
    }

    // Skips cancellation retries and playback notices, which depend on client timing.
    fn skip_to(ws: &mut WebSocket<TcpStream>, kind: &str) -> Value {
        loop {
            let event: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            if event["type"] == kind {
                return event;
            }
            assert!(
                event["type"] == "response.cancel" || event["type"] == "conversation.item.create",
                "Expected {kind}, got {event}"
            );
        }
    }

    fn text_reply(ws: &mut WebSocket<TcpStream>, id: usize, text: &str, status: &str) {
        for event in [
            json!({"type":"response.created","response":{"id":id.to_string()}}),
            json!({"type":"response.output_text.delta","response_id":id.to_string(),"delta":text}),
            json!({"type":"response.done","response":{"id":id.to_string(),"status":status,
                "usage":{"output_tokens":8}}}),
        ] {
            ws.send(Message::Text(event.to_string().into())).unwrap();
        }
    }

    #[test]
    fn identical_replies_renew_context_at_the_next_input_boundary() {
        const REPLIES: [(&str, bool); 8] = [
            ("Maya!", false),
            ("Maya.", false),
            ("maya", false),
            ("You're very welcome! Have a great day.", false),
            ("you're VERY welcome, have a great day", false),
            ("Something else entirely", true),
            ("You're very welcome! Have a great day!", false),
            ("You're very welcome. Have a great day.", false),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            let mut ws = context_session(&listener);
            for (id, (text, cancelled)) in REPLIES.iter().enumerate() {
                skip_to(&mut ws, "input_audio_buffer.append");
                skip_to(&mut ws, "response.create");
                if *cancelled {
                    skip_to(&mut ws, "response.cancel");
                }
                let status = if *cancelled { "cancelled" } else { "completed" };
                text_reply(&mut ws, id, text, status);
            }
            loop {
                match ws.read().unwrap() {
                    Message::Close(_) => break,
                    Message::Text(text) => assert!(text.contains("response.cancel")),
                    _ => {}
                }
            }
            let mut ws = context_session(&listener);
            context_event(&mut ws, "input_audio_buffer.append");
            context_event(&mut ws, "response.create");
            text_reply(&mut ws, 0, "You're very welcome!", "completed");
            assert!(matches!(ws.read().unwrap(), Message::Close(_)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        let finish = |client: &mut Realtime, cancel: bool| {
            client.append_audio(&[1; 320]).unwrap();
            client.finish_audio().unwrap();
            if cancel {
                assert!(client.interrupt().unwrap());
            }
            loop {
                match next_context_output(client) {
                    Output::Text(_) => assert!(!cancel, "Cancelled text leaked"),
                    output => break output,
                }
            }
        };
        for (turn, (_, cancelled)) in REPLIES.iter().enumerate() {
            assert!(client.ready_for_audio().unwrap(), "Turn {turn} renewed");
            let expected = if *cancelled {
                Output::Interrupted
            } else {
                Output::Done
            };
            assert_eq!(finish(&mut client, *cancelled), expected);
        }
        assert!(
            !client.ready_for_audio().unwrap(),
            "A fourth identical reply must renew before the next turn"
        );
        assert_eq!(next_context_output(&mut client), Output::ContextCleared);
        assert!(client.ready_for_audio().unwrap());
        assert_eq!(finish(&mut client, false), Output::Done);
        assert!(
            client.ready_for_audio().unwrap(),
            "Renewal resets the count"
        );
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn only_consecutive_finished_identical_replies_count_as_a_loop() {
        let (addr, server) = mock_server(|mut ws| {
            assert!(matches!(ws.read().unwrap(), Message::Close(_)));
        });
        let mut client = Realtime::connect(&addr, &DEFAULT, &AtomicBool::new(false)).unwrap();
        for (text, cancelled, count) in [
            ("Goodbye, take care now.", false, 1),
            ("goodbye TAKE care now", false, 2),
            ("Your name is Sam.", false, 1),
            ("Your name is Sam!", false, 2),
            ("Your name", true, 2),
            ("Sam", false, 0),
            ("", false, 0),
            ("Your name is Sam.", false, 1),
            ("Your name is Sam.", false, 2),
            ("your name is sam", false, 3),
            ("Your name, is Sam?", false, 4),
        ] {
            let now = Instant::now();
            client.reply.clear();
            client.phase = if cancelled {
                Phase::Cancelling {
                    id: Some("current".into()),
                    since: now,
                    last_sent: now,
                }
            } else {
                Phase::Generating {
                    id: Some("current".into()),
                    since: now,
                }
            };
            client
                .process(json!({"type":"response.output_text.delta","response_id":"current","delta":text}))
                .unwrap();
            let status = if cancelled { "cancelled" } else { "completed" };
            client
                .process(
                    json!({"type":"response.done","response":{"id":"current","status":status}}),
                )
                .unwrap();
            assert_eq!(client.identical_replies, count, "After {text:?}");
            assert_eq!(client.can_admit(0), count < MAX_IDENTICAL_REPLIES);
        }
        drop(client);
        server.join().unwrap();
    }

    #[test]
    #[ignore = "Requires cached Liquid assets; uses no microphone or speakers"]
    fn liquid_backend_cancels_and_accepts_the_next_turn() {
        let cache = std::env::var_os("BOUBOU_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dirs::cache_dir().unwrap().join("boubou"));
        for profile in crate::profiles::PROFILES {
            let assets = crate::assets::prepare(profile, &cache, true).unwrap();
            let backend =
                crate::backend::Backend::start(&assets, &cache, &AtomicBool::new(false)).unwrap();
            let mut client =
                Realtime::connect(&backend.address, profile, &AtomicBool::new(false)).unwrap();
            client
                .text_turn("Count slowly from one to one hundred. Say every number.")
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                assert!(Instant::now() < deadline, "No first audio from Liquid");
                if let Some(Output::Audio(pcm)) = client.poll().unwrap() {
                    assert!(!pcm.is_empty());
                    break;
                }
                assert!(client.busy(), "Liquid completed without audio");
            }
            client.interrupt().unwrap();
            let cancel_started = Instant::now();
            while client.busy() {
                assert!(Instant::now() < deadline);
                if let Some(output) = client.poll().unwrap() {
                    assert_eq!(
                        output,
                        Output::Interrupted,
                        "Cancelled output leaked to the player"
                    );
                }
            }
            eprintln!(
                "{}: cancellation acknowledged in {:?}",
                profile.id,
                cancel_started.elapsed()
            );
            client.text_turn("Say only: ready.").unwrap();
            let mut samples = 0;
            let mut text = String::new();
            while client.busy() {
                assert!(Instant::now() < deadline);
                match client.poll().unwrap() {
                    Some(Output::Audio(pcm)) => samples += pcm.len(),
                    Some(Output::Text(delta)) => text.push_str(&delta),
                    _ => {}
                }
            }
            assert!(samples > 0, "Next turn produced no audio");
            assert!(
                text.to_lowercase().contains("ready"),
                "Unexpected next turn: {text}"
            );
            eprintln!("{}: next turn produced {samples} audio samples", profile.id);
        }
    }

    #[test]
    #[ignore = "Checks real Liquid context renewal beyond 32K tokens; no audio devices"]
    fn liquid_conversation_renews_beyond_context_capacity() {
        let cache = std::env::var_os("BOUBOU_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dirs::cache_dir().unwrap().join("boubou"));
        let assets = crate::assets::prepare(&DEFAULT, &cache, true).unwrap();
        let backend =
            crate::backend::Backend::start(&assets, &cache, &AtomicBool::new(false)).unwrap();
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let prompt = format!(
            "Remember this code word: mango. Privacy marker: BOUBOU_PRIVACY_SENTINEL. The following is padding:{}\nReply only with the code word.",
            " ocean".repeat(9000)
        );
        let tokens: Value = serde_json::from_str(
            &http
                .post(format!("http://{}/tokenize", backend.address))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(json!({"content": prompt, "add_special": false}).to_string())
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .unwrap(),
        )
        .unwrap();
        let token_count = tokens["tokens"].as_array().unwrap().len();
        assert!(
            token_count > 8192,
            "The regression must cross the old capacity"
        );
        assert!(token_count + 2048 < DEFAULT.context_tokens);
        let mut chat =
            Realtime::connect(&backend.address, &DEFAULT, &AtomicBool::new(false)).unwrap();
        let rounds = DEFAULT.context_tokens / token_count + 2;
        let mut renewals = 0;
        for _ in 0..rounds {
            let follow_up = "What was the code word? Reply only with that word.";
            // Keep each recall check within one session, even at the output cap.
            let pair_tokens =
                token_count + follow_up.len() + TURN_OVERHEAD_TOKENS + MAX_OUTPUT_TOKENS;
            while !chat.prepare_turn(pair_tokens).unwrap() {
                renewals +=
                    usize::from(matches!(chat.poll().unwrap(), Some(Output::ContextCleared)));
            }
            for (text, tokens) in [(prompt.as_str(), token_count), (follow_up, follow_up.len())] {
                assert!(!chat.text_turn_with_tokens(text, tokens).unwrap());
                let deadline = Instant::now() + Duration::from_secs(120);
                let mut reply = String::new();
                let mut audio_samples = 0;
                while chat.busy() {
                    assert!(Instant::now() < deadline, "Context regression timed out");
                    match chat.poll().unwrap() {
                        Some(Output::Text(text)) => reply.push_str(&text),
                        Some(Output::Audio(pcm)) => audio_samples += pcm.len(),
                        _ => {}
                    }
                }
                assert!(
                    reply.to_lowercase().contains("mango"),
                    "Lost conversation context: {reply}"
                );
                assert!(audio_samples > 0, "No speech across context renewal");
            }
        }
        assert!(renewals > 0, "No context renewal occurred");
        assert!(rounds * token_count > DEFAULT.context_tokens);
        assert!(
            http.get(format!("http://{}/health", backend.address))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        eprintln!(
            "CONTEXT_REGRESSION input_tokens={} context={} renewals={renewals} backend_alive=true",
            rounds * token_count,
            DEFAULT.context_tokens
        );
        drop(chat);
        drop(backend);
        let log = std::fs::read_to_string(
            cache
                .join("logs")
                .join(format!("backend-{}.log", std::process::id())),
        )
        .unwrap();
        assert!(
            !log.contains("BOUBOU_PRIVACY_SENTINEL"),
            "Backend log persisted input text"
        );
        assert!(
            !log.to_lowercase().contains("mango"),
            "Backend log persisted a conversation word"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(
                cache
                    .join("logs")
                    .join(format!("backend-{}.log", std::process::id())),
            )
            .unwrap();
            assert_eq!(
                metadata.permissions().mode() & 0o077,
                0,
                "Backend diagnostics must be private"
            );
        }
    }

    #[test]
    #[ignore = "Requires cached Liquid assets and macOS `say`; uses no microphone or speakers"]
    fn liquid_sustained_voice_conversation_keeps_replies_varied() {
        // Greedy decoding answered every turn of such a session with one repeated reply.
        const TURNS: [&str; 40] = [
            "Hi there, how are you doing today?",
            "My name is Sam, and I work as a nurse.",
            "Do you remember my name?",
            "What should I cook for dinner tonight?",
            "I don't really like spicy food.",
            "Okay.",
            "Tell me something interesting about octopuses.",
            "Wow. Tell me more.",
            "Yeah.",
            "What's your favorite season, and why?",
            "I think you keep starting every answer the same way.",
            "Let's play a game. Give me a riddle.",
            "Is it a shadow?",
            "I give up.",
            "What did I tell you I do for work?",
            "I'm feeling a bit tired today.",
            "Thanks.",
            "Sure.",
            "Can you recommend a book for the weekend?",
            "Why that one?",
            "Tell me a short joke.",
            "Another one, please.",
            "I've been thinking about AI and creativity lately.",
            "When I paint, the messy part is where the ideas come from.",
            "Yeah, exactly.",
            "Mm-hmm.",
            "Be honest with me. Do you have opinions?",
            "I had a job interview yesterday.",
            "It went okay, I guess.",
            "Right.",
            "My friend Maya is here with me now.",
            "Tell Maya a fun fact about space.",
            "Cool.",
            "She's laughing.",
            "What should we do this weekend?",
            "Board games could be fun.",
            "Can you summarize what we've talked about so far?",
            "What's my friend's name?",
            "Okay, I need to go now.",
            "Bye.",
        ];
        let directory = std::env::temp_dir().join(format!(
            "boubou-sustained-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let speech: Vec<Vec<i16>> = TURNS
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let path = directory.join(format!("{index}.wav"));
                let status = std::process::Command::new("say")
                    .args(["--file-format=WAVE", "--data-format=LEI16@16000", "-o"])
                    .arg(&path)
                    .arg(text)
                    .status()
                    .unwrap();
                assert!(status.success(), "Cannot synthesize: {text}");
                hound::WavReader::open(&path)
                    .unwrap()
                    .samples()
                    .collect::<std::result::Result<_, _>>()
                    .unwrap()
            })
            .collect();
        std::fs::remove_dir_all(&directory).unwrap();
        let cache = std::env::var_os("BOUBOU_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dirs::cache_dir().unwrap().join("boubou"));
        let assets = crate::assets::prepare(&DEFAULT, &cache, true).unwrap();
        let backend =
            crate::backend::Backend::start(&assets, &cache, &AtomicBool::new(false)).unwrap();
        let mut client =
            Realtime::connect(&backend.address, &DEFAULT, &AtomicBool::new(false)).unwrap();
        // Roughly 15-20 minutes of spoken conversation, enough to cross one context renewal.
        let turns = 3 * TURNS.len();
        let began = Instant::now();
        let (mut renewals, mut interruptions) = (0, 0);
        let mut replies = Vec::new();
        for (turn, pcm) in speech.iter().cycle().take(turns).enumerate() {
            while !client.ready_for_audio().unwrap() {
                renewals += usize::from(matches!(
                    client.poll().unwrap(),
                    Some(Output::ContextCleared)
                ));
            }
            for frame in pcm.chunks(320) {
                client.append_audio(frame).unwrap();
            }
            client.finish_audio().unwrap();
            let deadline = Instant::now() + Duration::from_secs(120);
            let (mut text, mut samples, mut cut) = (String::new(), 0, false);
            while client.busy() {
                assert!(Instant::now() < deadline, "Turn {turn} timed out");
                match client.poll().unwrap() {
                    Some(Output::Text(delta)) => text.push_str(&delta),
                    Some(Output::Audio(pcm)) => {
                        samples += pcm.len();
                        // Barge in on every third reply after about 1.5 s of speech.
                        if turn % 3 == 2 && samples >= 36_000 && !cut {
                            cut = client.interrupt().unwrap();
                        }
                    }
                    _ => {}
                }
            }
            if cut {
                interruptions += 1;
                continue;
            }
            assert!(samples > 0, "Turn {turn} produced no speech");
            let words: String = text
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace())
                .collect();
            replies.push(words.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        let (mut repeated, mut run, mut longest) = (0, 1, 1);
        for pair in replies.windows(2) {
            run = if pair[0] == pair[1] { run + 1 } else { 1 };
            repeated += usize::from(run > 1);
            longest = longest.max(run);
        }
        eprintln!(
            "SUSTAINED_CONVERSATION turns={} completed={} interruptions={interruptions} renewals={renewals} consecutive_repeats={repeated} longest_identical_run={longest} elapsed={:?}",
            turns,
            replies.len(),
            began.elapsed()
        );
        assert!(renewals > 0, "The session never crossed a context renewal");
        assert!(interruptions > 0, "No reply was interrupted");
        assert!(longest <= 2, "Replies looped: {replies:#?}");
        assert!(
            repeated * 10 < replies.len(),
            "Too many repeated replies: {replies:#?}"
        );
        assert!(
            reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://{}/health", backend.address))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
    }

    #[test]
    #[ignore = "Checks the same spoken question with microphone-sized and reference-sized chunks"]
    fn capture_chunks_match_reference_input() {
        let wav = std::env::var_os("BOUBOU_QUESTION_WAV").expect("Set BOUBOU_QUESTION_WAV");
        let mut wav = hound::WavReader::open(wav).unwrap();
        assert_eq!(wav.spec().sample_rate, 16_000);
        let pcm: Vec<i16> = wav
            .samples()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let cache = dirs::cache_dir().unwrap().join("boubou");
        let assets = crate::assets::prepare(&DEFAULT, &cache, true).unwrap();
        let backend =
            crate::backend::Backend::start(&assets, &cache, &AtomicBool::new(false)).unwrap();
        for size in [1600, 320] {
            let mut client =
                Realtime::connect(&backend.address, &DEFAULT, &AtomicBool::new(false)).unwrap();
            for chunk in pcm.chunks(size) {
                client.append_audio(chunk).unwrap();
            }
            client.finish_audio().unwrap();
            let mut text = String::new();
            while client.busy() {
                if let Some(Output::Text(delta)) = client.poll().unwrap() {
                    text.push_str(&delta);
                }
            }
            eprintln!("CHUNK_SAMPLES={size} REPLY={text}");
            assert!(
                text.to_lowercase().contains("blue"),
                "The same question was not preserved: {text}"
            );
        }
    }
}
