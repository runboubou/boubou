use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const BYTE_LIMIT: usize = 256 * 1024 * 1024;
const QUEUE_CAPACITY: usize = 64;
type Wav = hound::WavWriter<BufWriter<File>>;

enum Event {
    Input { turn: u64, pcm: Vec<i16>, end: bool },
    Audio(Vec<i16>),
    Note(Value),
}

/// Explicitly enabled local evidence, not a transcript or a playback recording.
pub struct Recording {
    sender: Option<SyncSender<(u128, Event)>>,
    worker: Option<JoinHandle<Result<()>>>,
    error: Arc<Mutex<Option<String>>>,
    started: Instant,
}

impl Recording {
    pub fn start(path: &Path, metadata: Value) -> Result<Self> {
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(path).with_context(|| {
            format!(
                "Recording needs a NEW directory with an existing parent: {}",
                path.display()
            )
        })?;
        let mut writer = Journal {
            directory: path.to_owned(),
            events: BufWriter::new(private_file(&path.join("events.jsonl"))?),
            input: None,
            output: None,
            reply_turn: None,
            context: 0,
            remaining: BYTE_LIMIT,
        };
        writer.note(
            0,
            json!({
                "event":"session.start", "schema":1, "metadata":metadata,
                "audio":"input sent to model and generated output; not audio heard by user",
                "byte_limit":BYTE_LIMIT
            }),
        )?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let error = Arc::new(Mutex::new(None));
        let failure = error.clone();
        let worker = thread::Builder::new()
            .name("boubou-recording".into())
            .spawn(move || {
                let result = (|| {
                    for (ms, event) in receiver {
                        writer.process(ms, event)?;
                    }
                    Ok(())
                })();
                let finalized = writer.finish();
                let result: Result<()> = result.and(finalized);
                if let Err(reason) = &result {
                    *failure.lock().expect("recording error lock") = Some(format!("{reason:#}"));
                }
                result
            })
            .context("Cannot start the recording writer")?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            error,
            started: Instant::now(),
        })
    }

    pub fn check_error(&self) -> Result<()> {
        if let Some(error) = self.error.lock().expect("recording error lock").as_ref() {
            return Err(anyhow!("Session recording failed: {error}"));
        }
        Ok(())
    }

    fn send(&self, event: Event) -> Result<()> {
        self.check_error()?;
        match &event {
            Event::Input { pcm, .. } | Event::Audio(pcm) => {
                ensure!(pcm.len() <= 24_000 * 30, "Recording packet is too large");
            }
            Event::Note(value) => {
                ensure!(
                    serde_json::to_vec(value)?.len() <= 65_536,
                    "Recording event is too large"
                );
            }
        }
        self.sender
            .as_ref()
            .context("Session recording is closed")?
            .try_send((self.started.elapsed().as_millis(), event))
            .map_err(|_| {
                anyhow!("Session recording stopped or fell behind; no data was silently dropped")
            })
    }

    pub fn input(&self, turn: u64, pcm: &[i16], end: bool) -> Result<()> {
        self.send(Event::Input {
            turn,
            pcm: pcm.to_vec(),
            end,
        })
    }

    pub fn output(&self, output: &crate::realtime::Output) -> Result<()> {
        use crate::realtime::Output;
        match output {
            Output::Text(text) => self.note(json!({"event":"assistant.text", "text":text})),
            Output::Audio(pcm) => self.send(Event::Audio(pcm.clone())),
            Output::Done => self.note(json!({"event":"response.done", "status":"completed"})),
            Output::Interrupted => {
                self.note(json!({"event":"response.done", "status":"cancelled"}))
            }
            Output::ContextCleared => self.note(json!({"event":"context.cleared"})),
        }
    }

    pub fn note(&self, value: Value) -> Result<()> {
        self.send(Event::Note(value))
    }

    pub fn finish(mut self, status: &str) -> Result<()> {
        let recorded = self.note(json!({"event":"session.end", "status":status}));
        let finalized = self.close();
        recorded.and(finalized)
    }

    fn close(&mut self) -> Result<()> {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker.is_finished() {
                ensure!(
                    Instant::now() < deadline,
                    "Recording writer did not stop within 2 seconds; files may be incomplete"
                );
                thread::sleep(Duration::from_millis(5));
            }
            worker
                .join()
                .map_err(|_| anyhow!("Session recording writer panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("Cannot create {}", path.display()))
}

struct Journal {
    directory: PathBuf,
    events: BufWriter<File>,
    input: Option<(u64, Wav)>,
    output: Option<Wav>,
    reply_turn: Option<u64>,
    context: u64,
    remaining: usize,
}

impl Journal {
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .context("Session recording reached its 256 MiB budget; start a new recording")?;
        Ok(())
    }

    fn note(&mut self, ms: u128, mut event: Value) -> Result<()> {
        event["elapsed_ms"] = json!(ms);
        event["context"] = json!(self.context);
        event["reply_turn"] = json!(self.reply_turn);
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        self.reserve(bytes.len())?;
        self.events.write_all(&bytes)?;
        self.events.flush()?;
        Ok(())
    }

    fn wav(&mut self, ms: u128, role: &str, turn: u64, rate: u32) -> Result<Wav> {
        // Reserve more than the WAV header so the total file budget stays conservative.
        self.reserve(128)?;
        let name = format!("{role}-{turn:06}.wav");
        let file = private_file(&self.directory.join(&name))?;
        let writer = hound::WavWriter::new(
            BufWriter::new(file),
            hound::WavSpec {
                channels: 1,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        self.note(
            ms,
            json!({"event":"audio.file", "role":role, "turn":turn, "file":name, "rate":rate}),
        )?;
        Ok(writer)
    }

    fn end_input(&mut self, ms: u128, submitted: bool) -> Result<()> {
        if let Some((turn, writer)) = self.input.take() {
            let samples = writer.duration();
            writer.finalize()?;
            self.note(ms, json!({
                "event":"input.end", "turn":turn, "samples":samples, "response_requested":submitted
            }))?;
            if submitted {
                self.reply_turn = Some(turn);
            }
        }
        Ok(())
    }

    fn process(&mut self, ms: u128, event: Event) -> Result<()> {
        match event {
            Event::Input { turn, pcm, end } => {
                ensure!(
                    pcm.len() <= 30 * 16_000,
                    "Recording input packet exceeds 30 seconds"
                );
                if self.input.is_none() && !pcm.is_empty() {
                    let writer = self.wav(ms, "user", turn, 16_000)?;
                    self.input = Some((turn, writer));
                }
                self.reserve(pcm.len() * 2)?;
                if let Some((current, writer)) = &mut self.input {
                    ensure!(
                        *current == turn,
                        "Recording input changed before its turn ended"
                    );
                    ensure!(
                        writer.duration() as usize + pcm.len() <= 30 * 16_000,
                        "Recorded input exceeds 30 seconds"
                    );
                    for sample in pcm {
                        writer.write_sample(sample)?;
                    }
                }
                if end {
                    self.end_input(ms, true)?;
                }
            }
            Event::Audio(pcm) => {
                if pcm.is_empty() {
                    return Ok(());
                }
                let turn = self
                    .reply_turn
                    .context("Recorded response has no submitted input")?;
                self.reserve(pcm.len() * 2)?;
                if self.output.is_none() {
                    self.output = Some(self.wav(ms, "assistant", turn, 24_000)?);
                }
                let writer = self.output.as_mut().expect("output was opened");
                for sample in pcm {
                    writer.write_sample(sample)?;
                }
            }
            Event::Note(mut value) => {
                match value["event"].as_str() {
                    Some("pause" | "session.end") => self.end_input(ms, false)?,
                    Some("context.cleared") => {
                        ensure!(
                            self.input.is_none() && self.output.is_none(),
                            "Context cleared inside a recorded turn"
                        );
                        self.context += 1;
                        self.reply_turn = None;
                    }
                    _ => {}
                }
                if (value["event"] == "response.done" || value["event"] == "session.end")
                    && let Some(writer) = self.output.take()
                {
                    value["generated_samples"] = json!(writer.duration());
                    writer.finalize()?;
                }
                self.note(ms, value)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        // Finalize both streams even when an earlier write failed.
        let input = self
            .input
            .take()
            .map(|(_, writer)| writer.finalize())
            .transpose();
        let output = self
            .output
            .take()
            .map(|writer| writer.finalize())
            .transpose();
        let events = self.events.flush();
        input?;
        output?;
        events?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            Self(std::env::temp_dir().join(format!(
                "boubou-recording-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )))
        }
        fn events(&self) -> Vec<Value> {
            fs::read_to_string(self.0.join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn samples(path: &Path, rate: u32) -> Vec<i16> {
        let mut wav = hound::WavReader::open(path).unwrap();
        assert_eq!(wav.spec().sample_rate, rate);
        wav.samples()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn records_exact_submitted_audio_and_generated_reply_across_contexts() {
        use crate::realtime::Output;
        let directory = Directory::new();
        let record = Recording::start(&directory.0, json!({"mode":"test"})).unwrap();
        record.input(1, &[1, 2], false).unwrap();
        record.input(1, &[3], true).unwrap();
        record
            .output(&Output::Text("Synthetic reply".into()))
            .unwrap();
        record.output(&Output::Audio(vec![4, 5])).unwrap();
        record.output(&Output::Interrupted).unwrap();
        record.output(&Output::ContextCleared).unwrap();
        record.input(2, &[6], true).unwrap();
        record.output(&Output::Audio(vec![7])).unwrap();
        record.output(&Output::Done).unwrap();
        record.finish("completed").unwrap();
        assert_eq!(
            samples(&directory.0.join("user-000001.wav"), 16_000),
            [1, 2, 3]
        );
        assert_eq!(
            samples(&directory.0.join("assistant-000001.wav"), 24_000),
            [4, 5]
        );
        assert_eq!(samples(&directory.0.join("user-000002.wav"), 16_000), [6]);
        let events = directory.events();
        assert!(events.iter().any(|e| e["event"] == "assistant.text"
            && e["reply_turn"] == 1
            && e["text"] == "Synthetic reply"));
        assert!(
            events
                .iter()
                .any(|e| e["status"] == "cancelled" && e["generated_samples"] == 2)
        );
        assert!(
            events.iter().any(|e| e["event"] == "response.done"
                && e["reply_turn"] == 2
                && e["context"] == 1)
        );
        assert_eq!(events.last().unwrap()["event"], "session.end");
    }

    #[test]
    fn pause_keeps_partial_input_distinct_and_drop_finalizes_wav() {
        let directory = Directory::new();
        let record = Recording::start(&directory.0, json!({})).unwrap();
        record.input(1, &[1], false).unwrap();
        record.note(json!({"event":"pause"})).unwrap();
        record.input(2, &[2], false).unwrap();
        drop(record);
        assert_eq!(samples(&directory.0.join("user-000001.wav"), 16_000), [1]);
        assert_eq!(samples(&directory.0.join("user-000002.wav"), 16_000), [2]);
        let events = directory.events();
        assert!(events.iter().any(|e| e["event"] == "input.end"
            && e["turn"] == 1
            && e["response_requested"] == false));
        assert!(!events.iter().any(|e| e["response_requested"] == true));
    }

    #[test]
    fn refuses_existing_destinations_and_uses_private_permissions() {
        let directory = Directory::new();
        let record = Recording::start(&directory.0, json!({})).unwrap();
        record.finish("completed").unwrap();
        let before = fs::read(directory.0.join("events.jsonl")).unwrap();
        assert!(Recording::start(&directory.0, json!({})).is_err());
        assert_eq!(fs::read(directory.0.join("events.jsonl")).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&directory.0, &directory.0.join("events.jsonl")] {
                assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
            }
        }
    }

    #[test]
    fn full_queue_and_writer_failure_are_explicit_errors() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let record = Recording {
            sender: Some(sender),
            worker: None,
            error: Arc::new(Mutex::new(None)),
            started: Instant::now(),
        };
        record.note(json!({"event":"first"})).unwrap();
        assert!(record.note(json!({"event":"overflow"})).is_err());
        *record.error.lock().unwrap() = Some("synthetic disk failure".into());
        assert!(
            record
                .check_error()
                .unwrap_err()
                .to_string()
                .contains("synthetic disk failure")
        );
    }

    #[test]
    fn background_failure_reaches_finish() {
        let directory = Directory::new();
        let record = Recording::start(&directory.0, json!({})).unwrap();
        // Invalid ordering fails in the writer, not on the caller thread.
        record
            .output(&crate::realtime::Output::Audio(vec![1]))
            .unwrap();
        assert!(record.finish("completed").is_err());
    }

    #[test]
    fn budget_exhaustion_never_underflows_or_overwrites() {
        let directory = Directory::new();
        fs::create_dir(&directory.0).unwrap();
        let mut writer = Journal {
            directory: directory.0.clone(),
            events: BufWriter::new(private_file(&directory.0.join("events.jsonl")).unwrap()),
            input: None,
            output: None,
            reply_turn: None,
            context: 0,
            remaining: 3,
        };
        writer.reserve(2).unwrap();
        assert!(writer.reserve(2).is_err());
        assert_eq!(writer.remaining, 1);
        assert!(
            writer
                .process(
                    0,
                    Event::Input {
                        turn: 1,
                        pcm: vec![1],
                        end: true
                    }
                )
                .is_err()
        );
        assert!(!directory.0.join("user-000001.wav").exists());
    }
}
