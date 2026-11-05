use crate::{
    Cancelled,
    profiles::{self, Model, Profile},
    qwen,
};
use anyhow::{Context, Result, ensure};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::{Color, ResetColor, Stylize},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    ffi::CString,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const GIB: u64 = 1024 * 1024 * 1024;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

pub struct Selection {
    pub model: Model,
    pub offline: bool,
}

struct Device {
    apple_silicon: bool,
    macos: (u32, u32, u32),
    memory: u64,
    available: u64,
    disk: u64,
}

impl Device {
    fn detect(cache: &Path) -> Result<Self> {
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
        Ok(Self {
            apple_silicon: cfg!(target_arch = "aarch64"),
            macos: qwen::macos_version()?,
            memory,
            available: available.min(memory),
            disk: u64::from(volume.f_bavail).saturating_mul(volume.f_frsize),
        })
    }

    fn check(&self, setup: &Setup) -> Result<()> {
        ensure!(
            self.apple_silicon,
            "Voice chat requires an Apple Silicon Mac."
        );
        let (major, minor, patch) = setup.resources.min_macos;
        ensure!(
            self.macos >= setup.resources.min_macos,
            "This voice needs macOS {major}.{minor}.{patch} or later. Update macOS and retry."
        );
        ensure!(
            self.memory >= setup.memory + 2 * GIB,
            "This voice needs about {:.0} GiB for speech and conversation, plus macOS.",
            setup.memory as f64 / GIB as f64
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
        let chip = if self.apple_silicon {
            "Apple Silicon"
        } else {
            "Unsupported architecture"
        };
        let (major, minor, patch) = self.macos;
        vec![
            format!(
                "{chip} / macOS {major}.{minor}.{patch} / {:.0} GiB memory",
                self.memory as f64 / GIB as f64
            ),
            format!(
                "About {:.1} GiB available memory / {:.1} GB free cache disk",
                self.available as f64 / GIB as f64,
                self.disk as f64 / 1_000_000_000.0
            ),
        ]
    }
}

#[derive(Clone, Copy)]
enum Language {
    English,
    Chinese,
}

struct Setup {
    model: Model,
    voice: &'static str,
    memory: u64,
    resources: qwen::Requirements,
}

impl Setup {
    fn for_language(language: Language, cache: &Path) -> Result<Self> {
        match language {
            Language::English => Ok(Self {
                model: Model::Liquid(&profiles::DEFAULT),
                voice: "English / default voice",
                memory: profiles::DEFAULT
                    .memory_budget
                    .context("No memory estimate for this voice")?,
                resources: liquid_requirements(&profiles::DEFAULT, cache),
            }),
            Language::Chinese => Ok(Self {
                model: Model::Qwen("qwen3-tts:0.6b-customvoice-whisper"),
                voice: "Chinese / Vivian voice (also understands English)",
                // Short whole-pipeline runs used about 8 GiB server RSS on a 128 GiB Mac.
                // Reserve 12 GiB for STT + LLM + TTS and headroom; smaller Macs are unverified.
                memory: 12 * GIB,
                resources: qwen::requirements("qwen3-tts:0.6b-customvoice-whisper", cache)
                    .context("Cannot check the Chinese voice setup")?,
            }),
        }
    }
}

fn liquid_requirements(model: &Profile, cache: &Path) -> qwen::Requirements {
    let folder = cache.join(model.model_directory);
    let mut download = 0;
    let downloads = cache.join("downloads");
    for (asset, directory) in model
        .model_assets()
        .map(|asset| (asset, folder.as_path()))
        .chain([(&model.runtime.archive, downloads.as_path())])
    {
        if !directory
            .join(asset.name)
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == asset.size)
        {
            download += asset.size;
        }
    }
    let staging = if cache
        .join(model.runtime.directory)
        .join(model.runtime.server)
        .is_file()
    {
        0
    } else {
        4 * model.runtime.archive.size
    };
    qwen::Requirements {
        min_macos: (13, 3, 0),
        download,
        disk: download + staging,
    }
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
    })
}

