use crate::tui::{self, BOLD, DIM, Paint, RESET, REVERSE};
use crate::{
    Cancelled, modular,
    profiles::{self, Model, Profile},
    progress, update,
};
use anyhow::{Context, Result, ensure};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute, queue,
    style::ResetColor,
    terminal::{
        self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use std::{
    ffi::CString,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::Path,
    process::Stdio,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const GIB: u64 = 1024 * 1024 * 1024;
/// What the menu calls the computer it runs on.
const COMPUTER: &str = if cfg!(target_os = "macos") {
    "Mac"
} else {
    "computer"
};
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

pub struct Selection {
    pub model: Model,
    pub offline: bool,
    /// What the device check found, for the startup checklist.
    pub check: Option<String>,
}

/// The system this computer runs. Only on a Mac do voices need a chip and a version of it.
enum System {
    Mac {
        apple_silicon: bool,
        version: (u32, u32, u32),
    },
    /// The distribution, such as "Ubuntu 24.04". A Mac build makes one only in tests.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Linux(String),
}

struct Device {
    /// The chip or processor, such as "Apple M4".
    chip: Option<String>,
    system: System,
    memory: u64,
    available: u64,
    disk: u64,
}

/// The value of `key` in lines of `key<separator>value`, as /proc and /etc/os-release write them.
#[cfg(any(target_os = "linux", test))]
fn value<'a>(text: &'a str, key: &str, separator: char) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(separator)?;
        (name.trim() == key).then(|| value.trim().trim_matches('"'))
    })
}

/// Total and available memory from /proc/meminfo. MemTotal leaves out what the kernel and
/// firmware keep, so an 8 GiB computer reads about 7.6 GiB: the total rounds up to a whole GiB.
#[cfg(any(target_os = "linux", test))]
fn meminfo(text: &str) -> Option<(u64, u64)> {
    let bytes = |key| {
        let kib = value(text, key, ':')?.strip_suffix("kB")?.trim();
        Some(kib.parse::<u64>().ok()? * 1024)
    };
    let total = bytes("MemTotal")?.div_ceil(GIB) * GIB;
    Some((total, bytes("MemAvailable")?.min(total)))
}

