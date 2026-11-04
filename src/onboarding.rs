use crate::{
    Cancelled,
    profiles::{self, Profile},
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
    pub model: &'static Profile,
    pub offline: bool,
}

struct Device {
    memory: u64,
    available: u64,
    disk: u64,
}

impl Device {
    fn detect(cache: &Path) -> Result<Self> {
        ensure!(
            cfg!(target_arch = "aarch64"),
            "Boubou currently requires an Apple Silicon Mac."
        );
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
            memory,
            available: available.min(memory),
            disk: u64::from(volume.f_bavail).saturating_mul(volume.f_frsize),
        })
    }

    fn check(&self, model: &Profile, cache: &Path) -> Result<()> {
        let budget = model
            .memory_budget
            .context("This model does not have a profiled memory budget yet.")?;
        ensure!(
            self.memory >= budget && self.available >= budget,
            "Needs about {:.0} GB free memory. Close other apps and try again.",
            budget as f64 / GIB as f64
        );
        ensure!(
            self.disk >= space_needed(model, cache),
            "Not enough cache disk space. Free some space and try again."
        );
        Ok(())
    }
}

fn space_needed(model: &Profile, cache: &Path) -> u64 {
    let folder = cache.join(model.model_directory);
    let mut bytes = 0;
    let downloads = cache.join("downloads");
    for (asset, directory) in model
        .model_assets()
        .map(|asset| (asset, folder.as_path()))
        .chain([(&model.runtime.archive, downloads.as_path())])
    {
        if !directory
            .join(asset.name)
            .metadata()
            .is_ok_and(|m| m.len() == asset.size)
        {
            bytes += asset.size;
        }
    }
    if !cache
        .join(model.runtime.directory)
        .join(model.runtime.server)
        .is_file()
    {
        // Allow the unpacked runtime and its staging copy as well as the archive.
        bytes += 4 * model.runtime.archive.size;
    }
    bytes
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
    let model = profiles::PROFILES
        .iter()
        .copied()
        .find(|p| p.id == args[0])
        .context("Model not supported. Check the name or update Boubou.")?;
    Ok(Selection {
        model,
        offline: args.len() == 2,
    })
}

pub fn start(cache: &Path) -> Result<Selection> {
    ensure!(
        cfg!(target_arch = "aarch64"),
        "Boubou currently requires an Apple Silicon Mac."
    );
    let mut screen = Screen::enter()?;
    loop {
        let route = screen.choose(
            "How would you like to start?",
            &[],
            &[
                "Recommend a model for my Mac".into(),
                "Use a model I already chose".into(),
            ],
        )?;
        let Some(route) = route else {
            return Err(Cancelled.into());
        };
        if route == 0 {
            let device = match Device::detect(cache) {
                Ok(device) => device,
                Err(error) => {
                    screen.choose(
                        "Device check unavailable",
                        &[
                            error.to_string(),
                            "You can still enter a model from the library.".into(),
                        ],
                        &["Back".into()],
                    )?;
                    continue;
                }
            };
            let models: Vec<_> = profiles::PROFILES
                .iter()
                .copied()
                .filter(|p| device.check(p, cache).is_ok())
                .take(2)
                .collect();
            let summary = format!(
                "Apple Silicon  /  {:.0} GB memory  /  about {:.1} GB available",
                device.memory as f64 / GIB as f64,
                device.available as f64 / GIB as f64
            );
            if models.is_empty() {
                let reason = profiles::PROFILES
                    .iter()
                    .find_map(|p| device.check(p, cache).err())
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "No models are registered.".into());
                screen.choose(
                    "No suitable model found right now",
                    &[summary, reason],
                    &["Back".into()],
                )?;
                continue;
            }
            let rows: Vec<_> = models
                .iter()
                .map(|p| {
                    format!(
                        "{}\n  {} voice chat  /  up to {:.2} GB additional disk space",
                        p.id,
                        p.voice_chat_language,
                        space_needed(p, cache) as f64 / 1_000_000_000.0
                    )
                })
                .collect();
            if let Some(index) = screen.choose(
                "Recommended for your Mac",
                &[
                    summary,
                    "Memory fit is estimated; speed varies by chip and other apps.".into(),
                ],
                &rows,
            )? {
                return Ok(Selection {
                    model: models[index],
                    offline: false,
                });
            }
        } else {
            let mut text = String::new();
            let mut notice = String::new();
            loop {
                if !screen.input(&mut text, &notice)? {
                    break;
                }
                match parse_selection(&text) {
                    Ok(selection) => return Ok(selection),
                    Err(error) => notice = error.to_string(),
                }
            }
        }
    }
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
            assert_eq!(selection.model.id, DEFAULT.id);
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
    fn only_recommends_profiles_that_fit_memory_and_disk() {
        let cache =
            std::env::temp_dir().join(format!("boubou-missing-cache-{}", std::process::id()));
        let budget = DEFAULT.memory_budget.unwrap();
        let space = space_needed(&DEFAULT, &cache);
        let mut device = Device {
            memory: 16 * GIB,
            available: budget,
            disk: space,
        };
        assert!(device.check(&DEFAULT, &cache).is_ok());
        device.available = budget - 1;
        assert!(device.check(&DEFAULT, &cache).is_err());
        device.available = budget;
        device.memory = budget - 1;
        assert!(device.check(&DEFAULT, &cache).is_err());
        device.memory = 16 * GIB;
        device.disk = space - 1;
        assert!(device.check(&DEFAULT, &cache).is_err());
        let unknown = Profile {
            memory_budget: None,
            ..DEFAULT
        };
        device.disk = u64::MAX;
        assert!(device.check(&unknown, &cache).is_err());
        assert!(!cache.exists(), "Eligibility must not create the cache");
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
