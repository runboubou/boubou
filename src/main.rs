mod assets;
#[cfg(any(target_os = "macos", test))]
mod audio;
mod backend;
mod realtime;
#[cfg(any(target_os = "macos", test))]
mod voice;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
#[cfg(target_os = "macos")]
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal,
};
use realtime::{Output, Realtime};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
#[cfg(target_os = "macos")]
use std::{sync::mpsc::TryRecvError, time::Duration};

const MODEL_ID: &str = "LFM3-Audio-2B";

#[derive(Parser)]
#[command(
    name = "bobo",
    version,
    about = "Local voice conversations, one command away"
)]
struct Cli {
    /// Storage for pinned models and inference binaries.
    #[arg(long, global = true, env = "BOUBOU_HOME")]
    cache_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Start a local voice conversation with automatic context clearing.
    Run {
        #[arg(value_parser = [MODEL_ID])]
        model: String,
        /// Use cached assets only. Never download or check for updates.
        #[arg(long)]
        offline: bool,
        /// Press Enter to start and submit recording, or interrupt a reply.
        #[arg(long, conflicts_with = "input")]
        manual: bool,
        /// Speak from PCM16 mono 16 kHz WAV files instead of the microphone.
        /// Repeat to send multiple turns in one conversation.
        #[arg(long)]
        input: Vec<PathBuf>,
        /// Save spoken replies to a PCM16 mono 24 kHz WAV (requires --input).
        #[arg(long, requires = "input")]
        output: Option<PathBuf>,
    },
    /// Download and verify the Liquid model and its inference backend.
    Pull {
        #[arg(value_parser = [MODEL_ID])]
        model: String,
        #[arg(long)]
        offline: bool,
    },
    /// Show the selected profile and verify cached assets without networking.
    Doctor,
}

fn main() {
    if let Err(error) = run() {
        if error.is::<Cancelled>() {
            std::process::exit(130);
        }
        eprintln!("\nbobo: {error:#}");
        std::process::exit(1);
    }
}

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn check_cancel(stopped: &AtomicBool) -> Result<()> {
    if stopped.load(Ordering::SeqCst) {
        return Err(Cancelled.into());
    }
    Ok(())
}