/// A processor as /proc/cpuinfo names it, without the marks, core count and clock speed:
/// "AMD Ryzen 9 9950X 16-Core Processor" is "AMD Ryzen 9 9950X".
#[cfg(any(target_os = "linux", test))]
fn processor(name: &str) -> String {
    name.replace("(R)", "")
        .replace("(TM)", "")
        .split_whitespace()
        .take_while(|word| {
            !["CPU", "@", "with", "Processor"].contains(word) && !word.ends_with("-Core")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Device {
    fn detect(cache: &Path) -> Result<Self> {
        let existing = cache
            .ancestors()
            .find(|p| p.exists())
            .unwrap_or(Path::new("."));
        let path = CString::new(existing.as_os_str().as_bytes())?;
        let mut volume = unsafe { std::mem::zeroed::<libc::statvfs>() };
        ensure!(
            unsafe { libc::statvfs(path.as_ptr(), &mut volume) } == 0,
            "Cannot check free space on the model cache volume."
        );
        // The block count is 32 bits wide on macOS and 64 on Linux.
        #[allow(clippy::useless_conversion)]
        let blocks = u64::from(volume.f_bavail);
        Self::here(blocks.saturating_mul(volume.f_frsize))
    }

    /// This Mac's chip, system and memory, with `disk` free for the cache.
    #[cfg(target_os = "macos")]
    fn here(disk: u64) -> Result<Self> {
        let mut memory = 0_u64;
        let mut length = size_of_val(&memory);
        let mut stats = unsafe { std::mem::zeroed::<libc::vm_statistics64>() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // Darwin owns both output layouts. The host port is released after the snapshot.
        #[allow(deprecated)] // Darwin's stable Mach APIs; avoid another binding dependency.
        unsafe {
            ensure!(
                libc::sysctlbyname(
                    c"hw.memsize".as_ptr(),
                    (&mut memory as *mut u64).cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0
                ) == 0,
                "Cannot read this Mac's memory."
            );
            let host = libc::mach_host_self();
            let result = libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&mut stats as *mut libc::vm_statistics64).cast(),
                &mut count,
            );
            mach_port_deallocate(libc::mach_task_self(), host);
            ensure!(
                result == libc::KERN_SUCCESS,
                "Cannot estimate available memory."
            );
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        ensure!(page > 0, "Cannot read this Mac's memory page size.");
        // Inactive pages may be reclaimed; this is a snapshot estimate, not a speed guarantee.
        let available =
            (u64::from(stats.free_count) + u64::from(stats.inactive_count)) * page as u64;
        Ok(Self {
            chip: std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "machdep.cpu.brand_string"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty()),
            system: System::Mac {
                apple_silicon: cfg!(target_arch = "aarch64"),
                version: modular::macos_version()?,
            },
            memory,
            available: available.min(memory),
            disk,
        })
    }

    /// This computer's processor, distribution and memory, with `disk` free for the cache.
    #[cfg(target_os = "linux")]
    fn here(disk: u64) -> Result<Self> {
        let read = |path| std::fs::read_to_string(path).unwrap_or_default();
        let (memory, available) =
            meminfo(&read("/proc/meminfo")).context("Cannot read this computer's memory.")?;
        let release = read("/etc/os-release");
        let system: Vec<_> = ["NAME", "VERSION_ID"]
            .iter()
            .filter_map(|key| value(&release, key, '='))
            .collect();
        Ok(Self {
            chip: value(&read("/proc/cpuinfo"), "model name", ':').map(processor),
            system: System::Linux(if system.is_empty() {
                "Linux".into()
            } else {
                system.join(" ")
            }),
            memory,
            available,
            disk,
        })
    }

    /// The chip's name, or what an unnamed one is known by.
    fn chip(&self) -> &str {
        self.chip.as_deref().unwrap_or(match self.system {
            System::Mac { .. } => "Apple Silicon",
            System::Linux(_) if cfg!(target_arch = "aarch64") => "arm64",
            System::Linux(_) => "x86-64",
        })
    }

    /// The checklist's line: this computer's memory against what the voice needs.
    fn fit(&self, needs: u64) -> String {
        let memory = format!(
            "{:.0} GiB memory · needs about {:.0} GiB",
            self.memory as f64 / GIB as f64,
            needs as f64 / GIB as f64
        );
        if self.memory < needs + 2 * GIB {
            format!("{memory} · may be slow or fail to start")
        } else {
            format!("{} · {memory}", self.chip())
        }
    }

    fn check(&self, setup: &Setup) -> Result<()> {
        if let System::Mac {
            apple_silicon,
            version,
        } = self.system
        {
            ensure!(apple_silicon, "Voice chat requires an Apple Silicon Mac.");
            let (major, minor, patch) = setup.resources.min_macos;
            ensure!(
                version >= setup.resources.min_macos,
                "This voice needs macOS {major}.{minor}.{patch} or later. Update macOS and retry."
            );
        }
        ensure!(
            self.memory >= setup.memory + 2 * GIB,
            "This voice needs about {:.0} GiB for speech and conversation, plus {}.",
            setup.memory as f64 / GIB as f64,
            if cfg!(target_os = "macos") {
                "macOS"
            } else {
                "Linux"
            }
        );
        ensure!(
            self.available >= setup.memory,
            "Needs about {:.0} GiB available memory. Close other apps and check again.",
            setup.memory as f64 / GIB as f64
        );
        ensure!(
            self.disk >= setup.resources.disk,
            "Needs about {:.2} GB free cache disk space. Free some space and check again.",
            setup.resources.disk as f64 / 1_000_000_000.0
        );
        Ok(())
    }

    fn summary(&self) -> Vec<String> {
        let system = match &self.system {
            System::Mac {
                version: (major, minor, patch),
                ..
            } => format!("macOS {major}.{minor}.{patch}"),
            System::Linux(distribution) => distribution.clone(),
        };
        vec![
            format!(
                "{} · {system} · {:.0} GiB memory",
                self.chip(),
                self.memory as f64 / GIB as f64
            ),
            format!(
                "About {:.1} GiB memory available · {:.1} GB free disk",
                self.available as f64 / GIB as f64,
                self.disk as f64 / 1_000_000_000.0
            ),
        ]
    }
}

struct Setup {
    model: Model,
    voice: &'static str,
    memory: u64,
    resources: modular::Requirements,
    recommended_memory_gib: u64,
}

impl Setup {
    fn liquid(profile: &'static Profile, cache: &Path) -> Result<Self> {
        Ok(Self {
            model: Model::Liquid(profile),
            voice: if profile.id == profiles::OMNI.id {
                "Liquid Omni 2.17B"
            } else {
                "Liquid Audio 2B"
            },
            memory: profile
                .memory_budget
                .context("No memory estimate for this voice")?,
            resources: liquid_requirements(profile, cache)?,
            recommended_memory_gib: if profile.id == profiles::DEFAULT.id {
                8
            } else {
                16
            },
        })
    }

    /// The picker row: catalogue labels read "Name / voice / LLM".
    fn row(&self) -> Row {
        let mut label = self.voice.split(" / ");
        let name = label.next().unwrap_or(self.voice).to_owned();
        let rest: Vec<_> = label.collect();
        let preset = profiles::presets()
            .iter()
            .find(|preset| preset.id == self.model.id());
        let detail = if preset.is_some() {
            rest.join(" · ")
        } else {
            "One model hears and speaks".into()
        };
        let pipeline = preset.map_or_else(
            || "one model hears, thinks and speaks".to_owned(),
            |preset| format!("{} → {} → {}", preset.stt, preset.llm, preset.tts),
        );
        Row {
            name,
            detail,
            size: if self.resources.download == 0 {
                "ready".into()
            } else {
                tui::gb(self.resources.download)
            },
            command: Some(format!("{} run {}", update::program(), self.model.id())),
            about: vec![
                format!(
                    "Needs about {:.0} GiB memory (estimate) · {} GiB {COMPUTER} or larger · {} free disk",
                    self.memory as f64 / GIB as f64,
                    self.recommended_memory_gib,
                    tui::gb(self.resources.disk)
                ),
                format!("English · {pipeline}"),
            ],
        }
    }
}

/// One choice in a menu. Rows with a command are voices; the rest are actions.
struct Row {
    name: String,
    detail: String,
    size: String,
    command: Option<String>,
    about: Vec<String>,
}

impl Row {
    fn action(name: &str) -> Self {
        Self {
            name: name.into(),
            detail: String::new(),
            size: String::new(),
            command: None,
            about: Vec::new(),
        }
    }
}

/// Every voice and its run command.
const LIBRARY: &str = "runboubou.com/models";

/// The command that opens a web page, where a desktop can show one. Without a display, as over
/// SSH, xdg-open would start a text browser in the menu's terminal.
fn browser() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("open")
    } else {
        ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|display| !display.is_empty()))
            .then_some("xdg-open")
    }
}

/// A menu's last row: the model library opens, and a command copied there pastes into Boubou.
fn more_voices() -> Row {
    Row {
        about: vec![
            if browser().is_some() {
                "Opens the model library in your browser."
            } else {
                "Open the model library in a browser."
            }
            .into(),
            "Copy a command there, then paste it here.".into(),
        ],
        ..Row::action(&format!("More voices at {LIBRARY}"))
    }
}

