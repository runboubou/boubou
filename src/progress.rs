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

/// A startup message: the active step's note on the checklist, a plain line otherwise.
pub fn say(text: &str) {
    let mut shown = false;
    update(|state| {
        shown = true;
        let active = state.active;
        if let Some(entry) = state.steps.iter_mut().find(|(s, ..)| *s == active) {
            entry.2 = text.into();
        }
    });
    if !shown {
        eprintln!("{text}");
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

/// Moves the checklist to `step`; earlier steps count as done.
pub fn step(step: Step) {
    update(|state| {
        if step > state.active {
            state.active = step;
        }
    });
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
                let cached = path
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.len() == size);
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
        }
    }

    fn file_started(&mut self, path: &Path, phase: Phase) -> Option<usize> {
        let index = self.files.iter().position(|f| f.path == path)?;
        if phase == Phase::Download {
            let file = &mut self.files[index];
            if file.cached {
                // A cached file failed verification and is downloaded again.
                file.cached = false;
                self.download_total += file.size;
            }
            self.download_done -= file.downloaded;
            file.downloaded = 0;
            self.downloading = Some(index);
        }
        Some(index)
    }

    fn advanced(&mut self, file: usize, phase: Phase, bytes: u64) {
        match phase {
            Phase::Download => {
                self.files[file].downloaded += bytes;
                self.download_done += bytes;
            }
            Phase::Verify => self.verify_done += bytes,
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

    /// One frame. Every frame has the same number of lines so it can be redrawn in place.
    fn frame(&self, paint: Paint, width: usize) -> Vec<String> {
        let room = width.saturating_sub(7).max(1);
        let spinner = tui::SPINNER[self.tick % tui::SPINNER.len()];
        let light = if self.ready {
            format!("{}● {HERE}", paint.fg(tui::ACCENT))
        } else {
            format!("{DIM}○ {HERE}")
        };
        let command = tui::fit(&self.command, width.saturating_sub(8 + HERE.len()));
        let gap = width.saturating_sub(4 + 2 + tui::width(&command) + 2 + HERE.len());
        let mut lines = vec![
            format!("  {DIM}${RESET} {command}{}{light}", " ".repeat(gap)),
            String::new(),
        ];
        for (step, title, note) in &self.steps {
            let status = self.status(*step);
            let (icon, title_style) = match status {
                Status::Done => ("✓".to_owned(), BOLD),
                Status::Active => (spinner.to_string(), BOLD),
                Status::Failed => (format!("{}✗", paint.fg(tui::WARN)), BOLD),
                Status::Pending => ("○".to_owned(), DIM),
            };
            let dim_icon = if status == Status::Pending { DIM } else { "" };
            lines.push(format!(
                "  {dim_icon}{icon}{RESET}  {title_style}{}",
                tui::fit(title, room)
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
                    format!(
                        "{note} · {} / {}",
                        gb_amount(self.verify_done.min(self.verify_total)),
                        tui::gb(self.verify_total)
                    )
                }
                _ => note.clone(),
            };
            lines.push(format!("     {DIM}{}", tui::fit(&note, room)));
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
        let amount = format!("{} / {}", gb_amount(done), tui::gb(self.download_total));
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

/// The number part of an amount: "0.62" of "0.62 / 1.42 GB".
fn gb_amount(bytes: u64) -> String {
    format!("{:.2}", bytes as f64 / 1e9)
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

/// One visible blocking phase. Byte totals are measured; unknown totals animate without a percentage.
pub struct Progress {
    bytes: Arc<AtomicU64>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    /// Set when the checklist shows this phase: (file index, phase).
    checklist: Option<(usize, Phase)>,
}

impl Progress {
    pub fn new(label: &str, total: Option<u64>) -> Self {
        let bytes = Arc::new(AtomicU64::new(0));
        let mut shown = false;
        update(|state| {
            shown = true;
            let active = state.active;
            if let Some(entry) = state.steps.iter_mut().find(|(s, ..)| *s == active)
                && matches!(active, Step::Start | Step::Check)
            {
                entry.2 = label.into();
            }
        });
        if shown {
            return Self {
                bytes,
                stop: None,
                worker: None,
                checklist: None,
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
                stop: None,
                worker: None,
                checklist: None,
            };
        }
        let position = bytes.clone();
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("startup-progress".into())
            .spawn(move || {
                let began = Instant::now();
                let mut tick = 0;
                loop {
                    let mut out = io::stderr().lock();
                    let _ = write!(
                        out,
                        "\r\x1b[2K  {}  {DIM}{}s{RESET}",
                        bar(paint, position.load(Ordering::Relaxed), total, tick),
                        began.elapsed().as_secs()
                    );
                    let _ = out.flush();
                    drop(out);
                    if stopped.recv_timeout(FRAME) != Err(mpsc::RecvTimeoutError::Timeout) {
                        break;
                    }
                    tick += 1;
                }
                // Preserve the phase heading, leaving the next message on a clean line.
                let _ = write!(io::stderr(), "\r\x1b[2K");
                let _ = io::stderr().flush();
            })
            .ok();
        Self {
            bytes,
            stop: Some(stop),
            worker,
            checklist: None,
        }
    }

    /// A pinned file being downloaded or hashed. The checklist counts it toward its step.
    pub fn file(label: &str, path: &Path, size: u64, phase: Phase) -> Self {
        let mut index = None;
        update(|state| index = state.file_started(path, phase));
        match index {
            Some(file) => Self {
                bytes: Arc::new(AtomicU64::new(0)),
                stop: None,
                worker: None,
                checklist: Some((file, phase)),
            },
            None => Self::new(label, Some(size)),
        }
    }

    pub fn advance(&self, bytes: u64) {
        if let Some((file, phase)) = self.checklist {
            update(|state| state.advanced(file, phase, bytes));
        } else {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }
}

fn bar(paint: Paint, bytes: u64, total: Option<u64>, tick: usize) -> String {
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
    format!(
        "{}  {percent:3}%  {DIM}{} / {}{RESET}",
        smooth_bar(paint, bytes, total, 24),
        gb_amount(bytes.min(total)),
        tui::gb(total)
    )
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some((file, Phase::Download)) = self.checklist {
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
    fn only_known_totals_show_a_bounded_percentage() {
        let paint = Paint(Colors::None);
        assert!(bar(paint, 25, Some(100), 0).contains(" 25%"));
        assert!(bar(paint, 200, Some(100), 0).contains("100%"));
        assert!(bar(paint, 0, Some(0), 0).contains("100%"));
        assert!(!bar(paint, 200, None, 0).contains('%'));
        assert_ne!(bar(paint, 0, None, 0), bar(paint, 0, None, 1));
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
            .file_started(&state.files[2].path.clone(), Phase::Download)
            .unwrap();
        state.advanced(engine, Phase::Download, 420_000_000);
        assert!(state.part_done(1));
        assert!(!state.part_done(0));
        let frame = visible(&state.frame(paint, width).join("\n"));
        assert!(frame.contains("0.42 / 1.42 GB"), "{frame}");
        assert!(frame.contains("✓ Speech engine"), "{frame}");
        assert!(frame.contains("○ LFM3-Audio-2B"), "{frame}");
        state.active = Step::Verify;
        state.advanced(0, Phase::Verify, 4);
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
    fn a_cached_file_that_fails_verification_joins_the_download() {
        let mut state = fixture(true);
        let present = state.files[0].path.clone();
        state.file_started(&present, Phase::Download).unwrap();
        assert_eq!(state.download_total, 1_420_000_004);
        let _ = std::fs::remove_dir_all(present.parent().unwrap());
        let mut state = fixture(false);
        state.failed = true;
        state.active = Step::Verify;
        let frame = visible(&state.frame(Paint(Colors::None), 40).join("\n"));
        assert!(frame.contains("✗"));
        assert!(frame.contains("✓  Download"));
    }
}
