use crate::{Conversation, assets::Assets, backend::Backend, realtime::Output};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Cursor, Read},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const MAX_TURN_SAMPLES: usize = 30 * 16_000;
const SESSION_TOKENS: usize = 512;
const TURN_TOKENS: usize = 256;
const MAX_EVENT_BYTES: usize = 256 * 1024;

enum Phase {
    Idle,
    Generating {
        worker: JoinHandle<Result<()>>,
        output: Receiver<Output>,
    },
    Reloading {
        worker: JoinHandle<Result<Backend>>,
        stopped: Arc<AtomicBool>,
    },
}

pub struct Liquid {
    assets: Assets,
    cache: PathBuf,
    backend: Option<Backend>,
    phase: Phase,
    input: Vec<i16>,
    context_tokens: usize,
    reset_context: bool,
    context_notice: bool,
}

impl Liquid {
    pub fn new(assets: Assets, cache: PathBuf, backend: Backend) -> Self {
        Self {
            assets,
            cache,
            backend: Some(backend),
            phase: Phase::Idle,
            input: Vec::new(),
            context_tokens: SESSION_TOKENS,
            reset_context: true,
            context_notice: false,
        }
    }

    fn reset(&mut self) {
        self.context_tokens = SESSION_TOKENS;
        self.reset_context = true;
        self.context_notice = true;
    }

    fn can_admit(&self) -> bool {
        let profile = self.assets.profile;
        self.context_tokens
            + MAX_TURN_SAMPLES.div_ceil(profile.audio_samples_per_token)
            + TURN_TOKENS
            + profile.max_output_tokens
            < profile.context_tokens
    }
}

impl Conversation for Liquid {
    fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    fn ready_for_audio(&mut self) -> Result<bool> {
        if self.busy() || self.context_notice {
            return Ok(false);
        }
        if !self.input.is_empty() || self.can_admit() {
            return Ok(true);
        }
        self.reset();
        ensure!(
            self.can_admit(),
            "A single turn exceeds Liquid's context capacity"
        );
        Ok(false)
    }

    fn append_audio(&mut self, samples: &[i16]) -> Result<()> {
        ensure!(
            !self.busy(),
            "Wait for the previous response to stop before sending a new turn"
        );
        ensure!(
            self.input.len() + samples.len() <= MAX_TURN_SAMPLES,
            "A voice turn exceeded 30 seconds"
        );
        ensure!(
            !self.input.is_empty() || self.can_admit(),
            "Wait for automatic context renewal before sending a new turn"
        );
        self.input.extend_from_slice(samples);
        Ok(())
    }

