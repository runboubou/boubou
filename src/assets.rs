use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use fs2::FileExt;
use reqwest::{StatusCode, blocking::Client, header};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const REVISION: &str = "02355c1a141e464a064f2ab2c276638dbcb7f38a";
const MODEL: Download<'_> = Download {
    name: "LFM3-Audio-2B-Q4_K_M.gguf",
    url: "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/LFM3-Audio-2B-Q4_K_M.gguf",
    size: 1_099_626_016,
    sha256: "9a773289bd321ed879cd3a76545f6aca506e403851a45fb315420aee45625be2",
};
const PROJECTOR: Download<'_> = Download {
    name: "mmproj-LFM3-Audio-2B-Q8_0.gguf",
    url: "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/mmproj-LFM3-Audio-2B-Q8_0.gguf",
    size: 303_811_840,
    sha256: "2b6648b1fd48af16aa0877b0592b144d15070783673e038a379fdbc51d40e365",
};
const LICENSE: Download<'_> = Download {
    name: "LICENSE",
    url: "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/LICENSE",
    size: 10_574,
    sha256: "4d28ca14dedc0b3d0fcc2b3339f0e79931faa33874f3d24f522183a8fc70068c",
};
const RUNTIME: Download<'_> = Download {
    name: "llama-b11960-bin-macos-arm64.tar.gz",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-macos-arm64.tar.gz",
    size: 16_465_074,
    sha256: "58e5a5e1d7c2bf66cedd9c29e0323f3e570660a70a1e2156d32eed08f4cc1e3c",
};
const RUNTIME_DIR: &str = "runtimes/b11960-macos-arm64";
const SERVER: &str = "llama-b11960/llama-server";

struct Download<'a> {
    name: &'a str,
    url: &'a str,
    size: u64,
    sha256: &'a str,
}

#[derive(Debug)]
pub struct Assets {
    pub server: PathBuf,
    pub model: PathBuf,
    pub projector: PathBuf,
}

pub fn prepare(cache_dir: &Path, offline: bool) -> Result<Assets> {
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "This first release supports Apple Silicon Macs."
    );
    fs::create_dir_all(cache_dir).context("Cannot create the Boubou cache")?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache_dir.join("install.lock"))?;
    FileExt::try_lock_exclusive(&lock)
        .context("Another Boubou process is preparing models; wait for it to finish")?;

    let model_dir = cache_dir.join("models/lfm3-audio-2b").join(REVISION);
    let archive = fetch(&RUNTIME, &cache_dir.join("downloads"), offline)?;
    let installed = cache_dir.join(RUNTIME_DIR);
    if !runtime_valid(&archive, &installed)? {
        eprintln!("Installing the local speech engine…");
        let staging = installed.with_extension("partial");
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        tar::Archive::new(GzDecoder::new(File::open(&archive)?))
            .unpack(&staging)
            .context("Cannot unpack the speech engine")?;
        ensure!(
            runtime_valid(&archive, &staging)?,
            "The unpacked speech engine failed verification"
        );
        if installed.exists() {
            fs::remove_dir_all(&installed)?;
        }
        fs::rename(&staging, &installed)?;
        sync_parent(&installed)?;
    }

    // Cache the model's license alongside its weights before making it runnable.
    fetch(&LICENSE, &model_dir, offline)?;
    let model = fetch(&MODEL, &model_dir, offline)?;
    let projector = fetch(&PROJECTOR, &model_dir, offline)?;
    Ok(Assets {
        server: installed.join(SERVER),
        model,
        projector,
    })
}

pub fn status(cache_dir: &Path) -> Result<()> {
    let model_dir = cache_dir.join("models/lfm3-audio-2b").join(REVISION);
    for spec in [&MODEL, &PROJECTOR, &LICENSE] {
        println!(
            "{}: {}",
            spec.name,
            if verified(&model_dir.join(spec.name), spec)? {
                "verified"
            } else {
                "missing or incomplete"
            }
        );
    }
    let archive = cache_dir.join("downloads").join(RUNTIME.name);
    let ready =
        verified(&archive, &RUNTIME)? && runtime_valid(&archive, &cache_dir.join(RUNTIME_DIR))?;
    println!(
        "Speech engine (b11960, macOS arm64): {}",
        if ready {
            "verified"
        } else {
            "missing or incomplete"
        }
    );
    Ok(())
}