pub fn start(cache: &Path) -> Result<Selection> {
    let mut screen = Screen::enter()?;
    loop {
        let language = match screen.choose(
            "Choose your conversation language",
            &["Boubou will check this Mac before downloading anything.".into()],
            &[
                "English".into(),
                "Chinese".into(),
                "Developer: choose a model or paste a command".into(),
            ],
        )? {
            Some(0) => Language::English,
            Some(1) => Language::Chinese,
            Some(_) => {
                let mut text = String::new();
                let mut notice = String::new();
                while screen.input(&mut text, &notice)? {
                    match parse_selection(&text) {
                        Ok(selection) => return Ok(selection),
                        Err(error) => notice = error.to_string(),
                    }
                }
                continue;
            }
            None => return Err(Cancelled.into()),
        };
        let setup = match Setup::for_language(language, cache) {
            Ok(setup) => setup,
            Err(error) => {
                let mut details: Vec<_> = error.chain().map(ToString::to_string).collect();
                details
                    .push("Use a package with Chinese voice chat, or fix the cache error.".into());
                details.push("Then select the language again.".into());
                screen.choose(
                    "This voice setup is unavailable",
                    &details,
                    &["Change language".into()],
                )?;
                continue;
            }
        };
        loop {
            let device = match Device::detect(cache) {
                Ok(device) => device,
                Err(error) => {
                    if screen.choose(
                        "Device check unavailable",
                        &[error.to_string()],
                        &["Check again".into(), "Change language".into()],
                    )? == Some(0)
                    {
                        continue;
                    }
                    break;
                }
            };
            let mut details = device.summary();
            if let Err(error) = device.check(&setup) {
                details.push(error.to_string());
                details.push(
                    "No download has started. Choose a different voice or check again.".into(),
                );
                if screen.choose(
                    "This voice cannot start on this Mac right now",
                    &details,
                    &["Check again".into(), "Change language".into()],
                )? == Some(0)
                {
                    continue;
                }
                break;
            }
            details.extend([
                setup.voice.into(),
                format!(
                    "Estimated speech + conversation memory: {:.0} GiB",
                    setup.memory as f64 / GIB as f64
                ),
                format!(
                    "Estimated download: {:.2} GB / free disk needed: {:.2} GB",
                    setup.resources.download as f64 / 1_000_000_000.0,
                    setup.resources.disk as f64 / 1_000_000_000.0
                ),
                "8/16 GiB Macs: estimated fit only, not hardware-validated.".into(),
                "Speed and stability vary. Cached files are verified before use.".into(),
                "Downloads need internet; speech processing stays on this Mac.".into(),
            ]);
            if screen.choose(
                "Recommended voice setup",
                &details,
                &[
                    "Download, verify and start".into(),
                    "Change language".into(),
                ],
            )? != Some(0)
            {
                break;
            }
            // A user may leave the confirmation screen open while other apps consume memory.
            if let Err(error) = Device::detect(cache).and_then(|device| device.check(&setup)) {
                screen.choose(
                    "Resources changed before startup",
                    &[error.to_string()],
                    &["Check again".into()],
                )?;
                continue;
            }
            return Ok(Selection {
                model: setup.model,
                offline: false,
            });
        }
    }
}

pub fn explain_failure(error: anyhow::Error) -> anyhow::Error {
    if error.is::<Cancelled>() {
        return error;
    }
    error.context(
        "Voice chat could not start or continue. Close other apps if memory is low. \
         Check the connection for download failures, or microphone access in System Settings \
         > Privacy & Security > Microphone. Then run `boubou` again to recheck this Mac \
         and resume verified downloads. Details below",
    )
}

extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

struct Screen {
    signals: Vec<(libc::c_int, libc::sigaction)>,
    color: bool,
}

impl Screen {
    fn enter() -> Result<Self> {
        INTERRUPTED.store(false, Ordering::Relaxed);
        let mut screen = Self {
            signals: Vec::new(),
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
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
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste
        )?;
        Ok(screen)
    }

    fn line(&self, row: u16, text: &str, color: Option<Color>) -> Result<()> {
        let width = terminal::size()?.0.saturating_sub(4) as usize;
        let mut clipped = String::new();
        for c in text.chars() {
            if clipped.len() + c.len_utf8() > width {
                break;
            }
            clipped.push(c);
        }
        let mut out = io::stdout();
        queue!(out, MoveTo(2, row), ResetColor)?;
        if let Some(color) = color.filter(|_| self.color) {
            write!(out, "{}", clipped.with(color))?;
        } else {
            write!(out, "{clipped}")?;
        }
        Ok(())
    }