    fn finish_audio(&mut self) -> Result<()> {
        ensure!(!self.busy(), "The previous turn is still running");
        ensure!(!self.input.is_empty(), "Cannot submit an empty audio turn");
        let address = self
            .backend
            .as_ref()
            .context("Liquid backend is missing")?
            .address
            .clone();
        let profile = self.assets.profile;
        let reset_context = self.reset_context;
        let pcm = std::mem::take(&mut self.input);
        self.context_tokens += pcm.len().div_ceil(profile.audio_samples_per_token)
            + TURN_TOKENS
            + profile.max_output_tokens;
        self.reset_context = false;
        let (sender, output) = mpsc::sync_channel(8);
        let worker = thread::Builder::new()
            .name("boubou-liquid-response".into())
            .spawn(move || {
                let body = request(&pcm, profile, reset_context)?;
                let client = reqwest::blocking::Client::builder()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(1))
                    .timeout(Duration::from_secs(120))
                    .build()?;
                let response = client
                    .post(format!("http://{address}/v1/chat/completions"))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.to_string())
                    .send()
                    .context("Cannot submit the Liquid voice turn")?;
                ensure!(
                    response.status().is_success(),
                    "Liquid voice request failed: HTTP {}",
                    response.status()
                );
                ensure!(
                    response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.split(';').next() == Some("text/event-stream")),
                    "Liquid returned a response that is not an SSE stream"
                );
                read_stream(BufReader::new(response), |output| {
                    sender.send(output).map_err(|_| crate::Cancelled.into())
                })
            })
            .context("Cannot start the Liquid response worker")?;
        self.phase = Phase::Generating { worker, output };
        Ok(())
    }

    fn interrupt(&mut self) -> Result<bool> {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Reloading { worker, stopped } => {
                self.phase = Phase::Reloading { worker, stopped };
                return Ok(false);
            }
            Phase::Generating { worker, output } => {
                // Drop queued output before stopping the process; no stale event can be polled.
                drop(output);
                self.backend
                    .as_mut()
                    .context("Liquid backend is missing")?
                    .stop()?;
                self.backend.take();
                let assets = self.assets.clone();
                let cache = self.cache.clone();
                let stopped = Arc::new(AtomicBool::new(false));
                let signal = stopped.clone();
                let worker = thread::Builder::new()
                    .name("boubou-liquid-reload".into())
                    .spawn(move || {
                        let _ = worker.join();
                        Backend::start(&assets, &cache, &signal)
                            .context("Cannot reload Liquid after interruption")
                    })?;
                self.phase = Phase::Reloading { worker, stopped };
            }
            Phase::Idle => {}
        }
        self.reset();
        Ok(true)
    }

    fn poll(&mut self) -> Result<Option<Output>> {
        if self.context_notice {
            self.context_notice = false;
            return Ok(Some(Output::ContextCleared));
        }
        match &self.phase {
            Phase::Reloading { worker, .. } => {
                if !worker.is_finished() {
                    thread::sleep(Duration::from_millis(10));
                    return Ok(None);
                }
                let Phase::Reloading { worker, .. } =
                    std::mem::replace(&mut self.phase, Phase::Idle)
                else {
                    unreachable!()
                };
                self.backend = Some(
                    worker
                        .join()
                        .map_err(|_| anyhow!("Liquid reload worker failed"))??,
                );
                Ok(Some(Output::Interrupted))
            }
            Phase::Generating { output, .. } => {
                match output.recv_timeout(Duration::from_millis(10)) {
                    Ok(output) => Ok(Some(output)),
                    Err(RecvTimeoutError::Timeout) => Ok(None),
                    Err(RecvTimeoutError::Disconnected) => {
                        let Phase::Generating { worker, .. } =
                            std::mem::replace(&mut self.phase, Phase::Idle)
                        else {
                            unreachable!()
                        };
                        worker
                            .join()
                            .map_err(|_| anyhow!("Liquid response worker failed"))??;
                        Ok(None)
                    }
                }
            }
            Phase::Idle => {
                thread::sleep(Duration::from_millis(10));
                Ok(None)
            }
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn end_input_without_response(&mut self) -> Result<()> {
        self.input.clear();
        Ok(())
    }
}

impl Drop for Liquid {
    fn drop(&mut self) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        // Closing the owned process also releases a worker blocked in an HTTP read.
        self.backend.take();
        match phase {
            Phase::Generating { worker, output } => {
                drop(output);
                let _ = worker.join();
            }
            Phase::Reloading { worker, stopped } => {
                stopped.store(true, Ordering::SeqCst);
                let _ = worker.join();
            }
            Phase::Idle => {}
        }
    }
}