// Opt-in control metadata only: never pass speech, transcripts, or PCM here.
fn trace(event: &'static str, details: std::fmt::Arguments<'_>) {
    static START: std::sync::OnceLock<Option<Instant>> = std::sync::OnceLock::new();
    if let Some(started) = START.get_or_init(|| {
        std::env::var("BOUBOU_TRACE")
            .is_ok_and(|value| value == "1")
            .then(Instant::now)
    }) {
        eprintln!(
            "\r\n[trace +{}ms] {event} {details}\r",
            started.elapsed().as_millis()
        );
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cache = cli
        .cache_dir
        .or_else(|| dirs::cache_dir().map(|p| p.join("boubou")))
        .context("Cannot find a cache directory. Set BOUBOU_HOME or --cache-dir.")?;
    match cli.command {
        Action::Doctor => {
            println!("bobo {}", env!("CARGO_PKG_VERSION"));
            println!(
                "Platform: {} / {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            println!("Profile: Liquid AI {MODEL_ID} · Q4_K_M + Q8_0 audio · llama.cpp b11960");
            println!("Voice chat: English · audio input 16 kHz · audio output 24 kHz");
            println!(
                "Conversation context: {} tokens; automatically cleared before it fills",
                backend::CONTEXT_TOKENS
            );
            println!("Cache: {}", cache.display());
            assets::status(&cache)
        }
        Action::Pull { offline, .. } => {
            assets::prepare(&cache, offline)?;
            eprintln!("{MODEL_ID} is ready. Run: bobo run {MODEL_ID} --offline");
            Ok(())
        }
        Action::Run {
            offline,
            manual,
            input,
            output,
            ..
        } => {
            if input.is_empty() {
                ensure!(
                    io::stdin().is_terminal(),
                    "Microphone mode needs a terminal. Use --input question.wav for a file."
                );
            }
            // Validate file inputs before downloading models or acquiring audio devices.
            let turns: Vec<Vec<i16>> = input.iter().map(|p| read_wav(p)).collect::<Result<_>>()?;
            let files = assets::prepare(&cache, offline)?;
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = stopped.clone();
            ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
            check_cancel(&stopped)?;
            eprintln!("Loading {MODEL_ID} locally…");
            let backend = backend::Backend::start(&files, &cache, &stopped)?;
            let mut connection = Realtime::connect(&backend.address, &stopped)?;
            let result = if turns.is_empty() {
                microphone(&cache, &mut connection, &stopped, manual)
            } else {
                from_files(turns, output.as_deref(), &mut connection, &stopped)
            };
            check_cancel(&stopped)?;
            result.map_err(|error| backend.explain_failure(error))
        }
    }
}

fn read_wav(path: &Path) -> Result<Vec<i16>> {
    let mut wav =
        hound::WavReader::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let spec = wav.spec();
    ensure!(
        spec.channels == 1
            && spec.sample_rate == 16_000
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "{} must be a PCM16 mono WAV at 16 kHz",
        path.display()
    );
    ensure!(
        wav.duration() > 0 && wav.duration() <= 30 * 16_000,
        "Input WAV must be between 0 and 30 seconds"
    );
    wav.samples()
        .collect::<std::result::Result<Vec<i16>, _>>()
        .context("Invalid PCM16 samples")
}

fn from_files(
    turns: Vec<Vec<i16>>,
    output: Option<&Path>,
    chat: &mut Realtime,
    stopped: &AtomicBool,
) -> Result<()> {
    let mut writer = output
        .map(|path| {
            hound::WavWriter::create(
                path,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 24_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
        })
        .transpose()?;
    for (i, pcm) in turns.iter().enumerate() {
        eprintln!("Turn {}", i + 1);
        while !chat.ready_for_audio()? {
            check_cancel(stopped)?;
            if let Some(Output::ContextCleared) = chat.poll()? {
                eprintln!("[Earlier conversation context cleared.]");
            }
        }
        for chunk in pcm.chunks(1600) {
            check_cancel(stopped)?;
            chat.append_audio(chunk)?;
        }
        chat.finish_audio()?;
        let mut audio_samples = 0;
        while chat.busy() && !stopped.load(Ordering::SeqCst) {
            match chat.poll()? {
                Some(Output::Text(text)) => {
                    print!("{text}");
                    io::stdout().flush()?;
                }
                Some(Output::Audio(pcm)) => {
                    audio_samples += pcm.len();
                    if let Some(writer) = &mut writer {
                        for sample in pcm {
                            writer.write_sample(sample)?;
                        }
                    }
                }
                _ => {}
            }
        }
        println!();
        if stopped.load(Ordering::SeqCst) {
            chat.interrupt()?;
            break;
        }
        ensure!(audio_samples > 0, "Liquid returned no speech for this turn");
        eprintln!(
            "Generated {:.2} s of speech locally.",
            audio_samples as f64 / 24_000.0
        );
    }
    if let Some(writer) = writer {
        writer.finalize()?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct Terminal;
#[cfg(target_os = "macos")]
impl Terminal {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}
#[cfg(target_os = "macos")]
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        eprintln!();
    }
}

#[cfg(not(target_os = "macos"))]
fn microphone(
    _cache: &Path,
    _chat: &mut Realtime,
    _stopped: &AtomicBool,
    _manual: bool,
) -> Result<()> {
    bail!(
        "Live voice conversations currently require macOS. Other audio backends are not implemented yet."
    );
}

#[cfg(target_os = "macos")]
fn microphone(cache: &Path, chat: &mut Realtime, stopped: &AtomicBool, manual: bool) -> Result<()> {
    eprintln!("Allow microphone access if macOS asks. Speak English.");
    let (mut audio, captured) = audio::Audio::start(cache, !manual, stopped)?;
    let _terminal = Terminal::enter()?;
    if manual {
        eprint!(
            "\r\nManual · Enter starts/submits recording or interrupts a reply · Esc pauses · Ctrl+C exits\r\n"
        );
    } else {
        eprint!(
            "\r\nListening · pause to send · speak to interrupt · Esc pauses · Enter resumes · Ctrl+C exits\r\n"
        );
    }
    let mut voice = voice::Voice::new();
    let mut pending = voice::PendingAudio::default();
    let mut recording_since: Option<Instant> = None;
    let mut recording_samples = 0usize;
    let mut submit_at = None;
    let mut reply_open = false;
    let mut last_output = Instant::now();
    let mut listening_since = Some(Instant::now());

    while !stopped.load(Ordering::SeqCst) {
        audio.check_error()?;
        if let Some(since) = recording_since {
            ensure!(
                since.elapsed() < Duration::from_secs(30),
                "Manual recording reached 30 seconds. Restart and submit shorter turns with Enter."
            );
        }
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
        {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                stopped.store(true, Ordering::SeqCst);
                break;
            }
            if key.code == KeyCode::Esc {
                trace(
                    "pause.key",
                    format_args!("queued_samples={}", audio.pending_samples()),
                );
                audio.clear()?;
                chat.interrupt()?;
                chat.end_input_without_response()?;
                pending.clear();
                voice = voice::Voice::new();
                recording_since = None;
                recording_samples = 0;
                submit_at = None;
                listening_since = None;
                trace("pause.latched", format_args!(""));
                eprint!("\r\nPaused · Press Enter to resume · Ctrl+C exits\r\n");
                reply_open = false;
            }
            if key.code == KeyCode::Enter && listening_since.is_none() {
                listening_since = Some(Instant::now());
                trace("resume.key", format_args!(""));
                if !manual {
                    eprint!("\r\nListening…\r\n");
                }
            }
            if manual && key.code == KeyCode::Enter {
                if recording_since.is_some() {
                    // Keep the cutoff across ticks: earlier frames may still be in the pipe.
                    submit_at.get_or_insert_with(Instant::now);
                } else {
                    if chat.busy() || audio.pending_samples() > 0 {
                        audio.clear()?;
                        if chat.interrupt()? {
                            eprint!("\r\n[Interrupted]\r\n");
                        }
                    }
                    let turn = pending.push(voice::Segment::Start(Vec::new()))?;
                    trace("input.start", format_args!("turn={turn} manual=true"));
                    recording_since = Some(Instant::now());
                    recording_samples = 0;
                    reply_open = false;
                    eprint!("\r\nRecording… Press Enter to send.\r\n");
                }
            }
        }
        // Bound each tick so a continuous microphone cannot starve backend events.
        for _ in 0..64 {
            let frame = match captured.try_recv() {
                Ok(audio::CaptureFrame::Samples(frame)) => frame,
                Ok(audio::CaptureFrame::Error(error)) => bail!("Audio: {error}"),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => bail!(
                    "The audio device disconnected. Restart after checking the selected devices."
                ),
            };
            let Some(since) = listening_since else {
                continue;
            };
            if frame.captured_at < since {
                continue;
            }
            ensure!(
                frame.captured_at.elapsed() < Duration::from_millis(500),
                "Microphone processing fell behind. Restart Boubou after closing other busy applications."
            );
            let segment = if manual {
                let Some(since) = recording_since else {
                    continue;
                };
                if frame.captured_at < since {
                    continue;
                }
                if submit_at.is_some_and(|until| frame.captured_at > until) {
                    // Capture is ordered. Crossing the cutoff proves all earlier frames arrived.
                    submit_at = None;
                    if recording_samples == 0 {
                        eprint!(
                            "\r\nNo audio received yet. Keep recording, then press Enter to send.\r\n"
                        );
                    } else {
                        let turn = pending.push(voice::Segment::End(Vec::new()))?;
                        trace("input.end", format_args!("turn={turn} manual=true"));
                        recording_since = None;
                    }
                    continue;
                }
                recording_samples += frame.samples.len();
                ensure!(
                    recording_samples <= 16_000 * 30,
                    "Manual recording exceeded 30 seconds. Restart and submit shorter turns with Enter."
                );
                voice::Segment::Continue(frame.samples)
            } else {
                let Some(segment) = voice.push(frame.samples, frame.near_end_speech)? else {
                    continue;
                };
                segment
            };
            let starting = matches!(segment, voice::Segment::Start(_));
            let ending = matches!(segment, voice::Segment::End(_));
            let turn = pending.push(segment)?;
            if starting {
                trace(
                    "input.start",
                    format_args!(
                        "turn={turn} manual=false backend_busy={} queued_samples={}",
                        chat.busy(),
                        audio.pending_samples()
                    ),
                );
                if chat.busy() || audio.pending_samples() > 0 {
                    audio.clear()?;
                    if chat.interrupt()? {
                        eprint!("\r\n[Interrupted]\r\n");
                    }
                }
                reply_open = false;
            }
            if ending {
                trace("input.end", format_args!("turn={turn} manual=false"));
            }
            if !chat.busy() && audio.pending_samples() == 0 {
                send_pending(
                    &mut pending,
                    chat,
                    !voice.is_active() && recording_since.is_none(),
                )?;
            }
        }
        if let Some(output) = chat.poll()? {
            last_output = Instant::now();
            match output {
                Output::Text(text) => {
                    if !reply_open {
                        eprint!("Liquid: ");
                        reply_open = true;
                    }
                    print!("{}", text.replace('\n', "\r\n"));
                    io::stdout().flush()?;
                }
                Output::Audio(pcm) => audio.enqueue(&pcm)?,
                Output::Done => {
                    print!("\r\n");
                    reply_open = false;
                }
                Output::Interrupted => {}
                Output::ContextCleared => {
                    eprint!("\r\n[Earlier conversation context cleared.]\r\n");
                }
            }
        }
        if !chat.busy() && audio.pending_samples() == 0 {
            send_pending(
                &mut pending,
                chat,
                !voice.is_active() && recording_since.is_none(),
            )?;
        }
        // poll() already waits briefly on the local socket. Detect a stalled player
        // instead of allowing queued speech to hold the device indefinitely.
        ensure!(
            audio.pending_samples() == 0 || last_output.elapsed() < Duration::from_secs(120),
            "Audio playback stopped making progress. Check the output device and restart."
        );
    }
    audio.clear()?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn send_pending(
    pending: &mut voice::PendingAudio,
    chat: &mut Realtime,
    allow_finish: bool,
) -> Result<()> {
    if !chat.ready_for_audio()? {
        return Ok(());
    }
    if let Some((turn, pcm, end)) = pending.take(allow_finish) {
        chat.append_audio(&pcm)?;
        if end {
            trace("input.submit", format_args!("turn={turn}"));
            chat.finish_audio()?;
            eprint!("\r\nThinking…\r\n");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::atomic::AtomicU64};

    struct TestWav {
        path: PathBuf,
    }

    impl TestWav {
        fn new(spec: hound::WavSpec, samples: usize) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "boubou-wav-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            let fixture = Self {
                path: directory.join("input.wav"),
            };
            let mut writer = hound::WavWriter::create(&fixture.path, spec).unwrap();
            for _ in 0..samples {
                match spec.sample_format {
                    hound::SampleFormat::Int => writer.write_sample(123_i32).unwrap(),
                    hound::SampleFormat::Float => writer.write_sample(0.25_f32).unwrap(),
                }
            }
            writer.finalize().unwrap();
            fixture
        }
    }

    impl Drop for TestWav {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    fn input_wav_spec() -> hound::WavSpec {
        hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        }
    }

    #[test]
    fn reads_pcm16_mono_input_and_accepts_exactly_thirty_seconds() {
        let short = TestWav::new(input_wav_spec(), 320);
        assert_eq!(read_wav(&short.path).unwrap(), vec![123; 320]);

        let boundary = TestWav::new(input_wav_spec(), 30 * 16_000);
        assert_eq!(read_wav(&boundary.path).unwrap().len(), 30 * 16_000);
    }

    #[test]
    fn rejects_empty_and_overlong_wav_input() {
        for samples in [0, 30 * 16_000 + 1] {
            let fixture = TestWav::new(input_wav_spec(), samples);
            assert!(
                read_wav(&fixture.path).is_err(),
                "Accepted {samples} samples"
            );
        }
    }

    #[test]
    fn rejects_unsupported_wav_formats() {
        for spec in [
            hound::WavSpec {
                channels: 2,
                ..input_wav_spec()
            },
            hound::WavSpec {
                sample_rate: 44_100,
                ..input_wav_spec()
            },
            hound::WavSpec {
                bits_per_sample: 8,
                ..input_wav_spec()
            },
            hound::WavSpec {
                bits_per_sample: 24,
                ..input_wav_spec()
            },
            hound::WavSpec {
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
                ..input_wav_spec()
            },
        ] {
            let fixture = TestWav::new(spec, 320);
            assert!(read_wav(&fixture.path).is_err(), "Accepted {spec:?}");
        }
    }

    #[test]
    fn rejects_missing_malformed_and_truncated_wav_input() {
        let fixture = TestWav::new(input_wav_spec(), 320);
        assert!(read_wav(&fixture.path.with_file_name("missing.wav")).is_err());

        let length = fs::metadata(&fixture.path).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(&fixture.path)
            .unwrap()
            .set_len(length - 1)
            .unwrap();
        assert!(read_wav(&fixture.path).is_err());

        fs::write(&fixture.path, b"not a WAV file").unwrap();
        assert!(read_wav(&fixture.path).is_err());
    }

    #[test]
    fn model_name_is_explicit_and_matches_the_upstream_model() {
        for command in ["run", "pull"] {
            assert!(Cli::try_parse_from(["bobo", command, MODEL_ID]).is_ok());
            assert!(Cli::try_parse_from(["bobo", command]).is_err());
            assert!(Cli::try_parse_from(["bobo", command, "liquid"]).is_err());
            assert!(Cli::try_parse_from(["bobo", command, "other"]).is_err());
        }
    }

    #[test]
    fn output_requires_a_file_input() {
        assert!(Cli::try_parse_from(["bobo", "run", MODEL_ID, "--output", "reply.wav"]).is_err());
        assert!(Cli::try_parse_from(["bobo", "run", MODEL_ID, "--offline"]).is_ok());
        assert!(Cli::try_parse_from(["bobo", "run", "other"]).is_err());
    }

    #[test]
    fn manual_recording_is_available_only_for_the_microphone() {
        assert!(Cli::try_parse_from(["bobo", "run", MODEL_ID, "--manual", "--offline"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "bobo",
                "run",
                MODEL_ID,
                "--manual",
                "--input",
                "question.wav"
            ])
            .is_err()
        );
    }
}
