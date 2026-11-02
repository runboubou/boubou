use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::ErrorKind,
    net::TcpStream,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

const MAX_TURN_SAMPLES: usize = 16_000 * 30;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq)]
pub enum Output {
    Text(String),
    Audio(Vec<i16>),
    Done,
    Interrupted,
}

#[derive(Debug)]
enum Phase {
    Idle,
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
    socket: WebSocket<TcpStream>,
    phase: Phase,
    interrupted: bool,
    user_samples: usize,
    audio_samples: usize,
}

impl Realtime {
    pub fn connect(address: &str) -> Result<Self> {
        let stream =
            TcpStream::connect(address).context("Cannot connect to the local Liquid backend")?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let (socket, _) = tungstenite::client(format!("ws://{address}/v1/realtime"), stream)
            .context("Liquid backend does not support the realtime protocol")?;
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(10)))?;
        let mut this = Self {
            socket,
            phase: Phase::Idle,
            interrupted: false,
            user_samples: 0,
            audio_samples: 0,
        };
        let created = this.wait_for("session.created")?;
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
            "instructions":"Respond with interleaved text and audio.",
            "output_modalities":["text","audio"],
            "max_output_tokens":1024
        }}))?;
        this.wait_for("session.updated")?;
        Ok(this)
    }

    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    pub fn cancelling(&self) -> bool {
        matches!(self.phase, Phase::Cancelling { .. })
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
        self.begin_user()?;
        let bytes: Vec<u8> = samples.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.send(json!({"type":"input_audio_buffer.append","audio":STANDARD.encode(bytes)}))?;
        self.user_samples += samples.len();
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
        ensure!(!self.busy(), "The previous turn is still running");
        self.begin_user()?;
        self.send(json!({"type":"conversation.item.create","item":{
            "type":"message","role":"user","content":[{"type":"input_text","text":text}]
        }}))?;
        self.request_response()
    }

    fn request_response(&mut self) -> Result<()> {
        ensure!(!self.busy(), "The previous turn is still running");
        crate::trace(
            "response.create.send",
            format_args!("samples={}", self.user_samples),
        );
        self.send(json!({"type":"response.create"}))?;
        crate::trace("response.create.sent", format_args!(""));
        self.user_samples = 0;
        self.audio_samples = 0;
        self.phase = Phase::Generating {
            id: None,
            since: Instant::now(),
        };
        Ok(())
    }

    /// Call even if generation has ended but buffered audio is still playing.
    pub fn interrupt(&mut self) -> Result<()> {
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
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Option<Output>> {
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
            Phase::Idle => {}
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
                Phase::Idle => bail!("Received an unsolicited Liquid response"),
            }
            return Ok(None);
        }
        let id = event["response_id"]
            .as_str()
            .or_else(|| event["response"]["id"].as_str());
        let expected = match &self.phase {
            Phase::Generating { id, .. } | Phase::Cancelling { id, .. } => id.as_deref(),
            Phase::Idle => None,
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
            crate::trace(
                "response.done",
                format_args!(
                    "id={id:?} status={status} cancelling={cancelled} audio_samples={}",
                    self.audio_samples
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
            "response.output_text.delta" => Ok(Some(Output::Text(
                event["delta"]
                    .as_str()
                    .context("Invalid text delta")?
                    .to_owned(),
            ))),
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
                        .chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]]))
                        .collect(),
                )))
            }
            _ => Ok(None),
        }
    }

    fn wait_for(&mut self, kind: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
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
        match self.socket.read() {
            Ok(Message::Text(text)) => Ok(Some(serde_json::from_str(&text)?)),
            Ok(Message::Ping(_)) => {
                self.socket.flush()?;
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
            .send(Message::Text(value.to_string().into()))
            .context("Cannot send to Liquid")
    }
}

impl Drop for Realtime {
    fn drop(&mut self) {
        let _ = self.socket.close(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn mock_server(work: impl FnOnce(WebSocket<TcpStream>) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            ws.send(Message::Text(
                json!({"type":"session.created","session":{"audio":{
                    "input":{"format":{"rate":16000}},"output":{"format":{"rate":24000}}
                }}})
                .to_string()
                .into(),
            ))
            .unwrap();
            let _ = ws.read().unwrap();
            ws.send(Message::Text(
                json!({"type":"session.updated"}).to_string().into(),
            ))
            .unwrap();
            work(ws);
        });
        address
    }

    #[test]
    fn interrupt_before_created_discards_late_audio_and_waits_for_done() {
        let addr = mock_server(|mut ws| {
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
        let mut client = Realtime::connect(&addr).unwrap();
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
    }

    #[test]
    fn ending_partial_input_does_not_request_a_reply() {
        let (done, result) = std::sync::mpsc::channel();
        let addr = mock_server(move |mut ws| {
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
            done.send(()).unwrap();
        });
        let mut client = Realtime::connect(&addr).unwrap();
        client.append_audio(&[0; 320]).unwrap();
        client.end_input_without_response().unwrap();
        client.end_input_without_response().unwrap();
        assert!(client.finish_audio().is_err());
        assert!(!client.busy());
        result.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn odd_pcm_delta_is_rejected() {
        let addr = mock_server(|_| thread::sleep(Duration::from_millis(200)));
        let mut client = Realtime::connect(&addr).unwrap();
        client.phase = Phase::Generating {
            id: Some("x".into()),
            since: Instant::now(),
        };
        assert!(client.process(json!({"type":"response.output_audio.delta","response_id":"x","delta":STANDARD.encode([1])})).is_err());
    }

    #[test]
    #[ignore = "Requires cached Liquid assets; uses no microphone or speakers"]
    fn liquid_backend_cancels_and_accepts_the_next_turn() {
        let cache = std::env::var_os("BOUBOU_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dirs::cache_dir().unwrap().join("boubou"));
        let assets = crate::assets::prepare(&cache, true).unwrap();
        let backend = crate::backend::Backend::start(&assets, &cache).unwrap();
        let mut client = Realtime::connect(&backend.address).unwrap();
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
            "Cancellation acknowledged in {:?}",
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
        eprintln!("Next turn: {text}; {} audio samples", samples);
    }

    #[test]
    #[ignore = "Checks real Liquid context beyond the former limit; no audio devices"]
    fn liquid_conversation_exceeds_the_previous_context_limit() {
        let cache = dirs::cache_dir().unwrap().join("boubou");
        let assets = crate::assets::prepare(&cache, true).unwrap();
        let backend = crate::backend::Backend::start(&assets, &cache).unwrap();
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let prompt = format!(
            "Remember this code word: mango. The following is padding:{}\nReply only with the code word.",
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
        assert!(token_count + 2048 < crate::backend::CONTEXT_TOKENS as usize);
        let mut chat = Realtime::connect(&backend.address).unwrap();
        for text in [
            &prompt,
            "What was the code word? Reply only with that word.",
        ] {
            chat.text_turn(text).unwrap();
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
            assert!(
                audio_samples > 0,
                "No speech after crossing the old context limit"
            );
            eprintln!("CONTEXT_REPLY text={reply:?} audio_samples={audio_samples}");
        }
        assert!(
            http.get(format!("http://{}/health", backend.address))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        eprintln!(
            "CONTEXT_REGRESSION input_tokens={token_count} context={} turns=2 backend_alive=true",
            crate::backend::CONTEXT_TOKENS
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
        let assets = crate::assets::prepare(&cache, true).unwrap();
        let backend = crate::backend::Backend::start(&assets, &cache).unwrap();
        for size in [1600, 320] {
            let mut client = Realtime::connect(&backend.address).unwrap();
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