fn sha256(mut reader: impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn verified(path: &Path, spec: &Download<'_>) -> Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
    };
    let metadata = file.metadata()?;
    Ok(metadata.is_file() && metadata.len() == spec.size && sha256(file)? == spec.sha256)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("Cache path has no parent")?)?.sync_all()?;
    Ok(())
}

fn fetch(spec: &Download<'_>, dir: &Path, offline: bool) -> Result<PathBuf> {
    let destination = dir.join(spec.name);
    if verified(&destination, spec)? {
        return Ok(destination);
    }
    let partial = dir.join(format!("{}.part", spec.name));
    if verified(&partial, spec)? {
        fs::rename(&partial, &destination)?;
        sync_parent(&destination)?;
        return Ok(destination);
    }
    ensure!(
        !offline,
        "{} is missing or corrupted. Run `boubou pull liquid` while online first.",
        spec.name
    );

    fs::create_dir_all(dir)?;
    let offset = fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    // A full-sized partial with an incorrect checksum cannot be resumed.
    let offset = if offset < spec.size { offset } else { 0 };
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(30 * 60))
        .user_agent(concat!("boubou/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut request = client
        .get(spec.url)
        .header(header::ACCEPT_ENCODING, "identity");
    if offset > 0 {
        request = request.header(header::RANGE, format!("bytes={offset}-"));
    }
    eprintln!(
        "{} {} ({:.1} MB)…",
        if offset == 0 {
            "Downloading"
        } else {
            "Resuming"
        },
        spec.name,
        spec.size as f64 / 1_000_000.0
    );
    let mut response = request
        .send()
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("Cannot download {}", spec.name))?;
    let start = match response.status() {
        StatusCode::OK => 0,
        StatusCode::PARTIAL_CONTENT => {
            let expected = format!("bytes {offset}-{}/{}", spec.size - 1, spec.size);
            ensure!(
                response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|h| h.to_str().ok())
                    == Some(expected.as_str()),
                "{} returned an inconsistent Content-Range; partial download preserved",
                spec.name
            );
            offset
        }
        status => bail!("Download of {} failed: HTTP {status}", spec.name),
    };
    if let Some(length) = response.content_length() {
        ensure!(
            length == spec.size - start,
            "{} returned an unexpected download size; partial download preserved",
            spec.name
        );
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(start > 0)
        .truncate(start == 0)
        .open(&partial)?;
    let mut received = start;
    let mut last_progress = Instant::now();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let n = response
            .read(&mut buffer)
            .with_context(|| format!("Download interrupted; run again to resume {}", spec.name))?;
        if n == 0 {
            break;
        }
        ensure!(
            received + n as u64 <= spec.size,
            "{} exceeded its expected size; download was not installed",
            spec.name
        );
        file.write_all(&buffer[..n])?;
        received += n as u64;
        if last_progress.elapsed() >= Duration::from_secs(2) {
            eprintln!("  {:.0}%", received as f64 * 100.0 / spec.size as f64);
            last_progress = Instant::now();
        }
    }
    file.sync_all()?;
    drop(file);
    ensure!(
        verified(&partial, spec)?,
        "{} failed its size or SHA-256 check; it was not installed. Run again to retry.",
        spec.name
    );
    fs::rename(&partial, &destination)?;
    sync_parent(&destination)?;
    Ok(destination)
}

