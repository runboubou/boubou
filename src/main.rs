// macOS compiles every module, so its build is the authoritative dead-code check.
// On Linux, items used only by guided startup and the macOS audio helper are expected to be unused.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Boubou supports macOS and Linux.");

mod assets;
mod audio;
mod backend;
mod liquid;
mod media;
#[cfg(target_os = "macos")]
mod onboarding;
mod profiles;
mod progress;
mod qwen;
mod realtime;
mod serve;
mod talk;
mod tui;
mod voice;

use anyhow::{Context, Result, bail, ensure};
use clap::{CommandFactory, Parser, Subcommand};
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
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    name = "boubou",
    version,
    about = "Run voice AI locally with open-source models. Bring it to any device with one command."
)]
struct Cli {
    /// Storage for pinned models and inference binaries.
    #[arg(long, global = true, env = "BOUBOU_HOME")]
    cache_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// List supported model IDs.
    List,
    /// Start a local voice conversation with automatic context clearing.
    Run {
        #[arg(value_parser = profiles::value_parser())]
        model: profiles::Model,
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
    /// Talk with a model through a page in a browser on this computer.
    Serve {
        #[arg(value_parser = profiles::value_parser())]
        model: profiles::Model,
        /// Use cached assets only. Never download or check for updates.
        #[arg(long)]
        offline: bool,
        /// The local port that serves the page.
        #[arg(long, default_value_t = 2682)]
        port: u16,
    },
    /// Download and verify the selected model and its inference backend.
    Pull {
        #[arg(value_parser = profiles::value_parser())]
        model: profiles::Model,
        #[arg(long)]
        offline: bool,
    },
    /// Ask a text question about one local PNG/JPEG image or short MP4/MOV video.
    Ask {
        #[arg(value_parser = profiles::value_parser())]
        model: profiles::Model,
        file: PathBuf,
        question: String,
        #[arg(long)]
        offline: bool,
    },
    /// Show the selected profile and verify cached assets without networking.
    Doctor {
        #[arg(value_parser = profiles::value_parser(), default_value = profiles::DEFAULT.id)]
        model: profiles::Model,
    },
}

fn main() {
    if let Err(error) = run() {
        if error.is::<Cancelled>() {
            std::process::exit(130);
        }
        // The terminal may already be gone; printing must not panic on the way out.
        let _ = writeln!(io::stderr(), "\nboubou: {error:#}");
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

trait Conversation {
    fn busy(&self) -> bool;
    fn ready_for_audio(&mut self) -> Result<bool>;
    fn append_audio(&mut self, samples: &[i16]) -> Result<()>;
    fn finish_audio(&mut self) -> Result<()>;
    fn interrupt(&mut self) -> Result<bool>;
    fn poll(&mut self) -> Result<Option<Output>>;
    fn end_input_without_response(&mut self) -> Result<()>;
}

impl Conversation for Realtime {
    fn busy(&self) -> bool {
        self.busy()
    }
    fn ready_for_audio(&mut self) -> Result<bool> {
        self.ready_for_audio()
    }
    fn append_audio(&mut self, samples: &[i16]) -> Result<()> {
        self.append_audio(samples)
    }
    fn finish_audio(&mut self) -> Result<()> {
        self.finish_audio()
    }
    fn interrupt(&mut self) -> Result<bool> {
        self.interrupt()
    }
    fn poll(&mut self) -> Result<Option<Output>> {
        self.poll()
    }
    fn end_input_without_response(&mut self) -> Result<()> {
        self.end_input_without_response()
    }
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
    #[cfg(target_os = "macos")]
    let guided = cli.command.is_none();
    let mut check = None;
    let cache = cli
        .cache_dir
        .or_else(|| dirs::cache_dir().map(|p| p.join("boubou")))
        .context("Cannot find a cache directory. Set BOUBOU_HOME or --cache-dir.")?;
    let command = match cli.command {
        Some(command) => command,
        None => {
            if !io::stdin().is_terminal()
                || !io::stdout().is_terminal()
                || std::env::var("TERM").as_deref() == Ok("dumb")
            {
                Cli::command().print_help()?;
                println!();
                return Ok(());
            }
            #[cfg(target_os = "macos")]
            {
                let selected = onboarding::start(&cache)?;
                check = selected.check;
                Action::Run {
                    model: selected.model,
                    offline: selected.offline,
                    manual: false,
                    input: Vec::new(),
                    output: None,
                }
            }
            #[cfg(not(target_os = "macos"))]
            bail!(
                "Guided startup currently requires an Apple Silicon Mac. Use --help for commands."
            );
        }
    };
    let result = (|| match command {
        Action::List => {
            for model in profiles::Model::all() {
                println!("{}", model.id());
            }
            Ok(())
        }
        Action::Doctor {
            model: profiles::Model::Qwen(id),
        } => qwen::doctor(id, &cache),
        Action::Doctor {
            model: profiles::Model::Liquid(model),
        } => {
            println!("boubou {}", env!("CARGO_PKG_VERSION"));
            println!(
                "Platform: {} / {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            println!(
                "Profile: Liquid AI {} · {} · {}",
                model.id,
                model.quantization,
                model.runtime()?.label
            );
            println!(
                "Voice chat: {} · audio input 16 kHz · audio output 24 kHz",
                model.voice_chat_language
            );
            println!(
                "Conversation context: {} tokens; automatically cleared before it fills",
                model.context_tokens
            );
            println!("Cache: {}", cache.display());
            assets::status(model, &cache)
        }
        Action::Pull { model, offline } => {
            let _checklist = checklist(
                format!("boubou pull {}", model.id()),
                check.take(),
                model,
                &cache,
                false,
            );
            match model {
                profiles::Model::Liquid(profile) => {
                    assets::prepare(profile, &cache, offline)?;
                }
                profiles::Model::Qwen(id) => {
                    qwen::prepare(id, &cache, offline)?;
                }
            }
            let ready = format!("{0} is ready. Run: boubou run {0} --offline", model.id());
            if !progress::finish(&ready) {
                eprintln!("{ready}");
            }
            Ok(())
        }
        Action::Ask {
            model,
            file,
            question,
            offline,
        } => {
            let profiles::Model::Liquid(model) = model else {
                bail!("This model supports voice chat only.");
            };
            let input = media::Input::new(model, &file, &question)?;
            let files = assets::prepare(model, &cache, offline)?;
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = stopped.clone();
            ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
            let _progress = progress::Progress::new("Answering the media question locally", None);
            input.answer(&files, &cache, &question, &stopped)
        }
        Action::Run {
            model,
            offline,
            manual,
            input,
            output,
        } => {
            let _checklist = checklist(
                format!(
                    "boubou run {}{}",
                    model.id(),
                    if offline { " --offline" } else { "" }
                ),
                check.take(),
                model,
                &cache,
                true,
            );
            let model = match model {
                profiles::Model::Liquid(profile) => profile,
                profiles::Model::Qwen(id) => {
                    ensure!(
                        !manual && input.is_empty() && output.is_none(),
                        "Qwen voice chat supports live microphone turns only."
                    );
                    ensure!(
                        io::stdin().is_terminal(),
                        "Qwen voice chat needs a terminal."
                    );
                    return qwen::run(id, &cache, offline, None);
                }
            };
            if input.is_empty() {
                ensure!(
                    io::stdin().is_terminal(),
                    "Microphone mode needs a terminal. Use --input question.wav for a file."
                );
            }
            // Validate file inputs before downloading models or acquiring audio devices.
            let turns: Vec<Vec<i16>> = input.iter().map(|p| read_wav(p)).collect::<Result<_>>()?;
            with_liquid(model, &cache, offline, |chat, stopped| {
                if turns.is_empty() {
                    microphone(&cache, chat, stopped, manual, model.voice_chat_language)
                } else {
                    from_files(turns, output.as_deref(), chat, stopped)
                }
            })
        }
        Action::Serve {
            model,
            offline,
            port,
        } => {
            ensure!(io::stdin().is_terminal(), "boubou serve needs a terminal.");
            let _checklist = checklist(
                format!("boubou serve {}", model.id()),
                check.take(),
                model,
                &cache,
                true,
            );
            match model {
                profiles::Model::Liquid(profile) => {
                    with_liquid(profile, &cache, offline, |chat, stopped| {
                        from_browser(port, chat, stopped)
                    })
                }
                profiles::Model::Qwen(id) => qwen::run(id, &cache, offline, Some(port)),
            }
        }
    })();
    #[cfg(target_os = "macos")]
    if guided {
        return result.map_err(onboarding::explain_failure);
    }
    result
}

/// Loads a Liquid model and lends its conversation to `converse`.
fn with_liquid(
    model: &'static profiles::Profile,
    cache: &Path,
    offline: bool,
    converse: impl FnOnce(&mut dyn Conversation, &AtomicBool) -> Result<()>,
) -> Result<()> {
    let files = assets::prepare(model, cache, offline)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    check_cancel(&stopped)?;
    let backend = backend::Backend::start(&files, cache, &stopped)?;
    let result = match model.protocol {
        profiles::Protocol::Realtime => {
            let mut connection = {
                let _progress = progress::Progress::new("Connecting the voice session", None);
                Realtime::connect(&backend.address, files.profile, &stopped)?
            };
            converse(&mut connection, &stopped).map_err(|error| backend.explain_failure(error))
        }
        profiles::Protocol::LiquidAudio { .. } => converse(
            &mut liquid::Liquid::new(files, cache.to_path_buf(), backend),
            &stopped,
        ),
    };
    check_cancel(&stopped)?;
    result
}

/// The startup checklist for `run`, `serve` and `pull` on a terminal.
fn checklist(
    command: String,
    check: Option<String>,
    model: profiles::Model,
    cache: &Path,
    start: bool,
) -> Option<progress::Checklist> {
    let parts = match model {
        profiles::Model::Liquid(profile) => assets::parts(profile, cache),
        profiles::Model::Qwen(id) => qwen::parts(id, cache),
    }
    .unwrap_or_default();
    let check = check.unwrap_or_else(|| {
        #[cfg(target_os = "macos")]
        let os = qwen::macos_version().map_or_else(
            |_| "macOS".to_owned(),
            |(major, minor, patch)| format!("macOS {major}.{minor}.{patch}"),
        );
        #[cfg(not(target_os = "macos"))]
        let os = "Linux".to_owned();
        let arch = match std::env::consts::ARCH {
            "aarch64" if cfg!(target_os = "macos") => "Apple Silicon",
            "aarch64" => "arm64",
            "x86_64" => "x86-64",
            other => other,
        };
        format!("{os} · {arch}")
    });
    progress::Checklist::begin(command, check, parts, start)
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
    chat: &mut dyn Conversation,
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
    progress::finish("Ready. Sending the input files.");
    let mut missing_speech = false;
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
        let mut has_text = false;
        while chat.busy() && !stopped.load(Ordering::SeqCst) {
            match chat.poll()? {
                Some(Output::Text(text)) => {
                    has_text |= !text.trim().is_empty();
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
        if audio_samples == 0 {
            ensure!(
                has_text,
                "Liquid returned neither text nor speech for this turn"
            );
            missing_speech = true;
            eprintln!("Turn {} returned text without speech; continuing.", i + 1);
        } else {
            eprintln!(
                "Generated {:.2} s of speech locally.",
                audio_samples as f64 / 24_000.0
            );
        }
    }
    if let Some(writer) = writer {
        writer.finalize()?;
    }
    ensure!(
        !missing_speech,
        "Liquid returned text without speech for one or more turns"
    );
    Ok(())
}

/// Returns the next key or resize event, or waits up to `timeout` for input.
///
/// crossterm 0.29 retries a closed terminal forever inside `event::poll`, and its blocking read
/// can start without input. So crossterm reads only when the terminal has input, or when its
/// last read may still hold parsed events, and never waits itself. A resize sends no input, so
/// it is noticed by comparing the terminal size.
fn next_event(timeout: Duration) -> Result<Option<Event>> {
    static BUFFERED: AtomicBool = AtomicBool::new(false);
    // Until a size is recorded, the first call only records it; a failed or empty read leaves
    // the hang-up check to report a closed terminal.
    if let Some((columns, rows)) = terminal_size() {
        let packed = u32::from(columns) << 16 | u32::from(rows);
        let previous = TERMINAL_SIZE.swap(packed, Ordering::Relaxed);
        if previous != 0 && previous != packed {
            return Ok(Some(Event::Resize(columns, rows)));
        }
    }
    for wait in [Duration::ZERO, timeout] {
        // Check for a hang-up first, even when crossterm may hold buffered events.
        let input = terminal_input(wait)?;
        if (input || BUFFERED.load(Ordering::Relaxed)) && event::poll(Duration::ZERO)? {
            BUFFERED.store(true, Ordering::Relaxed);
            return Ok(Some(event::read()?));
        }
        BUFFERED.store(false, Ordering::Relaxed);
    }
    Ok(None)
}

/// The terminal size `next_event` last saw, to report a resize.
static TERMINAL_SIZE: AtomicU32 = AtomicU32::new(0);

fn terminal_size() -> Option<(u16, u16)> {
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    (unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0)
        .then_some((size.ws_col, size.ws_row))
}

/// Records the size a screen is about to draw for. Otherwise a resize between the first draw and
/// the first `next_event` would become the recorded size and never be reported.
fn watch_resizes() {
    if let Some((columns, rows)) = terminal_size() {
        TERMINAL_SIZE.store(
            u32::from(columns) << 16 | u32::from(rows),
            Ordering::Relaxed,
        );
    }
}

/// Waits up to `timeout` and returns whether the terminal has input, failing once it closes.
fn terminal_input(timeout: Duration) -> Result<bool> {
    let mut input = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    if unsafe { libc::poll(&mut input, 1, millis) } <= 0 {
        // Timed out, or a signal interrupted the wait; callers check their cancellation flags.
        return Ok(false);
    }
    let mut pending: libc::c_int = 0;
    ensure!(
        input.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0
            && unsafe { libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut pending) } == 0
            && pending > 0,
        "The terminal closed."
    );
    Ok(true)
}

struct Terminal;
impl Terminal {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = writeln!(io::stderr());
    }
}

fn microphone(
    cache: &Path,
    chat: &mut dyn Conversation,
    stopped: &AtomicBool,
    manual: bool,
    language: &str,
) -> Result<()> {
    let (mut audio, captured) = {
        let _progress = progress::opening_audio();
        audio::Audio::start(cache, !manual, stopped)?
    };
    if !progress::finish(&crate::progress::ready(language)) {
        eprintln!("Ready. Speak {language}.");
    }
    converse(&mut audio, &captured, chat, stopped, manual)
}

/// Lends the conversation to one browser page at a time, until Ctrl+C.
fn from_browser(port: u16, chat: &mut dyn Conversation, stopped: &AtomicBool) -> Result<()> {
    let door = serve::Door::open(port)?;
    while let Some((mut browser, captured)) = door.wait(stopped, || chat.poll().map(drop))? {
        let result = converse(&mut browser, &captured, chat, stopped, false);
        if !browser.left() {
            return result;
        }
        // Whatever the page was in the middle of ends with it.
        chat.interrupt()?;
        chat.end_input_without_response()?;
    }
    Ok(())
}

/// One conversation through an audio device, until it is stopped or the device fails.
fn converse(
    audio: &mut dyn audio::Device,
    captured: &Receiver<audio::CaptureFrame>,
    chat: &mut dyn Conversation,
    stopped: &AtomicBool,
    manual: bool,
) -> Result<()> {
    let _terminal = Terminal::enter()?;
    let mut panel = talk::Panel::new(manual);
    panel.note(talk::Note::Intro);
    let mut voice = voice::Voice::new();
    let mut pending = voice::PendingAudio::default();
    let mut recording_since: Option<Instant> = None;
    let mut recording_samples = 0usize;
    let mut submit_at = None;
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
        if let Some(Event::Key(key)) = next_event(Duration::ZERO)? {
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
                panel.silenced();
                chat.interrupt()?;
                chat.end_input_without_response()?;
                pending.clear();
                voice = voice::Voice::new();
                recording_since = None;
                recording_samples = 0;
                submit_at = None;
                listening_since = None;
                trace("pause.latched", format_args!(""));
                panel.note(talk::Note::Paused);
            }
            if key.code == KeyCode::Enter && listening_since.is_none() {
                listening_since = Some(Instant::now());
                trace("resume.key", format_args!(""));
                panel.note(talk::Note::Resumed);
            }
            if manual && key.code == KeyCode::Enter {
                if recording_since.is_some() {
                    // Keep the cutoff across ticks: earlier frames may still be in the pipe.
                    submit_at.get_or_insert_with(Instant::now);
                } else {
                    if chat.busy() || audio.pending_samples() > 0 {
                        audio.clear()?;
                        panel.silenced();
                        if chat.interrupt()? {
                            panel.note(talk::Note::Interrupted);
                        }
                    }
                    let turn = pending.push(voice::Segment::Start(Vec::new()))?;
                    trace("input.start", format_args!("turn={turn} manual=true"));
                    recording_since = Some(Instant::now());
                    recording_samples = 0;
                    panel.note(talk::Note::Recording);
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
            panel.mic(&frame.samples);
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
                        panel.note(talk::Note::NoAudioYet);
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
                    panel.silenced();
                    if chat.interrupt()? {
                        panel.note(talk::Note::Interrupted);
                    }
                }
                panel.reply_close();
            }
            if ending {
                trace("input.end", format_args!("turn={turn} manual=false"));
            }
            if !chat.busy()
                && audio.pending_samples() == 0
                && send_pending(
                    &mut pending,
                    chat,
                    !voice.is_active() && recording_since.is_none(),
                )?
            {
                panel.note(talk::Note::Thinking);
            }
        }
        if let Some(output) = chat.poll()? {
            last_output = Instant::now();
            match output {
                Output::Text(text) => panel.reply("Liquid", &text),
                Output::Audio(pcm) => {
                    audio.enqueue(&pcm)?;
                    panel.played(&pcm);
                }
                Output::Done => panel.reply_done(),
                Output::Interrupted => {}
                Output::ContextCleared => panel.note(talk::Note::ContextCleared),
            }
        }
        if !chat.busy()
            && audio.pending_samples() == 0
            && send_pending(
                &mut pending,
                chat,
                !voice.is_active() && recording_since.is_none(),
            )?
        {
            panel.note(talk::Note::Thinking);
        }
        panel.tick(audio.pending_samples());
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

/// Sends queued speech when the conversation can take it. True when a turn was submitted.
fn send_pending(
    pending: &mut voice::PendingAudio,
    chat: &mut dyn Conversation,
    allow_finish: bool,
) -> Result<bool> {
    if !chat.ready_for_audio()? {
        return Ok(false);
    }
    if let Some((turn, pcm, end)) = pending.take(allow_finish) {
        chat.append_audio(&pcm)?;
        if end {
            trace("input.submit", format_args!("turn={turn}"));
            chat.finish_audio()?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, fs, sync::atomic::AtomicU64};

    struct ScriptedChat {
        responses: VecDeque<Vec<Output>>,
        current: std::vec::IntoIter<Output>,
    }

    impl Conversation for ScriptedChat {
        fn busy(&self) -> bool {
            !self.current.as_slice().is_empty()
        }
        fn ready_for_audio(&mut self) -> Result<bool> {
            Ok(!self.busy())
        }
        fn append_audio(&mut self, _: &[i16]) -> Result<()> {
            Ok(())
        }
        fn finish_audio(&mut self) -> Result<()> {
            self.current = self.responses.pop_front().unwrap().into_iter();
            Ok(())
        }
        fn interrupt(&mut self) -> Result<bool> {
            self.current = Vec::new().into_iter();
            Ok(true)
        }
        fn poll(&mut self) -> Result<Option<Output>> {
            Ok(self.current.next())
        }
        fn end_input_without_response(&mut self) -> Result<()> {
            Ok(())
        }
    }

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
    fn file_batch_keeps_later_audio_but_reports_missing_speech() {
        for (first, remaining, expected_pcm, expected_error) in [
            (
                vec![Output::Text("Okay.".into()), Output::Done],
                0,
                vec![123, -456],
                Some("text without speech"),
            ),
            (
                vec![Output::Done],
                1,
                vec![],
                Some("neither text nor speech"),
            ),
            (
                vec![Output::Audio(vec![321]), Output::Done],
                0,
                vec![321, 123, -456],
                None,
            ),
        ] {
            let fixture = TestWav::new(input_wav_spec(), 320);
            let output = fixture.path.with_file_name("output.wav");
            let mut chat = ScriptedChat {
                responses: VecDeque::from([
                    first,
                    vec![Output::Audio(vec![123, -456]), Output::Done],
                ]),
                current: Vec::new().into_iter(),
            };
            let result = from_files(
                vec![vec![0; 320]; 2],
                Some(&output),
                &mut chat,
                &AtomicBool::new(false),
            );
            match expected_error {
                Some(message) => assert!(result.unwrap_err().to_string().contains(message)),
                None => result.unwrap(),
            }
            assert_eq!(chat.responses.len(), remaining);
            let mut wav = hound::WavReader::open(output).unwrap();
            assert_eq!(wav.spec().sample_rate, 24_000);
            assert_eq!(
                wav.samples::<i16>()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap(),
                expected_pcm
            );
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
        for command in ["run", "serve", "pull"] {
            for profile in profiles::PROFILES {
                let cli = Cli::try_parse_from(["boubou", command, profile.id]).unwrap();
                let selected = match cli.command.unwrap() {
                    Action::Run { model, .. }
                    | Action::Serve { model, .. }
                    | Action::Pull { model, .. } => model,
                    _ => unreachable!(),
                };
                assert!(
                    matches!(selected, profiles::Model::Liquid(p) if std::ptr::eq(p, *profile))
                );
            }
            assert!(Cli::try_parse_from(["boubou", command]).is_err());
            assert!(Cli::try_parse_from(["boubou", command, "liquid"]).is_err());
            assert!(Cli::try_parse_from(["boubou", command, "other"]).is_err());
        }
    }

    #[test]
    fn output_requires_a_file_input() {
        assert!(
            Cli::try_parse_from([
                "boubou",
                "run",
                profiles::DEFAULT.id,
                "--output",
                "reply.wav"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["boubou", "run", profiles::DEFAULT.id, "--offline"]).is_ok());
        assert!(Cli::try_parse_from(["boubou", "run", "other"]).is_err());
    }

    #[test]
    fn manual_recording_is_available_only_for_the_microphone() {
        assert!(
            Cli::try_parse_from([
                "boubou",
                "run",
                profiles::DEFAULT.id,
                "--manual",
                "--offline"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "boubou",
                "run",
                profiles::DEFAULT.id,
                "--manual",
                "--input",
                "question.wav"
            ])
            .is_err()
        );
    }
}
