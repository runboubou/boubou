use crate::{assets::Assets, profiles::Profile, progress::Progress};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    fs::{self, OpenOptions},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

/// The backend lives only as long as this CLI invocation.
pub struct Backend {
    profile: &'static Profile,
    child: Child,
    pub address: String,
    log_path: PathBuf,
}

impl Backend {
    pub fn start(assets: &Assets, cache: &Path, stopped: &AtomicBool) -> Result<Self> {
        let profile = assets.profile;
        crate::check_cancel(stopped)?;
        let _progress = Progress::new(&format!("Loading {} locally", profile.id), None);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let address = format!("127.0.0.1:{port}");
        let logs = cache.join("logs");
        fs::create_dir_all(&logs)?;
        let log_path = logs.join(format!("backend-{}.log", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let log = options.open(&log_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            log.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        drop(listener);

        let child = Command::new(&assets.server)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .arg("--model")
            .arg(&assets.model)
            .arg("--mmproj")
            .arg(&assets.projector)
            .args([
                "--ctx-size",
                &profile.context_tokens.to_string(),
                "--parallel",
                "1",
                "--n-gpu-layers",
                "99",
                "--no-webui",
                "--offline",
            ])
            .args(profile.backend_args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("Cannot start {}", assets.server.display()))?;
        let mut backend = Self {
            profile,
            child,
            address,
            log_path,
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            crate::check_cancel(stopped)?;
            if let Some(status) = backend.child.try_wait()? {
                bail!(
                    "Liquid backend exited ({status}). See {}",
                    backend.log_path.display()
                );
            }
            if let Ok(response) = client
                .get(format!("http://{}/health", backend.address))
                .send()
                && response.status().is_success()
            {
                return Ok(backend);
            }
            if Instant::now() >= deadline {
                bail!(
                    "Liquid did not become ready within 120 seconds. See {}",
                    backend.log_path.display()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn explain_failure(&self, error: anyhow::Error) -> anyhow::Error {
        // b11960 aborts while reporting realtime context exhaustion. Its socket
        // reset loses the actual cause, which is still available in our own log.
        if fs::read_to_string(&self.log_path).is_ok_and(|log| {
            log.lines().any(|line| {
                line.contains("send_error: task id =")
                    && line.contains("error: the conversation does not fit in the context")
            })
        }) {
            return anyhow!(
                "This conversation reached Liquid's context capacity ({} tokens). Start a new conversation with `boubou run {} --offline`. Backend log: {}",
                self.profile.context_tokens,
                self.profile.id,
                self.log_path.display()
            );
        }
        if error.chain().any(|cause| cause.is::<tungstenite::Error>()) {
            return error.context(format!(
                "Local Liquid backend log: {}",
                self.log_path.display()
            ));
        }
        error
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_a_backend_that_never_becomes_ready() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, atomic::Ordering};
        let directory = std::env::temp_dir().join(format!(
            "boubou-start-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let server = directory.join("never-ready");
        fs::write(
            &server,
            "#!/bin/sh\nprintf '%s\\n' \"$$\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        let startup_log = directory.join(format!("logs/backend-{}.log", std::process::id()));
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !fs::read_to_string(&startup_log).is_ok_and(|text| !text.trim().is_empty()) {
                assert!(Instant::now() < deadline, "Fake backend never started");
                thread::sleep(Duration::from_millis(10));
            }
            signal.store(true, Ordering::SeqCst);
        });
        let began = Instant::now();
        let error = Backend::start(
            &Assets {
                profile: &crate::profiles::DEFAULT,
                server,
                model: PathBuf::new(),
                projector: PathBuf::new(),
            },
            &directory,
            &stopped,
        )
        .err()
        .expect("Startup must be cancelled");
        worker.join().unwrap();
        assert!(error.is::<crate::Cancelled>());
        assert!(began.elapsed() < Duration::from_secs(3));
        let log =
            fs::read_to_string(directory.join(format!("logs/backend-{}.log", std::process::id())))
                .unwrap();
        let pid: i32 = log.trim().parse().unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "Owned backend is still alive"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn backend_samples_reply_text_instead_of_decoding_greedily() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "boubou-sampling-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let server = directory.join("print-arguments");
        fs::write(&server, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 3\n").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
        let error = Backend::start(
            &Assets {
                profile: &crate::profiles::DEFAULT,
                server,
                model: PathBuf::new(),
                projector: PathBuf::new(),
            },
            &directory,
            &AtomicBool::new(false),
        )
        .err()
        .expect("The fake backend exits");
        assert!(error.to_string().contains("Liquid backend exited"));
        let log =
            fs::read_to_string(directory.join(format!("logs/backend-{}.log", std::process::id())))
                .unwrap();
        let arguments: Vec<&str> = log.lines().collect();
        let value = |flag: &str| {
            let at = arguments.iter().rposition(|argument| *argument == flag)?;
            arguments.get(at + 1).copied()
        };
        assert_eq!(
            value("--temp"),
            Some("0.3"),
            "Greedy decoding loops replies"
        );
        assert_eq!(value("--min-p"), Some("0.15"));
        assert_eq!(value("--top-k"), Some("0"));
        assert_eq!(value("--top-p"), Some("1.0"));
        assert_eq!(value("--repeat-penalty"), Some("1.05"));
        assert_eq!(value("--ctx-size"), Some("32768"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn selected_profile_controls_backend_arguments() {
        use std::os::unix::fs::PermissionsExt;
        let profile = &crate::profiles::OMNI;
        let directory = std::env::temp_dir().join(format!("boubou-profile-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let server = directory.join("print-arguments");
        fs::write(&server, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 3\n").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
        let result = Backend::start(
            &Assets {
                profile,
                server,
                model: directory
                    .join(profile.model_directory)
                    .join(profile.model.name),
                projector: directory
                    .join(profile.model_directory)
                    .join(profile.projector.name),
            },
            &directory,
            &AtomicBool::new(false),
        );
        assert!(result.is_err());
        let arguments =
            fs::read_to_string(directory.join(format!("logs/backend-{}.log", std::process::id())))
                .unwrap();
        assert!(arguments.contains("--ctx-size\n32768\n"));
        assert!(arguments.contains("--temp\n0\n"));
        assert!(arguments.contains("--reasoning\noff\n"));
        assert!(arguments.contains(profile.model.name));
        assert!(arguments.contains(profile.projector.name));
        assert!(!arguments.contains(crate::profiles::DEFAULT.model.name));
        assert!(!arguments.contains("--min-p"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn context_abort_reports_its_cause_and_other_failures_keep_theirs() {
        let log_path = std::env::temp_dir().join(format!(
            "boubou-context-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut log = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
            .unwrap();
        writeln!(
            log,
            "E srv    send_error: task id = 0, error: the conversation does not fit in the context"
        )
        .unwrap();
        let backend = Backend {
            profile: &crate::profiles::DEFAULT,
            child: Command::new("true").spawn().unwrap(),
            address: String::new(),
            log_path: log_path.clone(),
        };
        let error = backend.explain_failure(tungstenite::Error::ConnectionClosed.into());
        assert!(
            error
                .to_string()
                .contains("context capacity (32768 tokens)")
        );
        assert!(
            error
                .to_string()
                .contains("boubou run LFM3-Audio-2B --offline")
        );
        fs::write(
            &log_path,
            "normal startup; unrelated text: the conversation does not fit in the context",
        )
        .unwrap();
        let error = backend.explain_failure(tungstenite::Error::ConnectionClosed.into());
        assert!(error.to_string().contains("Local Liquid backend log:"));
        assert!(error.chain().any(|cause| cause.is::<tungstenite::Error>()));
        let error = backend.explain_failure(anyhow!("Microphone disconnected"));
        assert_eq!(error.to_string(), "Microphone disconnected");
        fs::remove_file(log_path).unwrap();
    }
}
