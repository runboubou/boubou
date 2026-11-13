//! Startup progress. On a terminal, `run` and `pull` show one checklist (check, download,
//! verify, start) redrawn in place; everything else, and every non-terminal, keeps one plain
//! line per phase.

use crate::tui::{self, BOLD, DIM, Paint, RESET};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const FRAME: Duration = Duration::from_millis(80);

pub const HERE: &str = if cfg!(target_os = "macos") {
    "On this Mac"
} else {
    "On this computer"
};

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum Step {
    Check,
    Download,
    Verify,
    Start,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Phase {
    Download,
    Verify,
}

/// A named group of pinned files, shown as one chip under the download bar.
pub struct Part {
    pub name: String,
    pub files: Vec<(PathBuf, u64)>,
}

struct File {
    path: PathBuf,
    size: u64,
    part: usize,
    /// Full-sized at startup: nothing to download, only to verify.
    cached: bool,
    downloaded: u64,
}

struct State {
    command: String,
    steps: Vec<(Step, &'static str, String)>,
    active: Step,
    failed: bool,
    ready: bool,
    parts: Vec<String>,
    files: Vec<File>,
    downloading: Option<usize>,
    download_total: u64,
    download_done: u64,
    verify_total: u64,
    verify_done: u64,
    tick: usize,
    phase_started: Instant,
    download_status: String,
}

struct Shared {
    state: State,
    paint: Paint,
    drawn: usize,
}

static CHECKLIST: Mutex<Option<Shared>> = Mutex::new(None);
static WORKER: Mutex<Option<(mpsc::Sender<()>, JoinHandle<()>)>> = Mutex::new(None);

/// Owns the checklist for one startup. Dropping it without [`finish`] marks the active step failed.
pub struct Checklist;

impl Checklist {
    /// Starts the checklist when stderr is an interactive terminal.
    pub fn begin(command: String, check: String, parts: Vec<Part>, start: bool) -> Option<Self> {
        if !tui::interactive(&io::stderr()) {
            return None;
        }
        let mut steps = vec![
            (Step::Check, "Check this computer", check),
            (Step::Download, "Download pinned model files", String::new()),
            (
                Step::Verify,
                "Verify every file",
                "Each one must match its pinned SHA-256".into(),
            ),
        ];
        if cfg!(target_os = "macos") {
            steps[0].1 = "Check this Mac";
        }
        if start {
            steps.push((
                Step::Start,
                if cfg!(target_os = "macos") {
                    "Start on this Mac"
                } else {
                    "Start on this computer"
                },
                String::new(),
            ));
        }
        let state = State::new(command, steps, parts);
        *CHECKLIST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Shared {
            state,
            paint: Paint::detect(),
            drawn: 0,
        });
        eprintln!();
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("startup-checklist".into())
            .spawn(move || {
                loop {
                    redraw(|state| state.tick += 1);
                    if stopped.recv_timeout(FRAME) != Err(mpsc::RecvTimeoutError::Timeout) {
                        break;
                    }
                }
            })
            .ok()?;
        *WORKER.lock().unwrap_or_else(|e| e.into_inner()) = Some((stop, worker));
        Some(Self)
    }
}

impl Drop for Checklist {
    fn drop(&mut self) {
        end(|state| state.failed = true);
    }
}

/// Marks every step done, lights the local status and leaves the final frame in scrollback.
/// Returns false when no checklist is showing, so callers print their plain message.
pub fn finish(note: &str) -> bool {
    end(|state| {
        state.ready = true;
        if let Some(last) = state.steps.last_mut() {
            last.2 = note.into();
        }
    })
}

/// The checklist's last note once the conversation can start.
pub fn ready(language: &str) -> String {
    format!(
        "Ready. Speak {language}. Speech stays {}, network or not.",
        HERE.replacen('O', "o", 1)
    )
}

/// The progress for opening audio devices. On macOS it says that macOS may ask for microphone
/// access, which it does while the devices open.
pub fn opening_audio() -> Progress {
    const LABEL: &str = "Opening microphone and speakers";
    if !cfg!(target_os = "macos") {
        return Progress::new(LABEL, None);
    }
    let mut shown = false;
    update(|_| shown = true);
    if shown {
        Progress::new(&format!("{LABEL} · allow access if macOS asks"), None)
    } else {
        eprintln!("Allow microphone access if macOS asks.");
        Progress::new(LABEL, None)
    }
}

fn end(mark: impl FnOnce(&mut State)) -> bool {
    let worker = WORKER.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some((stop, worker)) = worker else {
        return false;
    };
    let _ = stop.send(());
    let _ = worker.join();
    redraw(mark);
    *CHECKLIST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    true
}

/// Moves the checklist to `step`; earlier steps count as done. A step this checklist does not
/// show (`pull` has no Start) keeps the last shown step active, so its failure still shows ✗.
pub fn step(step: Step) {
    update(|state| state.reach(step));
}

fn update(change: impl FnOnce(&mut State)) {
    if let Some(shared) = CHECKLIST.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        change(&mut shared.state);
    }
}