    fn heading(&self, title: &str) -> Result<()> {
        execute!(io::stdout(), Clear(ClearType::All))?;
        self.line(1, "Boubou", Some(Color::Magenta))?;
        self.line(3, title, None)
    }

    fn event(&self) -> Result<Event> {
        loop {
            if INTERRUPTED.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            if event::poll(Duration::from_millis(100))? {
                let event = event::read()?;
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
                return Ok(event);
            }
        }
    }

    fn choose(
        &mut self,
        title: &str,
        details: &[String],
        rows: &[String],
    ) -> Result<Option<usize>> {
        let mut selected = 0;
        loop {
            self.heading(title)?;
            for (i, detail) in details.iter().enumerate() {
                self.line(5 + i as u16, detail, None)?;
            }
            let first = 6 + details.len() as u16;
            let mut row_index = first;
            for (i, row) in rows.iter().enumerate() {
                for (part, line) in row.lines().enumerate() {
                    self.line(
                        row_index,
                        &format!(
                            "{} {line}",
                            if i == selected && part == 0 { ">" } else { " " }
                        ),
                        (i == selected).then_some(Color::DarkCyan),
                    )?;
                    row_index += 1;
                }
                row_index += 1;
            }
            self.line(
                row_index + 1,
                "Up/Down Select  Enter Continue  Esc Back  Ctrl+C Exit",
                None,
            )?;
            io::stdout().flush()?;
            match self.event()? {
                Event::Key(key) => match key.code {
                    KeyCode::Up => selected = (selected + rows.len() - 1) % rows.len(),
                    KeyCode::Down => selected = (selected + 1) % rows.len(),
                    KeyCode::Enter => return Ok(Some(selected)),
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                },
                Event::Resize(..) => {}
                _ => continue,
            }
        }
    }

    fn input(&mut self, text: &mut String, notice: &str) -> Result<bool> {
        loop {
            self.heading("Use a model you already chose")?;
            self.line(
                5,
                "Paste a model name or a run command from the model library.",
                None,
            )?;
            let width = terminal::size()?.0.saturating_sub(9) as usize;
            let tail: String = text
                .chars()
                .rev()
                .take(width)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let prefix = if tail.len() < text.len() { "..." } else { "" };
            self.line(7, &format!("> {prefix}{tail}"), Some(Color::DarkCyan))?;
            self.line(10, notice, Some(Color::DarkYellow))?;
            self.line(13, "Enter  Start    Esc  Back    Ctrl+C  Exit", None)?;
            io::stdout().flush()?;
            match self.event()? {
                Event::Key(key) => match key.code {
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
                Event::Paste(paste) if text.len() + paste.len() <= 1024 => {
                    text.extend(paste.chars().filter(|c| !c.is_control()));
                }
                _ => {}
            }
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            ResetColor,
            Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
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
        let setup = Setup::for_language(Language::English, &cache).unwrap();
        assert_eq!(setup.model.id(), DEFAULT.id);
        let mut device = Device {
            apple_silicon: true,
            macos: (13, 3, 0),
            memory: 8 * GIB,
            available: setup.memory,
            disk: setup.resources.disk,
        };
        assert!(
            device.check(&setup).is_ok(),
            "Estimated 8 GiB fit at the boundary"
        );
        device.apple_silicon = false;
        assert!(
            device
                .check(&setup)
                .unwrap_err()
                .to_string()
                .contains("Apple Silicon")
        );
        device.apple_silicon = true;
        device.macos = (13, 2, 9);
        assert!(
            device
                .check(&setup)
                .unwrap_err()
                .to_string()
                .contains("macOS")
        );
        device.macos = (13, 3, 0);
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
            model: Model::Qwen("qwen3-tts:0.6b-customvoice-whisper"),
            voice: "Chinese / Vivian",
            memory: 12 * GIB,
            resources: qwen::Requirements {
                min_macos: (15, 0, 0),
                download: 1,
                disk: 1,
            },
        };
        let mut device = Device {
            apple_silicon: true,
            macos: (15, 0, 0),
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
        device.macos = (14, 7, 0);
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
    fn reads_device_resources_without_creating_the_cache() {
        if !cfg!(target_arch = "aarch64") {
            return;
        }
        let cache =
            std::env::temp_dir().join(format!("boubou-device-check-{}", std::process::id()));
        let device = Device::detect(&cache).unwrap();
        assert!(device.memory > 0 && device.available <= device.memory && device.disk > 0);
        assert!(!cache.exists());
    }
}
