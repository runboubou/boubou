use crate::assets::Assets;
use anyhow::{Context, Result, anyhow, bail};
use std::{
    fs::{self, File},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const CONTEXT_TOKENS: u32 = 32_768;

/// The backend lives only as long as this CLI invocation.
pub struct Backend {
    child: Child,
    pub address: String,
    log_path: PathBuf,
}

impl Backend {
    pub fn start(assets: &Assets, cache: &Path) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let address = format!("127.0.0.1:{port}");
        let logs = cache.join("logs");
        fs::create_dir_all(&logs)?;
        let log_path = logs.join(format!("backend-{}.log", std::process::id()));
        let log = File::create(&log_path)?;
        drop(listener);

        let child = Command::new(&assets.server)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .arg("--model")
            .arg(&assets.model)
            .arg("--mmproj")
            .arg(&assets.projector)
            .args([
                "--ctx-size",
                &CONTEXT_TOKENS.to_string(),
                "--parallel",
                "1",
                "--n-gpu-layers",
                "99",
                "--temp",
                "0",
                "--no-webui",
                "--offline",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("Cannot start {}", assets.server.display()))?;
        let mut backend = Self {
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
                "This conversation reached Liquid's context capacity ({CONTEXT_TOKENS} tokens). Start a new conversation with `boubou run liquid --offline`. Backend log: {}",
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
        assert!(error.to_string().contains("Start a new conversation"));
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