fn available_setups<'a>(device: &Device, setups: &'a [Setup]) -> Vec<&'a Setup> {
    setups
        .iter()
        .filter(|setup| {
            device.memory >= setup.recommended_memory_gib * GIB && device.check(setup).is_ok()
        })
        .collect()
}

fn liquid_requirements(model: &Profile, cache: &Path) -> Result<modular::Requirements> {
    let runtime = model.runtime()?;
    let folder = cache.join(model.model_directory);
    let mut download = 0;
    let downloads = cache.join("downloads");
    for (asset, directory) in model
        .model_assets()
        .map(|asset| (asset, folder.as_path()))
        .chain([(&runtime.archive, downloads.as_path())])
    {
        if !directory
            .join(asset.name)
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == asset.size)
        {
            download += asset.size;
        }
    }
    let staging = if cache.join(runtime.directory).join(runtime.server).is_file() {
        0
    } else {
        4 * runtime.archive.size
    };
    Ok(modular::Requirements {
        min_macos: (13, 3, 0),
        download,
        disk: download + staging,
    })
}

fn parse_selection(text: &str) -> Result<Selection> {
    ensure!(
        !text.contains(['\n', '\r', '\0', ';', '|', '&', '>', '<', '`']) && !text.contains("$("),
        "Paste one Boubou run command or a model name."
    );
    let words =
        shlex::split(text).context("The command has an unfinished quote. Paste it again.")?;
    let mut args = words.as_slice();
    if args.first().is_some_and(|s| s == "npx") {
        args = &args[1..];
        while let Some(option) = args.first() {
            match option.as_str() {
                "--yes" | "-y" => args = &args[1..],
                "--cache" if args.len() >= 2 => args = &args[2..],
                _ => break,
            }
        }
        let package = args.first().context("The command is missing boubou.")?;
        ensure!(
            package == "boubou"
                || package
                    .strip_prefix("boubou@")
                    .is_some_and(|s| !s.is_empty()),
            "Paste a Boubou command from the model library."
        );
        args = &args[1..];
        ensure!(
            args.first().is_some_and(|s| s == "run"),
            "Use a model name or a boubou run command."
        );
        args = &args[1..];
    } else if args.first().is_some_and(|s| s == "boubou") {
        ensure!(
            args.get(1).is_some_and(|s| s == "run"),
            "Use a model name or a boubou run command."
        );
        args = &args[2..];
    }
    ensure!(
        args.len() == 1 || (args.len() == 2 && args[1] == "--offline"),
        "Use a model name or boubou run MODEL [--offline]."
    );
    let model =
        Model::find(&args[0]).context("Model not supported. Check the name or update Boubou.")?;
    Ok(Selection {
        model,
        offline: args.len() == 2,
        check: None,
    })
}