fn runtime_valid(archive: &Path, installed: &Path) -> Result<bool> {
    if !installed.is_dir() {
        return Ok(false);
    }
    let mut tar = tar::Archive::new(GzDecoder::new(File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = installed.join(entry.path()?);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let kind = entry.header().entry_type();
        if kind.is_file() {
            if !metadata.is_file()
                || metadata.len() != entry.size()
                || sha256(File::open(&path)?)? != sha256(&mut entry)?
            {
                return Ok(false);
            }
        } else if kind.is_dir() {
            if !metadata.is_dir() {
                return Ok(false);
            }
        } else if kind.is_symlink() {
            if !metadata.file_type().is_symlink()
                || Some(fs::read_link(&path)?) != entry.link_name()?.map(|p| p.into_owned())
            {
                return Ok(false);
            }
        } else {
            bail!("Unsupported entry in the pinned speech engine archive");
        }
    }
    let server = installed.join(SERVER);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&server)?.permissions().mode() & 0o111 == 0 {
            return Ok(false);
        }
    }
    Ok(server.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufRead,
        net::TcpListener,
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "boubou-assets-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn serve(response: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/weights", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(&mut socket);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).unwrap();
            request
        });
        (url, worker)
    }

    fn spec<'a>(url: &'a str, hash: &'a str) -> Download<'a> {
        Download {
            name: "weights",
            url,
            size: 6,
            sha256: hash,
        }
    }

    #[test]
    fn resumes_partial_and_publishes_only_verified_bytes() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abc").unwrap();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let (url, worker) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
        );
        let path = fetch(&spec(&url, &hash), &dir.0, false).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"abcdef");
        assert!(!dir.0.join("weights.part").exists());
        assert!(
            worker
                .join()
                .unwrap()
                .to_lowercase()
                .contains("range: bytes=3-")
        );
    }

    #[test]
    fn restarts_when_server_ignores_range() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abc").unwrap();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let (url, worker) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef");
        let path = fetch(&spec(&url, &hash), &dir.0, false).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"abcdef");
        worker.join().unwrap();
    }

    #[test]
    fn refuses_inconsistent_range_without_destroying_partial() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abc").unwrap();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let (url, worker) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/6\r\nConnection: close\r\n\r\nabc",
        );
        assert!(fetch(&spec(&url, &hash), &dir.0, false).is_err());
        assert_eq!(fs::read(dir.0.join("weights.part")).unwrap(), b"abc");
        assert!(!dir.0.join("weights").exists());
        worker.join().unwrap();
    }

    #[test]
    fn offline_checks_hash_and_never_connects() {
        let dir = TestDir::new();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let spec = spec("http://127.0.0.1:1/must-not-connect", &hash);
        assert!(fetch(&spec, &dir.0, true).is_err());
        fs::write(dir.0.join("weights"), b"badbad").unwrap();
        assert!(fetch(&spec, &dir.0, true).is_err());
        fs::write(dir.0.join("weights"), b"abcdef").unwrap();
        assert!(fetch(&spec, &dir.0, true).is_ok());
    }

    #[test]
    fn recovers_complete_partial_without_network() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abcdef").unwrap();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let path = fetch(&spec("http://127.0.0.1:1", &hash), &dir.0, true).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"abcdef");
    }

    #[test]
    fn does_not_install_corrupted_download() {
        let dir = TestDir::new();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let (url, worker) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdeg");
        assert!(fetch(&spec(&url, &hash), &dir.0, false).is_err());
        assert!(!dir.0.join("weights").exists());
        worker.join().unwrap();
    }

    #[test]
    fn interrupted_body_preserves_bytes_for_next_attempt() {
        let dir = TestDir::new();
        let hash = sha256(&b"abcdef"[..]).unwrap();
        let (url, worker) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc");
        assert!(fetch(&spec(&url, &hash), &dir.0, false).is_err());
        assert_eq!(fs::read(dir.0.join("weights.part")).unwrap(), b"abc");
        assert!(!dir.0.join("weights").exists());
        worker.join().unwrap();
    }

    #[test]
    fn installed_engine_checks_actual_contents() {
        let dir = TestDir::new();
        let archive = dir.0.join("engine.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(6);
        header.set_cksum();
        tar.append_data(&mut header, SERVER, &b"engine"[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let installed = dir.0.join("installed");
        assert!(!runtime_valid(&archive, &installed).unwrap());
        tar::Archive::new(GzDecoder::new(File::open(&archive).unwrap()))
            .unpack(&installed)
            .unwrap();
        assert!(runtime_valid(&archive, &installed).unwrap());
        fs::write(installed.join(SERVER), b"broken").unwrap();
        assert!(!runtime_valid(&archive, &installed).unwrap());
    }
}