fn request(pcm: &[i16], profile: &crate::profiles::Profile, reset_context: bool) -> Result<Value> {
    let mut wav = Cursor::new(Vec::with_capacity(44 + pcm.len() * 2));
    let mut writer = hound::WavWriter::new(
        &mut wav,
        hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;
    for &sample in pcm {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    let mut messages = Vec::new();
    if reset_context {
        messages.push(json!({"role":"system", "content":profile.instructions}));
    }
    messages.push(
        json!({"role":"user", "content":[{"type":"input_audio", "input_audio":{
            "data":STANDARD.encode(wav.into_inner()), "format":"wav"
        }}]}),
    );
    Ok(
        json!({"messages":messages, "stream":true, "reset_context":reset_context,
        "modalities":["text","audio"], "max_tokens":profile.max_output_tokens}),
    )
}

fn read_stream(mut reader: impl BufRead, mut emit: impl FnMut(Output) -> Result<()>) -> Result<()> {
    let mut line = Vec::new();
    let mut data = String::new();
    let mut finished = false;
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take((MAX_EVENT_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .context("Liquid SSE stream was interrupted")?;
        ensure!(n > 0, "Liquid SSE stream ended before [DONE]");
        ensure!(
            n <= MAX_EVENT_BYTES && line.ends_with(b"\n"),
            "Invalid or oversized Liquid SSE line"
        );
        let line = std::str::from_utf8(&line)
            .context("Liquid SSE stream contains invalid UTF-8")?
            .trim_end_matches(['\r', '\n']);
        if !line.is_empty() {
            if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                ensure!(
                    data.len() <= MAX_EVENT_BYTES,
                    "Liquid SSE event exceeded its size limit"
                );
            }
            continue;
        }
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            ensure!(
                finished,
                "Liquid ended the SSE stream without a finish event"
            );
            emit(Output::Done)?;
            return Ok(());
        }
        let event: Value =
            serde_json::from_str(&data).context("Liquid returned malformed SSE JSON")?;
        data.clear();
        if let Some(error) = event.get("error") {
            bail!("Liquid: {}", error["message"]);
        }
        ensure!(!finished, "Liquid sent output after its finish event");
        let choices = event["choices"]
            .as_array()
            .context("Liquid SSE event is missing choices")?;
        ensure!(
            choices.len() == 1 && choices[0]["index"] == 0,
            "Invalid Liquid SSE choices"
        );
        let choice = &choices[0];
        if !choice["finish_reason"].is_null() {
            ensure!(
                choice["finish_reason"] == "stop",
                "Liquid generation failed: {}",
                choice["finish_reason"]
            );
            finished = true;
        }
        let delta = choice["delta"]
            .as_object()
            .context("Invalid Liquid SSE delta")?;
        ensure!(
            delta
                .keys()
                .all(|key| matches!(key.as_str(), "content" | "audio")),
            "Liquid returned an unsupported SSE delta"
        );
        if let Some(text) = delta.get("content") {
            emit(Output::Text(
                text.as_str()
                    .context("Invalid Liquid text delta")?
                    .to_owned(),
            ))?;
        }
        if let Some(audio) = delta.get("audio") {
            ensure!(
                audio["format"] == "pcm" && audio["sample_rate"] == 24_000,
                "Liquid audio must be PCM16 mono at 24 kHz"
            );
            let bytes = STANDARD
                .decode(
                    audio["data"]
                        .as_str()
                        .context("Invalid Liquid audio delta")?,
                )
                .context("Liquid returned invalid base64 audio")?;
            ensure!(
                bytes.len() % 2 == 0,
                "Liquid returned an incomplete PCM16 sample"
            );
            emit(Output::Audio(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .copied()
                    .map(i16::from_le_bytes)
                    .collect(),
            ))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{LFM25, LFM25_JP};
    use std::{fs, io, time::Instant};

    const END: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    fn event(delta: Value) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
        )
    }

    fn parse(data: impl Read) -> Result<Vec<Output>> {
        let mut outputs = Vec::new();
        read_stream(BufReader::new(data), |output| {
            outputs.push(output);
            Ok(())
        })?;
        Ok(outputs)
    }

    #[test]
    fn sse_handles_split_utf8_crlf_comments_text_and_pcm() {
        struct Bytes<'a>(&'a [u8]);
        impl Read for Bytes<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let count = out.len().min(1).min(self.0.len());
                out[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Ok(count)
            }
        }
        let data = format!(": heartbeat\r\n\r\n{}{}{}",
            event(json!({"content":"日本語"})),
            event(json!({"audio":{"data":STANDARD.encode([1,0,255,255]),"format":"pcm","sample_rate":24000}})), END).replace('\n', "\r\n");
        assert_eq!(
            parse(Bytes(data.as_bytes())).unwrap(),
            vec![
                Output::Text("日本語".into()),
                Output::Audio(vec![1, -1]),
                Output::Done
            ]
        );
        let multiline = format!(
            "data: {{\"choices\":\ndata: [{{\"index\":0,\"delta\":{{\"content\":\"ok\"}}}}]}}\n\n{END}"
        );
        assert_eq!(
            parse(multiline.as_bytes()).unwrap(),
            vec![Output::Text("ok".into()), Output::Done]
        );
    }

    #[test]
    fn sse_rejects_premature_eof_missing_finish_and_malformed_events() {
        for data in [
            "".to_owned(),
            "data: {".into(),
            event(json!({"content":"partial"})),
            END.replace("data: [DONE]\n\n", ""),
            "data: [DONE]\n\n".into(),
            "data: not-json\n\n".into(),
            "data: {}\n\n".into(),
            "data: {\"error\":{\"message\":\"prefill failed\"}}\n\n".into(),
            format!("data: {}\n\n", "x".repeat(MAX_EVENT_BYTES + 1)),
        ] {
            assert!(parse(data.as_bytes()).is_err(), "Accepted invalid stream");
        }
    }

    #[test]
    fn sse_rejects_invalid_audio_and_unsuccessful_finishes() {
        assert!(parse(event(json!({"audio_chunk":{"data":"AAAAAA=="}})).as_bytes()).is_err());
        for audio in [
            json!({"data":"!","format":"pcm","sample_rate":24000}),
            json!({"data":"AA==","format":"pcm","sample_rate":24000}),
            json!({"data":"AAA=","format":"wav","sample_rate":24000}),
            json!({"data":"AAA=","format":"pcm","sample_rate":16000}),
        ] {
            assert!(parse(format!("{}{END}", event(json!({"audio":audio}))).as_bytes()).is_err());
        }
        assert!(parse(END.replace("stop", "error").as_bytes()).is_err());
    }

    #[test]
    fn requests_keep_the_matching_prompt_only_on_reset_and_encode_real_wav() {
        for profile in [&LFM25, &LFM25_JP] {
            assert_eq!(
                profile.instructions,
                "Respond with interleaved text and audio."
            );
            for reset in [true, false] {
                let body = request(&[123, -234], profile, reset).unwrap();
                assert_eq!(body["reset_context"], reset);
                assert_eq!(body["max_tokens"], profile.max_output_tokens);
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), if reset { 2 } else { 1 });
                if reset {
                    assert_eq!(messages[0]["content"], profile.instructions);
                }
                let audio = &messages.last().unwrap()["content"][0]["input_audio"];
                assert_eq!(audio["format"], "wav");
                let bytes = STANDARD.decode(audio["data"].as_str().unwrap()).unwrap();
                let mut wav = hound::WavReader::new(Cursor::new(bytes)).unwrap();
                assert_eq!(wav.spec().sample_rate, 16000);
                assert_eq!(wav.spec().channels, 1);
                assert_eq!(
                    wav.samples::<i16>().collect::<Result<Vec<_>, _>>().unwrap(),
                    vec![123, -234]
                );
            }
        }
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "boubou-liquid-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        #[cfg(unix)]
        fn assets(&self) -> Assets {
            use std::os::unix::fs::PermissionsExt;
            let script = self.0.join("server");
            // An owned HTTP process verifies termination even when generation emits no callbacks.
            fs::write(&script, r##"#!/usr/bin/env python3
import http.server, json, pathlib, sys, time, os
root = pathlib.Path(__file__).parent
port = int(sys.argv[sys.argv.index('--port') + 1])
with (root/'pids').open('a') as f: f.write(str(os.getpid())+'\n')
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *args): pass
    def do_GET(self):
        self.send_response(404); self.send_header('Content-Length','0'); self.end_headers()
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with (root/'requests').open('a') as f:
            f.write(json.dumps([body['reset_context'],len(body['messages'])])+'\n')
        print('Reply content must never reach a runner log',flush=True)
        mode = (root/'mode').read_text()
        if mode == 'error':
            self.send_response(500); self.send_header('Content-Length','0'); self.end_headers(); return
        self.send_response(200); self.send_header('Content-Type','text/event-stream')
        self.send_header('Transfer-Encoding','chunked'); self.end_headers()
        def send(delta):
            frame = ('data: '+json.dumps({'choices':[{'index':0,'delta':delta,'finish_reason':None}]})+'\n\n').encode()
            self.wfile.write(('%x\r\n'%len(frame)).encode()+frame+b'\r\n'); self.wfile.flush()
        if mode == 'hold': time.sleep(30)
        send({'content':'fresh'})
        send({'audio':{'data':'AQD//w==','format':'pcm','sample_rate':24000}})
        if mode == 'stale':
            for i in range(30): send({'content':'stale'})
            time.sleep(30)
        if mode != 'truncated':
            frame = b'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
            self.wfile.write(('%x\r\n'%len(frame)).encode()+frame+b'\r\n')
        self.wfile.write(b'0\r\n\r\n'); self.wfile.flush()
http.server.HTTPServer(('127.0.0.1',port),Handler).serve_forever()
"##).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            Assets {
                profile: &LFM25,
                server: script,
                model: PathBuf::new(),
                projector: PathBuf::new(),
            }
        }
        fn mode(&self, mode: &str) {
            fs::write(self.0.join("mode"), mode).unwrap();
        }
        fn requests(&self) -> Vec<Value> {
            fs::read_to_string(self.0.join("requests"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn drain(chat: &mut Liquid) -> Result<Vec<Output>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outputs = Vec::new();
        while chat.busy() || chat.context_notice {
            assert!(Instant::now() < deadline, "Liquid fixture stalled");
            if let Some(output) = chat.poll()? {
                outputs.push(output);
            }
        }
        Ok(outputs)
    }

    #[cfg(unix)]
    #[test]
    fn owned_http_session_continues_resets_and_survives_interruption_without_stale_output() {
        let fixture = Fixture::new();
        fixture.mode("complete");
        let assets = fixture.assets();
        let backend = Backend::start(&assets, &fixture.0, &AtomicBool::new(false)).unwrap();
        let mut chat = Liquid::new(assets, fixture.0.clone(), backend);
        for _ in 0..2 {
            chat.append_audio(&[1; 320]).unwrap();
            chat.finish_audio().unwrap();
            assert_eq!(
                drain(&mut chat).unwrap(),
                vec![
                    Output::Text("fresh".into()),
                    Output::Audio(vec![1, -1]),
                    Output::Done
                ]
            );
        }
        assert_eq!(
            fixture.requests(),
            vec![json!([true, 2]), json!([false, 1])]
        );
        chat.append_audio(&[1; 320]).unwrap();
        chat.end_input_without_response().unwrap();
        assert!(chat.finish_audio().is_err());
        chat.context_tokens = LFM25.context_tokens;
        assert!(!chat.ready_for_audio().unwrap());
        assert_eq!(chat.poll().unwrap(), Some(Output::ContextCleared));
        assert!(chat.ready_for_audio().unwrap());
        for mode in ["stale", "hold"] {
            fixture.mode(mode);
            chat.append_audio(&[1; 320]).unwrap();
            chat.finish_audio().unwrap();
            if mode == "stale" {
                loop {
                    if matches!(chat.poll().unwrap(), Some(Output::Audio(_))) {
                        break;
                    }
                }
            } else {
                let deadline = Instant::now() + Duration::from_secs(3);
                while fixture.requests().len() < 5 {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(10));
                }
            }
            let began = Instant::now();
            chat.interrupt().unwrap();
            assert!(began.elapsed() < Duration::from_secs(1));
            fixture.mode("complete");
            let mut pending = crate::voice::PendingAudio::default();
            pending
                .push(crate::voice::Segment::Start(vec![2; 320]))
                .unwrap();
            pending
                .push(crate::voice::Segment::End(vec![3; 320]))
                .unwrap();
            crate::send_pending(&mut pending, &mut chat, true).unwrap();
            let outputs = drain(&mut chat).unwrap();
            assert_eq!(outputs, vec![Output::ContextCleared, Output::Interrupted]);
            crate::send_pending(&mut pending, &mut chat, true).unwrap();
            assert!(drain(&mut chat).unwrap().contains(&Output::Done));
            assert_eq!(fixture.requests().last().unwrap(), &json!([true, 2]));
        }
        for mode in ["error", "truncated"] {
            fixture.mode(mode);
            chat.append_audio(&[1; 320]).unwrap();
            chat.finish_audio().unwrap();
            assert!(drain(&mut chat).is_err());
        }
        assert!(!fixture.0.join("logs").exists(), "Runner output was saved");
        drop(chat);
        for pid in fs::read_to_string(fixture.0.join("pids")).unwrap().lines() {
            assert_eq!(
                unsafe { libc::kill(pid.parse().unwrap(), 0) },
                -1,
                "Owned backend survived cleanup"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Requires cached English/JP assets and macOS say; no microphone or speakers"]
    fn official_models_handle_multiturn_cancellation_and_offline_restart() {
        use std::{net::TcpStream, process::Command};
        let fixture = Fixture::new();
        let cache = std::env::var_os("BOUBOU_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::cache_dir().unwrap().join("boubou"));
        let mut memory_failures = Vec::new();
        for (profile, voice, utterances, names) in [
            (
                &LFM25,
                "Samantha",
                [
                    "The secret word is pineapple. Please repeat it.",
                    "What is the secret word?",
                ],
                &["pineapple"][..],
            ),
            (
                &LFM25_JP,
                "Kyoko",
                [
                    "合言葉はパイナップルです。合言葉を覚えて、繰り返してください。",
                    "合言葉は何ですか。",
                ],
                &["パイナップル", "ぱいなっぷる"][..],
            ),
        ] {
            let mut inputs = Vec::new();
            for (index, utterance) in utterances.iter().enumerate() {
                let path = fixture.0.join(format!("{index}.wav"));
                assert!(
                    Command::new("say")
                        .args([
                            "-v",
                            voice,
                            "--file-format=WAVE",
                            "--data-format=LEI16@16000",
                            "-o"
                        ])
                        .arg(&path)
                        .arg(utterance)
                        .status()
                        .unwrap()
                        .success()
                );
                inputs.push(crate::read_wav(&path).unwrap());
            }
            let assets = crate::assets::prepare(profile, &cache, true).unwrap();
            let backend = Backend::start(&assets, &cache, &AtomicBool::new(false)).unwrap();
            let mut chat = Liquid::new(assets, cache.clone(), backend);
            let finish = |chat: &mut Liquid, pcm: &[i16]| {
                while !chat.ready_for_audio().unwrap() {
                    chat.poll().unwrap();
                }
                chat.append_audio(pcm).unwrap();
                chat.finish_audio().unwrap();
                let deadline = Instant::now() + Duration::from_secs(120);
                let mut text = String::new();
                let mut samples = 0;
                while chat.busy() {
                    assert!(Instant::now() < deadline, "Real Liquid response stalled");
                    match chat.poll().unwrap() {
                        Some(Output::Text(delta)) => text.push_str(&delta),
                        Some(Output::Audio(pcm)) => samples += pcm.len(),
                        _ => {}
                    }
                }
                assert!(samples > 0, "Real model returned no audio");
                eprintln!(
                    "{}: {} audio samples; synthetic reply: {text}",
                    profile.id, samples
                );
                text
            };
            let first = finish(&mut chat, &inputs[0]);
            if !names.iter().any(|name| first.to_lowercase().contains(name)) {
                memory_failures.push(format!("{}: first-turn keyword recognition", profile.id));
            }
            let recall = finish(&mut chat, &inputs[1]);
            // A concrete keyword avoids ambiguous speaker identity in the ASR/model response.
            if !names
                .iter()
                .any(|name| recall.to_lowercase().contains(name))
            {
                memory_failures.push(format!("{}: audio conversation continuation", profile.id));
            }
            chat.append_audio(&inputs[0]).unwrap();
            chat.finish_audio().unwrap();
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                assert!(Instant::now() < deadline);
                if matches!(chat.poll().unwrap(), Some(Output::Audio(_))) {
                    break;
                }
            }
            let began = Instant::now();
            chat.interrupt().unwrap();
            assert!(began.elapsed() < Duration::from_secs(1));
            while chat.busy() {
                assert!(Instant::now() < deadline);
                assert!(
                    matches!(
                        chat.poll().unwrap(),
                        None | Some(Output::ContextCleared | Output::Interrupted)
                    ),
                    "Stale output survived interruption"
                );
            }
            eprintln!("{}: cancellation and reload passed", profile.id);
            finish(&mut chat, &inputs[0]);
            let address = chat.backend.as_ref().unwrap().address.parse().unwrap();
            drop(chat);
            assert!(TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err());
            let assets = crate::assets::prepare(profile, &cache, true).unwrap();
            let backend = Backend::start(&assets, &cache, &AtomicBool::new(false)).unwrap();
            let mut chat = Liquid::new(assets, cache.clone(), backend);
            finish(&mut chat, &inputs[0]);
            let address = chat.backend.as_ref().unwrap().address.parse().unwrap();
            drop(chat);
            assert!(TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err());
            eprintln!(
                "{}: next turn, offline restart and process cleanup passed",
                profile.id
            );
        }
        assert!(
            memory_failures.is_empty(),
            "Real model acceptance failed: {memory_failures:?}"
        );
    }
}
