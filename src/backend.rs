use crate::assets::Assets;
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// The backend lives only as long as this CLI invocation.
pub struct Backend {
    child: Child,
    pub address: String,
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
                "8192",
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
        let mut backend = Self { child, address };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = backend.child.try_wait()? {
                bail!(
                    "Liquid backend exited ({status}). See {}",
                    log_path.display()
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
                    log_path.display()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
