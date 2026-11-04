use crossterm::style::{Color, Stylize};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// One visible blocking phase. Byte totals are measured; unknown totals animate without a percentage.
pub struct Progress {
    bytes: Arc<AtomicU64>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Progress {
    pub fn new(label: &str, total: Option<u64>) -> Self {
        let interactive =
            io::stderr().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
        let color = interactive && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        if color {
            eprintln!("{}", label.with(Color::DarkCyan));
        } else {
            eprintln!("{label}");
        }
        let bytes = Arc::new(AtomicU64::new(0));
        if !interactive {
            return Self {
                bytes,
                stop: None,
                worker: None,
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
                        "\r\x1b[2K  {}  {}s",
                        bar(position.load(Ordering::Relaxed), total, tick),
                        began.elapsed().as_secs()
                    );
                    let _ = out.flush();
                    drop(out);
                    if stopped.recv_timeout(Duration::from_millis(120))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
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
        }
    }

    pub fn advance(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

fn bar(bytes: u64, total: Option<u64>, tick: usize) -> String {
    let mut cells = [b' '; 16];
    let suffix = if let Some(total) = total {
        let percent = if total == 0 {
            100
        } else {
            u128::from(bytes.min(total)) * 100 / u128::from(total)
        };
        cells[..percent as usize * 16 / 100].fill(b'=');
        format!("{percent:3}%")
    } else {
        cells[tick % 16] = b'>';
        "working".into()
    };
    format!("[{}] {suffix}", String::from_utf8_lossy(&cells))
}

impl Drop for Progress {
    fn drop(&mut self) {
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
    #[test]
    fn only_known_totals_show_a_bounded_percentage() {
        assert!(bar(25, Some(100), 0).ends_with(" 25%"));
        assert!(bar(200, Some(100), 0).ends_with("100%"));
        assert!(bar(0, Some(0), 0).ends_with("100%"));
        assert!(!bar(200, None, 0).contains('%'));
        assert_ne!(bar(0, None, 0), bar(0, None, 1));
    }
}