fn redraw(change: impl FnOnce(&mut State)) {
    let mut guard = CHECKLIST.lock().unwrap_or_else(|e| e.into_inner());
    let Some(shared) = guard.as_mut() else {
        return;
    };
    change(&mut shared.state);
    let width = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);
    let lines = shared.state.frame(shared.paint, width);
    let mut out = Vec::new();
    if shared.drawn > 0 {
        let _ = write!(out, "\x1b[{}A", shared.drawn);
    }
    for line in &lines {
        let _ = write!(out, "\r\x1b[2K{line}{RESET}\n");
    }
    // A terminal made narrower loses the toucan's rows: clear what the taller frame left below.
    out.extend(b"\x1b[J");
    shared.drawn = lines.len();
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(&out);
    let _ = stderr.flush();
}

impl State {
    fn new(command: String, steps: Vec<(Step, &'static str, String)>, parts: Vec<Part>) -> Self {
        let mut files = Vec::new();
        let mut names = Vec::new();
        for (index, part) in parts.into_iter().enumerate() {
            names.push(part.name);
            for (path, size) in part.files {
                let full = |path: &Path| {
                    path.metadata()
                        .is_ok_and(|m| m.is_file() && m.len() == size)
                };
                // A full-sized partial is only verified, as a cached file is.
                let mut partial = path.clone().into_os_string();
                partial.push(".part");
                let cached = full(&path) || full(Path::new(&partial));
                files.push(File {
                    path,
                    size,
                    part: index,
                    cached,
                    downloaded: 0,
                });
            }
        }
        Self {
            command,
            steps,
            active: Step::Check,
            failed: false,
            ready: false,
            parts: names,
            download_total: files.iter().filter(|f| !f.cached).map(|f| f.size).sum(),
            verify_total: files.iter().map(|f| f.size).sum(),
            files,
            downloading: None,
            download_done: 0,
            verify_done: 0,
            tick: 0,
            phase_started: Instant::now(),
            download_status: String::new(),
        }
    }

    /// A download counts the `size` it fetches: a cached file that failed verification joins the
    /// download, and an engine update replaces its estimate with the changed files it fetches.
    fn file_started(&mut self, path: &Path, phase: Phase, size: u64) -> Option<usize> {
        let index = self.files.iter().position(|f| f.path == path)?;
        if phase == Phase::Download {
            let file = &mut self.files[index];
            self.download_total =
                self.download_total + size - if file.cached { 0 } else { file.size };
            self.verify_total = self.verify_total + size - file.size;
            file.size = size;
            file.cached = false;
            self.download_done -= file.downloaded;
            file.downloaded = 0;
            self.downloading = Some(index);
        }
        Some(index)
    }

    fn advanced(&mut self, counted: Counted, bytes: u64) {
        match counted {
            Counted::Download(file) => {
                self.files[file].downloaded += bytes;
                self.download_done += bytes;
            }
            Counted::Verify => self.verify_done += bytes,
        }
    }

    fn reach(&mut self, step: Step) {
        if step > self.active && self.steps.iter().any(|(s, ..)| *s == step) {
            self.active = step;
            self.phase_started = Instant::now();
        }
    }

    fn describe(&mut self, label: &str) {
        if self.active == Step::Download {
            self.download_status = label.into();
        } else if let Some((_, _, note)) = self.steps.iter_mut().find(|(s, ..)| *s == self.active) {
            *note = label.into();
        }
    }

    fn part_done(&self, part: usize) -> bool {
        self.files
            .iter()
            .filter(|f| f.part == part)
            .all(|f| f.cached || f.downloaded >= f.size)
    }

    fn status(&self, step: Step) -> Status {
        if self.ready || step < self.active {
            Status::Done
        } else if step > self.active {
            Status::Pending
        } else if self.failed {
            Status::Failed
        } else {
            Status::Active
        }
    }

    /// One frame: the header the menu has, with the command where the menu has its title, then
    /// the steps. At one width every frame has the same number of lines, so it is redrawn in place.
    ///
    /// The menu redraws when its window changes; these lines stay as printed, and one that wrapped
    /// in a window made narrower would cut the toucan in two. So the header keeps to the 60
    /// columns the toucan asks for, and a longer command breaks at its spaces.
    fn frame(&self, paint: Paint, width: usize) -> Vec<String> {
        let room = width.saturating_sub(7).max(1);
        let spinner = tui::SPINNER[self.tick % tui::SPINNER.len()];
        let (style, mark) = if self.ready {
            (paint.fg(tui::ACCENT), '●')
        } else {
            (DIM.to_owned(), '○')
        };
        let light = format!("{mark} {HERE}");
        let mut lines = tui::header(paint, width.min(60), (&style, &light), |room| {
            tui::wrap(&self.command, room.saturating_sub(2))
                .iter()
                .zip(std::iter::once('$').chain(std::iter::repeat(' ')))
                .map(|(line, prompt)| format!("{DIM}{prompt}{RESET} {line}"))
                .collect()
        });
        lines.push(String::new());
        for (step, title, note) in &self.steps {
            let status = self.status(*step);
            let (icon, title_style) = match status {
                Status::Done => ("✓".to_owned(), BOLD),
                Status::Active => (spinner.to_string(), BOLD),
                Status::Failed => (format!("{}✗", paint.fg(tui::WARN)), BOLD),
                Status::Pending => ("○".to_owned(), DIM),
            };
            let dim_icon = if status == Status::Pending { DIM } else { "" };
            let title = if status == Status::Active {
                format!(
                    "{title} · {}s elapsed",
                    self.phase_started.elapsed().as_secs()
                )
            } else {
                (*title).to_owned()
            };
            lines.push(format!(
                "  {dim_icon}{icon}{RESET}  {title_style}{}",
                tui::fit(&title, room)
            ));
            let note = match step {
                Step::Download => {
                    lines.push(self.download_bar(paint, room));
                    self.chips(spinner, room)
                }
                Step::Verify
                    if status == Status::Active
                        || (status == Status::Done
                            && Some(*step) != self.steps.last().map(|s| s.0)) =>
                {
                    format!("{note} · {}", amount(self.verify_done, self.verify_total))
                }
                _ => note.clone(),
            };
            lines.push(format!("     {DIM}{}", tui::fit(&note, room)));
            if *step == Step::Download {
                lines.push(format!(
                    "     {DIM}{}",
                    tui::fit(&self.download_status, room)
                ));
            }
        }
        if let Some(notice) = crate::update::message() {
            lines.push(String::new());
            for line in tui::wrap(&notice, width.saturating_sub(4).max(1)) {
                lines.push(format!("  {line}"));
            }
        }
        lines
    }

    fn download_bar(&self, paint: Paint, room: usize) -> String {
        if self.download_total == 0 {
            return format!(
                "     {DIM}{}",
                tui::fit(
                    &format!("Already here · {}", tui::gb(self.verify_total)),
                    room
                )
            );
        }
        let done = self.download_done.min(self.download_total);
        let amount = amount(done, self.download_total);
        let cells = room.saturating_sub(tui::width(&amount) + 2).min(56);
        format!(
            "     {}  {amount}",
            smooth_bar(paint, done, self.download_total, cells)
        )
    }

    fn chips(&self, spinner: char, room: usize) -> String {
        let current = self.downloading.map(|i| self.files[i].part);
        let mut styled = Vec::new();
        let mut plain = Vec::new();
        for (i, name) in self.parts.iter().enumerate() {
            let (mark, lit) = if self.part_done(i) {
                ('✓', true)
            } else if current == Some(i) && self.active == Step::Download && !self.failed {
                (spinner, true)
            } else {
                ('○', false)
            };
            plain.push(format!("{mark} {name}"));
            styled.push(if lit {
                format!("{RESET}{mark} {name}{DIM}")
            } else {
                format!("{mark} {name}")
            });
        }
        // Lit chips break out of the dim note; fall back to plain text when it must be cut.
        if tui::width(&plain.join("   ")) <= room {
            styled.join("   ")
        } else {
            tui::fit(&plain.join("  "), room)
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Done,
    Active,
    Failed,
    Pending,
}

/// "0.62 / 1.42 GB", or MB below 0.1 GB, such as a speech engine update's few changed files.
fn amount(done: u64, total: u64) -> String {
    let (unit, scale) = if total < 100_000_000 {
        ("MB", 1e6)
    } else {
        ("GB", 1e9)
    };
    let done = done.min(total) as f64 / scale;
    format!("{done:.2} / {:.2} {unit}", total as f64 / scale)
}

/// An eighth-block bar on a shaded track. Without colour the track is drawn with ░.
fn smooth_bar(paint: Paint, done: u64, total: u64, cells: usize) -> String {
    let (full, partial, empty) = tui::bar(done, total, cells);
    let track = paint.bg(tui::TRACK);
    let mut bar = "█".repeat(full);
    if paint.colored() {
        if let Some(partial) = partial {
            bar.push_str(&format!("{track}{partial}"));
        }
        bar.push_str(&format!("{track}{}{RESET}", " ".repeat(empty)));
    } else {
        if let Some(partial) = partial {
            bar.push(partial);
        }
        bar.push_str(&format!("{DIM}{}{RESET}", "░".repeat(empty)));
    }
    bar
}

/// What a [`Progress`] adds its bytes to on the checklist.
#[derive(Clone, Copy)]
enum Counted {
    Download(usize),
    Verify,
}

/// One visible blocking phase. Byte totals are measured; unknown totals animate without a percentage.
pub struct Progress {
    bytes: Arc<AtomicU64>,
    label: Arc<Mutex<String>>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    /// Set when the checklist shows this progress.
    checklist: Option<Counted>,
}

impl Progress {
    pub fn new(label: &str, total: Option<u64>) -> Self {
        let bytes = Arc::new(AtomicU64::new(0));
        let label_text = Arc::new(Mutex::new(label.to_owned()));
        let mut shown = false;
        let mut verifying = false;
        update(|state| {
            shown = true;
            let active = state.active;
            verifying = active == Step::Verify && total.is_some();
            state.describe(label);
        });
        if shown {
            // Hashing several files at once still counts toward the checklist's Verify step.
            return Self {
                bytes,
                label: label_text,
                stop: None,
                worker: None,
                checklist: verifying.then_some(Counted::Verify),
            };
        }
        let interactive = tui::interactive(&io::stderr());
        let paint = Paint::detect();
        if interactive && paint.colored() {
            eprintln!("{BOLD}{label}{RESET}");
        } else {
            eprintln!("{label}");
        }
        if !interactive {
            return Self {
                bytes,
                label: label_text,
                stop: None,
                worker: None,
                checklist: None,
            };
        }
        let position = bytes.clone();
        let status = label_text.clone();
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("startup-progress".into())
            .spawn(move || {
                let began = Instant::now();
                let mut tick = 0;
                loop {
                    let mut out = io::stderr().lock();
                    let width = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);
                    let label = status.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = write!(
                        out,
                        "\r\x1b[2K{}\n\r\x1b[2K  {}\x1b[1A",
                        progress_line(
                            paint,
                            position.load(Ordering::Relaxed),
                            total,
                            tick,
                            began.elapsed(),
                            width
                        ),
                        tui::fit(&label, width.saturating_sub(3))
                    );
                    drop(label);
                    let _ = out.flush();
                    drop(out);
                    if stopped.recv_timeout(FRAME) != Err(mpsc::RecvTimeoutError::Timeout) {
                        break;
                    }
                    tick += 1;
                }
                // Preserve the phase heading, leaving the next message on a clean line.
                let _ = write!(io::stderr(), "\r\x1b[2K\n\r\x1b[2K\x1b[1A");
                let _ = io::stderr().flush();
            })
            .ok();
        Self {
            bytes,
            label: label_text,
            stop: Some(stop),
            worker,
            checklist: None,
        }
    }

    /// A pinned file being downloaded or hashed. The checklist counts it toward its step.
    pub fn file(label: &str, path: &Path, size: u64, phase: Phase) -> Self {
        let mut index = None;
        update(|state| index = state.file_started(path, phase, size));
        if index.is_some() {
            update(|state| state.describe(label));
        }
        match index {
            Some(file) => Self {
                bytes: Arc::new(AtomicU64::new(0)),
                label: Arc::new(Mutex::new(label.into())),
                stop: None,
                worker: None,
                checklist: Some(match phase {
                    Phase::Download => Counted::Download(file),
                    Phase::Verify => Counted::Verify,
                }),
            },
            None => Self::new(label, Some(size)),
        }
    }

    pub fn advance(&self, bytes: u64) {
        if let Some(counted) = self.checklist {
            update(|state| state.advanced(counted, bytes));
        } else {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Replaces the current operation without hiding byte progress or printing repeated lines.
    pub fn status(&self, message: &str) {
        // The checklist is the only owner while it is active. A second deduplication value
        // could get ahead of its displayed state when range workers and the writer interleave.
        let mut shown = false;
        update(|state| {
            shown = true;
            state.describe(message);
        });
        if shown {
            return;
        }
        let mut label = self.label.lock().unwrap_or_else(|e| e.into_inner());
        if *label == message {
            return;
        }
        *label = message.into();
        drop(label);
        if !tui::interactive(&io::stderr()) {
            let _ = writeln!(io::stderr(), "{message}");
        }
    }
}

fn progress_line(
    paint: Paint,
    bytes: u64,
    total: Option<u64>,
    tick: usize,
    elapsed: Duration,
    width: usize,
) -> String {
    let elapsed = format!("{}s elapsed", elapsed.as_secs());
    let room = width.saturating_sub(tui::width(&elapsed) + 5);
    if room < 10 {
        return tui::fit(&elapsed, width.saturating_sub(1));
    }
    format!(
        "  {}  {DIM}{elapsed}{RESET}",
        bar(paint, bytes, total, tick, room)
    )
}

fn bar(paint: Paint, bytes: u64, total: Option<u64>, tick: usize, room: usize) -> String {
    let Some(total) = total else {
        return format!(
            "{} {DIM}working{RESET}",
            tui::SPINNER[tick % tui::SPINNER.len()]
        );
    };
    let percent = if total == 0 {
        100
    } else {
        u128::from(bytes.min(total)) * 100 / u128::from(total)
    };
    let amount = format!("{percent:3}%  {}", amount(bytes, total));
    let cells = room.saturating_sub(tui::width(&amount) + 2).min(24);
    if cells == 0 {
        return tui::fit(&amount, room);
    }
    format!(
        "{}  {DIM}{amount}{RESET}",
        smooth_bar(paint, bytes, total, cells)
    )
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(Counted::Download(file)) = self.checklist {
            update(|state| {
                if state.downloading == Some(file) {
                    state.downloading = None;
                }
            });
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::Colors;

    #[test]
    fn receiving_replaces_retry_even_when_the_standalone_label_is_unchanged() {
        // Isolate the process-global terminal owner from other parallel progress/download tests.
        const CHILD: &str = "BOUBOU_PROGRESS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "progress::tests::receiving_replaces_retry_even_when_the_standalone_label_is_unchanged"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let mut state = fixture(false);
        state.reach(Step::Download);
        *CHECKLIST.lock().unwrap() = Some(Shared {
            state,
            paint: Paint(Colors::None),
            drawn: 0,
        });
        let progress = Progress::new("Receiving model.bin", Some(100));
        // Reproduce the old interleaving: the label said Receiving but the last UI write was Retry.
        update(|state| state.describe("Retry 1/2: connection interrupted"));
        progress.status("Receiving model.bin");
        assert_eq!(
            CHECKLIST
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .state
                .download_status,
            "Receiving model.bin"
        );
        CHECKLIST.lock().unwrap().take();
    }

    #[test]
    fn only_known_totals_show_a_bounded_percentage() {
        let paint = Paint(Colors::None);
        assert!(bar(paint, 25, Some(100), 0, 70).contains(" 25%"));
        assert!(bar(paint, 200, Some(100), 0, 70).contains("100%"));
        assert!(bar(paint, 0, Some(0), 0, 70).contains("100%"));
        assert!(!bar(paint, 200, None, 0, 70).contains('%'));
        assert_ne!(bar(paint, 0, None, 0, 70), bar(paint, 0, None, 1, 70));
    }

    #[test]
    fn standalone_progress_does_not_wrap_in_narrow_terminals() {
        for width in [20, 40, 60, 80] {
            for total in [None, Some(8_010_000_000)] {
                let row = progress_line(
                    Paint(Colors::Basic),
                    1_000_000_000,
                    total,
                    0,
                    Duration::from_secs(120),
                    width,
                );
                let text = visible(&row);
                assert!(tui::width(&text) < width, "{width}: {text}");
                assert!(text.contains("120s elapsed"));
            }
        }
    }

    #[test]
    fn loading_and_retry_frames_show_real_elapsed_time_and_current_work() {
        let mut state = fixture(false);
        state.reach(Step::Download);
        state.phase_started = Instant::now() - Duration::from_secs(12);
        state.describe("Retry 1/2: connection failed; model.bin");
        let frame = visible(&state.frame(Paint(Colors::None), 80).join("\n"));
        assert!(
            frame.contains("Download pinned model files · 12s elapsed"),
            "{frame}"
        );
        assert!(
            frame.contains("Retry 1/2: connection failed; model.bin"),
            "{frame}"
        );
        state.describe("Receiving model.bin");
        let frame = visible(&state.frame(Paint(Colors::None), 80).join("\n"));
        assert!(!frame.contains("Retry 1/2"), "{frame}");
        assert!(frame.contains("Receiving model.bin"), "{frame}");
        state.reach(Step::Start);
        state.phase_started = Instant::now() - Duration::from_secs(45);
        state.describe("Loading voice models locally (up to 300s)");
        let frame = visible(&state.frame(Paint(Colors::None), 80).join("\n"));
        assert!(frame.contains("Start on this Mac · 45s elapsed"), "{frame}");
        assert!(
            frame.contains("Loading voice models locally (up to 300s)"),
            "{frame}"
        );
        assert!(
            !frame.contains('%'),
            "Unknown loading totals must not invent a percentage: {frame}"
        );
    }

    fn visible(line: &str) -> String {
        let mut out = String::new();
        let mut escape = false;
        for c in line.chars() {
            match (escape, c) {
                (false, '\x1b') => escape = true,
                (true, 'm') => escape = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    fn fixture(cached: bool) -> State {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "boubou-checklist-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let present = dir.join("present.bin");
        if cached {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&present, [0; 4]).unwrap();
        }
        State::new(
            "boubou run LFM3-Audio-2B".into(),
            vec![
                (Step::Check, "Check this Mac", "Apple M4".into()),
                (Step::Download, "Download pinned model files", String::new()),
                (Step::Verify, "Verify every file", "SHA-256".into()),
                (Step::Start, "Start on this Mac", String::new()),
            ],
            vec![
                Part {
                    name: "LFM3-Audio-2B".into(),
                    files: vec![(present, 4), (dir.join("missing.bin"), 1_000_000_000)],
                },
                Part {
                    name: "Speech engine".into(),
                    files: vec![(dir.join("engine.tar.gz"), 420_000_000)],
                },
            ],
        )
    }

    #[test]
    fn checklist_counts_real_bytes_and_marks_parts_as_their_files_finish() {
        let mut state = fixture(true);
        assert_eq!(state.download_total, 1_420_000_000);
        assert_eq!(state.verify_total, 1_420_000_004);
        let width = 80;
        let paint = Paint(Colors::None);
        let height = state.frame(paint, width).len();
        state.active = Step::Download;
        let engine = state
            .file_started(&state.files[2].path.clone(), Phase::Download, 420_000_000)
            .unwrap();
        state.advanced(Counted::Download(engine), 420_000_000);
        assert!(state.part_done(1));
        assert!(!state.part_done(0));
        let frame = visible(&state.frame(paint, width).join("\n"));
        assert!(frame.contains("0.42 / 1.42 GB"), "{frame}");
        assert!(frame.contains("✓ Speech engine"), "{frame}");
        assert!(frame.contains("○ LFM3-Audio-2B"), "{frame}");
        state.active = Step::Verify;
        state.advanced(Counted::Verify, 4);
        let frame = state.frame(paint, width);
        assert_eq!(
            frame.len(),
            height,
            "Frames keep one height for in-place redraws"
        );
        for line in &frame {
            assert!(tui::width(&visible(line)) <= width, "{line}");
        }
        assert!(visible(&frame.join("\n")).contains("✓  Download"));
        let _ = std::fs::remove_dir_all(state.files[0].path.parent().unwrap());
    }

    #[test]
    fn the_toucan_heads_the_checklist_where_there_is_room_for_it() {
        let mut state = fixture(false);
        let paint = Paint(Colors::Basic);
        let wide = state.frame(paint, 80);
        assert!(wide[1].contains("\x1b[43m "), "No toucan beak");
        let top = visible(&wide[0]);
        assert!(top.trim_start().starts_with("boubou "), "{top}");
        assert!(top.ends_with(&format!("○ {HERE}")), "{top}");
        assert_eq!(visible(&wide[2]).trim(), "$ boubou run LFM3-Audio-2B");
        // A wider window stretches nothing: no line can wrap and cut the toucan in two when the
        // window is made narrower again, and a long command breaks beside it.
        assert_eq!(state.frame(paint, 200)[..6], wide[..6]);
        state.command = "boubou serve qwen3-tts:0.6b-customvoice".into();
        let long = state.frame(paint, 200);
        assert_eq!(long.len(), wide.len());
        assert_eq!(visible(&long[2]).trim(), "$ boubou serve");
        assert_eq!(visible(&long[3]).trim(), "qwen3-tts:0.6b-customvoice");
        for line in &long[..6] {
            assert!(tui::width(&visible(line)) <= 58, "{line}");
        }
        state.command = "boubou run LFM3-Audio-2B".into();
        state.ready = true;
        let top = &state.frame(paint, 80)[0];
        assert!(top.ends_with(&format!("\x1b[33m● {HERE}")), "{top}");
        // A narrow terminal keeps its columns for the text.
        let narrow = state.frame(paint, 59);
        assert_eq!(narrow.len() + 3, wide.len());
        assert_eq!(visible(&narrow[2]), "  $ boubou run LFM3-Audio-2B");
        assert!(!narrow[..3].concat().contains("\x1b[43m"), "A beak");
    }

    #[test]
    fn a_cached_file_that_fails_verification_joins_the_download() {
        let mut state = fixture(true);
        let present = state.files[0].path.clone();
        state.file_started(&present, Phase::Download, 4).unwrap();
        assert_eq!(state.download_total, 1_420_000_004);
        let _ = std::fs::remove_dir_all(present.parent().unwrap());
        let mut state = fixture(false);
        state.failed = true;
        state.active = Step::Verify;
        let frame = visible(&state.frame(Paint(Colors::None), 40).join("\n"));
        assert!(frame.contains("✗"));
        assert!(frame.contains("✓  Download"));
    }

    #[test]
    fn an_engine_update_counts_the_changed_files_it_fetches() {
        let mut state = fixture(true);
        let engine = state.files[2].path.clone();
        state
            .file_started(&engine, Phase::Download, 3_000_000)
            .unwrap();
        assert_eq!(state.download_total, 1_003_000_000);
        assert_eq!(state.verify_total, 1_003_000_004);
        assert_eq!(amount(1_330_000, 1_330_000), "1.33 / 1.33 MB");
        assert_eq!(amount(620_000_000, 1_420_000_000), "0.62 / 1.42 GB");
        let _ = std::fs::remove_dir_all(state.files[0].path.parent().unwrap());
    }

    #[test]
    fn pull_installs_during_its_last_step_and_shows_a_failure_there() {
        let mut state = fixture(false);
        state.steps.pop();
        state.reach(Step::Verify);
        state.reach(Step::Start);
        assert_eq!(state.active, Step::Verify);
        state.failed = true;
        let frame = visible(&state.frame(Paint(Colors::None), 60).join("\n"));
        assert!(frame.contains("✗  Verify"), "{frame}");
    }

    #[test]
    fn a_full_sized_partial_is_already_downloaded() {
        let state = fixture(false);
        let dir = state.files[1].path.parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("small.bin.part"), [0; 4]).unwrap();
        let state = State::new(
            "boubou pull LFM3-Audio-2B".into(),
            Vec::new(),
            vec![Part {
                name: "LFM3-Audio-2B".into(),
                files: vec![(dir.join("small.bin"), 4)],
            }],
        );
        assert_eq!(state.download_total, 0);
        assert_eq!(state.verify_total, 4);
        let _ = std::fs::remove_dir_all(dir);
    }
}