fn choose_model(screen: &mut Screen) -> Result<Option<Selection>> {
    // Without a browser, as over SSH, the paste screen still names the address.
    if let Some(open) = browser() {
        let mut open = std::process::Command::new(open);
        open.arg(format!("https://{LIBRARY}/"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // `open` returns at once. xdg-open may stay until the browser closes, so it runs on.
        if cfg!(target_os = "macos") {
            let _ = open.status();
        } else if let Ok(mut browser) = open.spawn() {
            std::thread::spawn(move || browser.wait());
        }
    }
    let mut text = String::new();
    let mut notice = String::new();
    while screen.input(&mut text, &notice)? {
        match parse_selection(&text) {
            Ok(selection) => return Ok(Some(selection)),
            Err(error) => notice = error.to_string(),
        }
    }
    Ok(None)
}

pub fn start(cache: &Path) -> Result<Selection> {
    let mut screen = Screen::enter()?;
    loop {
        let device = match Device::detect(cache) {
            Ok(device) => device,
            Err(error) => {
                match screen.choose(
                    "Device check unavailable",
                    &[error.to_string()],
                    &[Row::action("Check again"), more_voices()],
                )? {
                    Some(0) => continue,
                    Some(_) => {
                        if let Some(selection) = choose_model(&mut screen)? {
                            return Ok(selection);
                        }
                        continue;
                    }
                    None => return Err(Cancelled.into()),
                }
            }
        };
        let mut setups = vec![
            Setup::liquid(&profiles::DEFAULT, cache)?,
            Setup::liquid(&profiles::OMNI, cache)?,
        ];
        let mut unavailable = Vec::new();
        // On Linux the packaged voices need an NVIDIA GPU: the model library has their commands.
        if cfg!(target_os = "macos") && modular::packaged() {
            for preset in profiles::presets().iter().filter(|preset| preset.guided) {
                match modular::requirements(preset.id, cache) {
                    Ok(resources) => setups.push(Setup {
                        model: Model::Modular(preset.id),
                        voice: preset.label,
                        memory: preset.memory_gib * GIB,
                        resources,
                        recommended_memory_gib: preset.recommended_memory_gib,
                    }),
                    Err(error) => unavailable.push(format!("{}: {error}", preset.label)),
                }
            }
        }
        let mut details = device.summary();
        let choices = available_setups(&device, &setups);
        let title = if choices.is_empty() {
            if let Err(error) = device.check(&setups[0]) {
                details.push(error.to_string());
            }
            format!("No English voice fits this {COMPUTER} right now")
        } else {
            format!("Choose a voice for this {COMPUTER}")
        };
        if let Some(error) = unavailable.first() {
            details.push(format!("Some packaged voices are unavailable: {error}"));
        }
        let mut rows: Vec<_> = choices.iter().map(|setup| setup.row()).collect();
        if choices.is_empty() {
            rows.push(Row::action("Check again"));
        }
        rows.push(more_voices());
        let Some(selected) = screen.choose(&title, &details, &rows)? else {
            return Err(Cancelled.into());
        };
        if selected == rows.len() - 1 {
            if let Some(selection) = choose_model(&mut screen)? {
                return Ok(selection);
            }
            continue;
        }
        if choices.is_empty() {
            continue;
        }
        let setup = choices[selected];
        // A user may leave the picker open while other apps consume memory.
        let device = Device::detect(cache);
        if let Err(error) = device
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .and_then(|device| device.check(setup))
        {
            screen.choose(
                "Resources changed before startup",
                &[error.to_string()],
                &[Row::action("Check again")],
            )?;
            continue;
        }
        let device = device?;
        return Ok(Selection {
            model: setup.model,
            offline: false,
            check: Some(device.fit(setup.memory)),
        });
    }
}

/// What a command typed by hand tells its checklist: this computer's memory against the model's
/// estimate, before anything is downloaded.
pub fn fit(model: Model, cache: &Path) -> Option<String> {
    let needs = model.memory_gib()? * GIB;
    Some(Device::detect(cache).ok()?.fit(needs))
}

pub fn explain_failure(error: anyhow::Error) -> anyhow::Error {
    if error.is::<Cancelled>() {
        return error;
    }
    error.context(format!(
        "Voice chat could not start or continue. Close other apps if memory is low. \
         Check the connection for download failures, or {}. Then run `{}` again to recheck \
         this {COMPUTER} and resume verified downloads. Details below",
        if cfg!(target_os = "macos") {
            "microphone access in System Settings > Privacy & Security > Microphone"
        } else {
            "that a microphone is connected"
        },
        update::program()
    ))
}

extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

struct Screen {
    signals: Vec<(libc::c_int, libc::sigaction)>,
    paint: Paint,
    update_notice: Option<String>,
}

/// Where the last frame put clickable things.
#[derive(Default)]
struct Hits {
    rows: Vec<(u16, usize)>,
    confirm: Option<(u16, u16)>,
}

/// Leave raw mode and the alternate screen. Safe to repeat.
fn restore_terminal() {
    let _ = execute!(
        io::stdout(),
        ResetColor,
        Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

impl Screen {
    fn enter() -> Result<Self> {
        INTERRUPTED.store(false, Ordering::Relaxed);
        let mut screen = Self {
            signals: Vec::new(),
            paint: Paint::detect(),
            update_notice: None,
        };
        // Only the prompt temporarily owns these handlers; run retains its existing Ctrl+C path.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut previous = unsafe { std::mem::zeroed() };
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = interrupt as *const () as usize;
            ensure!(
                unsafe { libc::sigaction(signal, &action, &mut previous) } == 0,
                "Cannot enable terminal cancellation."
            );
            screen.signals.push((signal, previous));
        }
        // A panic message must land on the normal screen, not a raw alternate one.
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_terminal();
                previous(info);
            }));
        });
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        crate::watch_resizes();
        Ok(screen)
    }

    /// Draw `text` (which may carry escape codes) as row `y`, clearing what was there.
    fn put(&self, out: &mut impl Write, height: u16, y: &mut u16, text: &str) -> Result<()> {
        if *y < height {
            queue!(out, MoveTo(0, *y), Clear(ClearType::CurrentLine))?;
            write!(out, "{text}{RESET}")?;
        }
        *y += 1;
        Ok(())
    }

    /// The top of a screen: beside the toucan, the product name and the dim local-status light
    /// over a rule, then the screen's title and details.
    fn header(
        &mut self,
        out: &mut impl Write,
        size: (u16, u16),
        y: &mut u16,
        title: &str,
        details: &[String],
    ) -> Result<()> {
        self.update_notice = update::message();
        let lines = tui::header(
            self.paint,
            size.0 as usize,
            (DIM, &format!("○ {}", progress::HERE)),
            |room| {
                std::iter::once(format!("{BOLD}{}", tui::fit(title, room)))
                    .chain(
                        details
                            .iter()
                            .map(|detail| format!("{DIM}{}", tui::fit(detail, room))),
                    )
                    .collect()
            },
        );
        self.put(out, size.1, y, "")?;
        for line in lines {
            self.put(out, size.1, y, &line)?;
        }
        self.put(out, size.1, y, "")?;
        if let Some(notice) = self.update_notice.as_deref() {
            let (version, command) = notice.split_once(". Run: ").unwrap_or((notice, ""));
            for line in [version.to_owned(), format!("Run: {command}")] {
                self.put(
                    out,
                    size.1,
                    y,
                    &format!(
                        "  {}{}",
                        self.paint.fg(tui::ACCENT),
                        tui::fit(&line, usize::from(size.0).saturating_sub(4))
                    ),
                )?;
            }
        }
        Ok(())
    }

    fn event(&self) -> Result<Option<Event>> {
        loop {
            if INTERRUPTED.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            if let Some(event) = crate::next_event(Duration::from_millis(100))? {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Err(Cancelled.into());
                    }
                }
                return Ok(Some(event));
            }
            if update::message() != self.update_notice {
                return Ok(None);
            }
        }
    }

    fn row_line(&self, row: &Row, columns: &tui::Columns, selected: bool, hover: bool) -> String {
        let paint = self.paint;
        let mark = match (&row.command, selected) {
            (Some(_), true) => '◉',
            (Some(_), false) => '○',
            (None, _) => '›',
        };
        let (name, detail, size) = if row.command.is_none() {
            // Actions span the whole row.
            let span = columns.name + columns.detail + if columns.detail > 0 { 2 } else { 0 };
            (
                tui::pad(&row.name, span),
                String::new(),
                " ".repeat(columns.size),
            )
        } else {
            let detail = if columns.detail > 0 {
                format!("  {}", tui::pad(&row.detail, columns.detail))
            } else {
                String::new()
            };
            (
                tui::pad(&row.name, columns.name),
                detail,
                tui::pad_left(&row.size, columns.size),
            )
        };
        if selected && paint.colored() {
            let on = format!("{}{}", paint.bg(tui::ROW), paint.fg(tui::ROW_TEXT));
            format!("  {on} {mark}  {BOLD}{name}{RESET}{on}{detail}  {size} {RESET}")
        } else if selected {
            format!("  {BOLD} {mark}  {name}{detail}  {size} ")
        } else if hover {
            // Hover lifts the row to full brightness on light and dark themes alike.
            format!("   {mark}  {name}{detail}  {size} ")
        } else {
            format!("  {DIM} {mark}  {RESET}{name}{DIM}{detail}  {size} ")
        }
    }

    /// The run line under the list, styled like a command with a button.
    fn confirm_box(&self, row: &Row, width: usize, hover: bool) -> [String; 3] {
        let inner = width.saturating_sub(8);
        let (prompt, text, button) = match &row.command {
            Some(command) => ("$ ", command.as_str(), " Run locally ↵ "),
            None => ("", row.name.as_str(), " Continue ↵ "),
        };
        let button_width = tui::width(button);
        let text_width = inner.saturating_sub(button_width + 2 + prompt.len());
        let text = tui::pad(text, text_width);
        // Reverse video follows the theme: a light button on dark terminals and vice versa.
        let button = format!("{BOLD}{REVERSE}{button}{RESET}");
        let text = if hover {
            format!("{BOLD}{text}{RESET}")
        } else {
            text
        };
        let rule = "─".repeat(inner + 2);
        [
            format!("  {DIM}╭{rule}╮"),
            format!("  {DIM}│{RESET} {DIM}{prompt}{RESET}{text}  {button} {DIM}│"),
            format!("  {DIM}╰{rule}╯"),
        ]
    }

    fn draw(
        &mut self,
        title: &str,
        details: &[String],
        rows: &[Row],
        selected: usize,
        hover: Option<usize>,
        confirm_hover: bool,
    ) -> Result<Hits> {
        let size = terminal::size()?;
        let (width, height) = (size.0 as usize, size.1);
        // One write per frame: no half-drawn screens between reads.
        let mut out = Vec::new();
        queue!(out, BeginSynchronizedUpdate)?;
        let mut y = 0;
        let mut hits = Hits::default();
        self.header(&mut out, size, &mut y, title, details)?;
        let text = width.saturating_sub(4);
        let about = &rows[selected].about;
        // Header, title, blanks, the action gap, the about block, the run box and the footer.
        let fixed = y + 2 + about.len() as u16 + 1 + 3 + 2;
        let visible = (height.saturating_sub(fixed) as usize).max(1);
        let offset = selected / visible * visible;
        let voices = || rows.iter().filter(|row| row.command.is_some());
        let columns = tui::Columns::fit(
            voices().map(|row| row.name.as_str()),
            voices().map(|row| row.size.as_str()),
            width.saturating_sub(2),
        );
        for (i, row) in rows.iter().enumerate().skip(offset).take(visible) {
            // Set actions apart from the voices above them.
            if i > offset && row.command.is_none() && rows[i - 1].command.is_some() {
                self.put(&mut out, height, &mut y, "")?;
            }
            hits.rows.push((y, i));
            let line = self.row_line(row, &columns, i == selected, hover == Some(i));
            self.put(&mut out, height, &mut y, &line)?;
        }
        self.put(&mut out, height, &mut y, "")?;
        for line in about {
            self.put(
                &mut out,
                height,
                &mut y,
                &format!("  {DIM}{}", tui::fit(line, text)),
            )?;
        }
        if !about.is_empty() {
            self.put(&mut out, height, &mut y, "")?;
        }
        hits.confirm = Some((y, y + 2));
        for line in self.confirm_box(&rows[selected], width, confirm_hover) {
            self.put(&mut out, height, &mut y, &line)?;
        }
        let mut keys = String::from("↑↓ choose  ↵ ");
        keys.push_str(if rows[selected].command.is_some() {
            "run"
        } else {
            "continue"
        });
        keys.push_str("  esc back  ctrl+c exit  ·  click to choose");
        if rows.len() > visible {
            keys.push_str(&format!(
                "  ·  {}-{} of {}",
                offset + 1,
                (offset + visible).min(rows.len()),
                rows.len()
            ));
        }
        self.put(
            &mut out,
            height,
            &mut y,
            &format!("  {DIM}{}", tui::fit(&keys, text)),
        )?;
        if y < height {
            queue!(out, MoveTo(0, y), Clear(ClearType::FromCursorDown))?;
        }
        queue!(out, EndSynchronizedUpdate)?;
        let mut stdout = io::stdout().lock();
        stdout.write_all(&out)?;
        stdout.flush()?;
        Ok(hits)
    }

    fn choose(&mut self, title: &str, details: &[String], rows: &[Row]) -> Result<Option<usize>> {
        let mut selected = 0;
        let mut hover = None;
        let mut confirm_hover = false;
        execute!(io::stdout(), Clear(ClearType::All))?;
        loop {
            let hits = self.draw(title, details, rows, selected, hover, confirm_hover)?;
            let hit_row = |y: u16| hits.rows.iter().find(|(row, _)| *row == y).map(|(_, i)| *i);
            let on_confirm = |y: u16| {
                hits.confirm
                    .is_some_and(|(top, bottom)| (top..=bottom).contains(&y))
            };
            match self.event()? {
                Some(Event::Key(key)) => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = (selected + rows.len() - 1) % rows.len()
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                        selected = (selected + 1) % rows.len()
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = rows.len() - 1,
                    KeyCode::Enter => return Ok(Some(selected)),
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    _ => {}
                },
                Some(Event::Mouse(mouse)) => {
                    hover = hit_row(mouse.row);
                    confirm_hover = on_confirm(mouse.row);
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(row) = hover {
                                selected = row;
                            } else if confirm_hover {
                                return Ok(Some(selected));
                            }
                        }
                        MouseEventKind::ScrollUp => selected = selected.saturating_sub(1),
                        MouseEventKind::ScrollDown => selected = (selected + 1).min(rows.len() - 1),
                        _ => {}
                    }
                }
                Some(Event::Resize(..)) => execute!(io::stdout(), Clear(ClearType::All))?,
                _ => {}
            }
        }
    }

    fn input(&mut self, text: &mut String, notice: &str) -> Result<bool> {
        execute!(io::stdout(), Clear(ClearType::All))?;
        loop {
            let size = terminal::size()?;
            let (width, height) = (size.0 as usize, size.1);
            let mut out = Vec::new();
            queue!(out, BeginSynchronizedUpdate)?;
            let mut y = 0;
            self.header(
                &mut out,
                size,
                &mut y,
                "More voices",
                &[
                    format!("Paste a run command from {LIBRARY}."),
                    "A model name works too.".into(),
                ],
            )?;
            let room = width.saturating_sub(4);
            let inner = width.saturating_sub(8);
            // Keep the end of long input in view, marking the hidden start.
            let field = inner.saturating_sub(3);
            let mut tail: String = text
                .chars()
                .rev()
                .take(field)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if tail.len() < text.len() {
                tail = format!("…{}", tail.chars().skip(1).collect::<String>());
            }
            let rule = "─".repeat(inner + 2);
            self.put(&mut out, height, &mut y, &format!("  {DIM}╭{rule}╮"))?;
            self.put(
                &mut out,
                height,
                &mut y,
                &format!(
                    "  {DIM}│{RESET} {DIM}${RESET} {}{REVERSE} {RESET} {DIM}│",
                    tui::pad(&tail, field)
                ),
            )?;
            self.put(&mut out, height, &mut y, &format!("  {DIM}╰{rule}╯"))?;
            self.put(&mut out, height, &mut y, "")?;
            let notice = tui::fit(notice, room);
            self.put(
                &mut out,
                height,
                &mut y,
                &format!("  {}{notice}", self.paint.fg(tui::WARN)),
            )?;
            self.put(&mut out, height, &mut y, "")?;
            self.put(
                &mut out,
                height,
                &mut y,
                &format!(
                    "  {DIM}{}",
                    tui::fit("↵ start  esc back  ctrl+u clear  ctrl+c exit", room)
                ),
            )?;
            if y < height {
                queue!(out, MoveTo(0, y), Clear(ClearType::FromCursorDown))?;
            }
            queue!(out, EndSynchronizedUpdate)?;
            let mut stdout = io::stdout().lock();
            stdout.write_all(&out)?;
            stdout.flush()?;
            drop(stdout);
            match self.event()? {
                Some(Event::Key(key)) => match key.code {
                    KeyCode::Enter => return Ok(true),
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        text.clear()
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c)
                        if !c.is_control()
                            && !key.modifiers.contains(KeyModifiers::CONTROL)
                            && text.len() < 1024 =>
                    {
                        text.push(c)
                    }
                    _ => {}
                },
                Some(Event::Paste(paste)) if text.len() + paste.len() <= 1024 => {
                    text.extend(paste.chars().filter(|c| !c.is_control()));
                }
                Some(Event::Resize(..)) => execute!(io::stdout(), Clear(ClearType::All))?,
                _ => {}
            }
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore_terminal();
        for (signal, previous) in &self.signals {
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::DEFAULT;

    fn mac(version: (u32, u32, u32)) -> System {
        System::Mac {
            apple_silicon: true,
            version,
        }
    }

    fn catalog_setups() -> Vec<Setup> {
        let mut setups = vec![
            Setup::liquid(&profiles::DEFAULT, Path::new("/nonexistent-boubou-cache")).unwrap(),
            Setup::liquid(&profiles::OMNI, Path::new("/nonexistent-boubou-cache")).unwrap(),
        ];
        setups.extend(
            profiles::presets()
                .iter()
                .filter(|preset| preset.guided)
                .map(|preset| Setup {
                    model: Model::Modular(preset.id),
                    voice: preset.label,
                    memory: preset.memory_gib * GIB,
                    resources: modular::Requirements {
                        min_macos: (15, 0, 0),
                        download: GIB,
                        disk: GIB,
                    },
                    recommended_memory_gib: preset.recommended_memory_gib,
                }),
        );
        setups
    }

    #[test]
    fn offers_liquid_first_then_the_guided_voices_that_fit() {
        let setups = catalog_setups();
        for total in [8, 16, 24, 32, 64, 128] {
            let device = Device {
                chip: Some("Apple Silicon".into()),
                system: mac((15, 0, 0)),
                memory: total * GIB,
                available: if total == 8 {
                    6 * GIB
                } else {
                    (total - 4) * GIB
                },
                disk: 100 * GIB,
            };
            let actual: Vec<_> = available_setups(&device, &setups)
                .iter()
                .map(|setup| setup.model.id())
                .collect();
            let expected = if total == 8 {
                vec![DEFAULT.id]
            } else {
                vec![
                    DEFAULT.id,
                    profiles::OMNI.id,
                    "qwen3-tts:0.6b-customvoice",
                    "pocket-tts:english_2026-10",
                    "phonon:31m-en",
                ]
            };
            assert_eq!(actual, expected, "{total} GiB");
        }
    }

    #[test]
    fn the_checklist_says_when_a_mac_has_less_memory_than_the_voice_needs() {
        let mut device = Device {
            chip: Some("Apple M4".into()),
            system: mac((15, 0, 0)),
            memory: 16 * GIB,
            available: 16 * GIB,
            disk: 100 * GIB,
        };
        let needs = |id| Model::Modular(id).memory_gib().unwrap() * GIB;
        assert_eq!(
            device.fit(needs("qwen3-tts:0.6b-customvoice")),
            "Apple M4 · 16 GiB memory · needs about 12 GiB"
        );
        // The Whisper entry a voice of another language starts needs what its English preset does.
        device.memory = 8 * GIB;
        assert_eq!(
            device.fit(needs("qwen3-tts:0.6b-customvoice-whisper")),
            "8 GiB memory · needs about 12 GiB · may be slow or fail to start"
        );
        assert_eq!(
            device.fit(Model::Liquid(&DEFAULT).memory_gib().unwrap() * GIB),
            "Apple M4 · 8 GiB memory · needs about 6 GiB"
        );
        // Models without an estimate keep the plain system line.
        assert_eq!(Model::Modular("supertonic:3").memory_gib(), None);
    }

    #[test]
    fn busy_high_memory_macs_offer_only_currently_usable_choices() {
        let setups = catalog_setups();
        let mut device = Device {
            chip: None,
            system: mac((15, 0, 0)),
            memory: 128 * GIB,
            available: 15 * GIB,
            disk: 100 * GIB,
        };
        assert_eq!(available_setups(&device, &setups).len(), 5);
        device.available = 11 * GIB;
        let choices = available_setups(&device, &setups);
        assert_eq!(
            choices
                .iter()
                .map(|setup| setup.model.id())
                .collect::<Vec<_>>(),
            vec![DEFAULT.id, profiles::OMNI.id]
        );
        device.available = 5 * GIB;
        assert!(available_setups(&device, &setups).is_empty());
    }

    #[test]
    fn recommendation_filters_os_architecture_and_cache_disk() {
        let setups = catalog_setups();
        let mut device = Device {
            chip: None,
            system: mac((14, 7, 0)),
            memory: 128 * GIB,
            available: 100 * GIB,
            disk: 100 * GIB,
        };
        assert_eq!(available_setups(&device, &setups).len(), 2);
        device.system = mac((13, 2, 9));
        assert!(available_setups(&device, &setups).is_empty());
        device.system = System::Mac {
            apple_silicon: false,
            version: (15, 0, 0),
        };
        assert!(available_setups(&device, &setups).is_empty());
        device.system = mac((15, 0, 0));
        device.disk = 0;
        assert!(available_setups(&device, &setups).is_empty());
        device.disk = 3 * GIB;
        assert_eq!(available_setups(&device, &setups)[0].model.id(), DEFAULT.id);
    }

    #[test]
    fn accepts_model_and_website_commands_without_running_a_shell() {
        for command in [
            "LFM3-Audio-2B",
            "boubou run LFM3-Audio-2B",
            "npx boubou@beta run LFM3-Audio-2B",
            "npx --yes --cache \"$HOME/a cache\" boubou@beta run LFM3-Audio-2B",
        ] {
            let selection = parse_selection(command).unwrap();
            assert_eq!(selection.model.id(), DEFAULT.id);
            assert!(!selection.offline);
        }
        assert!(
            parse_selection("boubou run LFM3-Audio-2B --offline")
                .unwrap()
                .offline
        );
    }

    #[test]
    fn rejects_invalid_ids_options_and_shell_syntax() {
        for command in [
            "",
            "missing",
            "boubou",
            "boubou pull LFM3-Audio-2B",
            "boubou run LFM3-Audio-2B --input personal.wav",
            "npx other-app run LFM3-Audio-2B",
            "npx --cache",
            "'unfinished",
            "LFM3-Audio-2B; echo bad",
            "LFM3-Audio-2B | cat",
            "LFM3-Audio-2B\necho bad",
            "$(echo LFM3-Audio-2B)",
        ] {
            assert!(parse_selection(command).is_err(), "Accepted {command}");
        }
    }

    #[test]
    fn english_fit_respects_architecture_os_memory_and_cache_space() {
        let cache =
            std::env::temp_dir().join(format!("boubou-missing-cache-{}", std::process::id()));
        let setup = Setup::liquid(&profiles::DEFAULT, &cache).unwrap();
        assert_eq!(setup.model.id(), DEFAULT.id);
        let omni = Setup::liquid(&profiles::OMNI, &cache).unwrap();
        assert_eq!(omni.model.id(), profiles::OMNI.id);
        assert!(omni.resources.download > setup.resources.download);
        let mut device = Device {
            chip: None,
            system: mac((13, 3, 0)),
            memory: 8 * GIB,
            available: setup.memory,
            disk: setup.resources.disk,
        };
        assert!(
            device.check(&setup).is_ok(),
            "Estimated 8 GiB fit at the boundary"
        );
        device.system = System::Mac {
            apple_silicon: false,
            version: (13, 3, 0),
        };
        assert!(
            device
                .check(&setup)
                .unwrap_err()
                .to_string()
                .contains("Apple Silicon")
        );
        device.system = mac((13, 2, 9));
        assert!(
            device
                .check(&setup)
                .unwrap_err()
                .to_string()
                .contains("macOS")
        );
        device.system = mac((13, 3, 0));
        device.available = setup.memory - 1;
        assert!(
            device
                .check(&setup)
                .unwrap_err()
                .to_string()
                .contains("Close other apps")
        );
        device.available = setup.memory;
        device.memory -= 1;
        assert!(device.check(&setup).is_err(), "Keep room for macOS");
        device.memory = 8 * GIB;
        device.disk -= 1;
        assert!(
            device
                .check(&setup)
                .unwrap_err()
                .to_string()
                .contains("Free some space")
        );
        assert!(!cache.exists(), "Planning must not create the cache");
    }

    #[test]
    fn whole_pipeline_budget_filters_small_and_busy_macs() {
        let setup = Setup {
            model: Model::Modular("qwen3-tts:0.6b-customvoice"),
            voice: "Qwen / Aiden",
            recommended_memory_gib: 16,
            memory: 12 * GIB,
            resources: modular::Requirements {
                min_macos: (15, 0, 0),
                download: 1,
                disk: 1,
            },
        };
        let mut device = Device {
            chip: None,
            system: mac((15, 0, 0)),
            memory: 8 * GIB,
            available: 8 * GIB,
            disk: 1,
        };
        assert!(device.check(&setup).is_err());
        device.memory = 16 * GIB;
        assert!(
            device.check(&setup).is_err(),
            "Total memory alone cannot establish fit"
        );
        device.available = 12 * GIB;
        assert!(
            device.check(&setup).is_ok(),
            "Estimated fit, not hardware validation"
        );
        device.system = mac((14, 7, 0));
        assert!(
            device.check(&setup).is_err(),
            "The packaged MLX backend needs macOS 15"
        );
    }

    #[test]
    fn guided_failure_keeps_the_cause_and_an_actionable_restart() {
        let error = explain_failure(anyhow::anyhow!("Not enough memory to load speech"));
        let text = format!("{error:#}");
        assert!(text.contains("Close other apps"));
        assert!(text.contains("run `boubou` again"));
        assert!(text.contains("Not enough memory to load speech"));
        assert!(explain_failure(Cancelled.into()).is::<Cancelled>());
    }

    #[test]
    fn linux_reads_its_processor_distribution_and_memory_from_text() {
        // An 8 GiB computer reads about 7.5 GiB and counts as the 8 GiB its voices are sized for.
        assert_eq!(
            meminfo(
                "MemTotal:        7907396 kB\nMemFree:  210288 kB\nMemAvailable:    5924316 kB\n"
            ),
            Some((8 * GIB, 5_924_316 * 1024))
        );
        assert_eq!(meminfo("MemTotal:        7907396 kB\n"), None);
        let release = "PRETTY_NAME=\"Ubuntu 24.04.4 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\n";
        assert_eq!(value(release, "NAME", '='), Some("Ubuntu"));
        assert_eq!(value(release, "VERSION_ID", '='), Some("24.04"));
        assert_eq!(value(release, "VERSION", '='), None);
        let cpuinfo = "processor\t: 0\nmodel name\t: AMD Ryzen 9 9950X 16-Core Processor\n";
        for (name, short) in [
            (
                value(cpuinfo, "model name", ':').unwrap(),
                "AMD Ryzen 9 9950X",
            ),
            (
                "Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz",
                "Intel Core i7-8700K",
            ),
            (
                "12th Gen Intel(R) Core(TM) i7-12700K",
                "12th Gen Intel Core i7-12700K",
            ),
            (
                "AMD Ryzen 7 5800H with Radeon Graphics",
                "AMD Ryzen 7 5800H",
            ),
        ] {
            assert_eq!(processor(name), short);
        }
    }

    #[test]
    fn a_linux_computer_is_offered_the_liquid_voices_that_fit() {
        let cache = Path::new("/nonexistent-boubou-cache");
        let setups = [
            Setup::liquid(&profiles::DEFAULT, cache).unwrap(),
            Setup::liquid(&profiles::OMNI, cache).unwrap(),
        ];
        let mut device = Device {
            chip: Some("AMD Ryzen 9 9950X".into()),
            system: System::Linux("Ubuntu 24.04".into()),
            memory: 8 * GIB,
            available: 6 * GIB,
            disk: 100 * GIB,
        };
        let offered = |device: &Device| {
            available_setups(device, &setups)
                .iter()
                .map(|setup| setup.model.id())
                .collect::<Vec<_>>()
        };
        assert_eq!(offered(&device), [DEFAULT.id]);
        device.memory = 16 * GIB;
        assert_eq!(offered(&device), [DEFAULT.id, profiles::OMNI.id]);
        assert_eq!(
            device.summary()[0],
            "AMD Ryzen 9 9950X · Ubuntu 24.04 · 16 GiB memory"
        );
        assert_eq!(
            device.fit(setups[0].memory),
            "AMD Ryzen 9 9950X · 16 GiB memory · needs about 6 GiB"
        );
        device.available = 5 * GIB;
        assert!(offered(&device).is_empty());
        // A processor without a name, as on most arm64 boards, is known by its architecture.
        device.chip = None;
        assert!(["x86-64", "arm64"].contains(&device.chip()));
    }

    #[test]
    fn reads_device_resources_without_creating_the_cache() {
        if cfg!(all(target_os = "macos", not(target_arch = "aarch64"))) {
            return;
        }
        let cache =
            std::env::temp_dir().join(format!("boubou-device-check-{}", std::process::id()));
        let device = Device::detect(&cache).unwrap();
        assert!(device.memory > 0 && device.available <= device.memory && device.disk > 0);
        assert!(!cache.exists());
    }
}
