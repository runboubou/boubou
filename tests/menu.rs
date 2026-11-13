//! The menu on Linux, in a terminal of its own. `tests/packaging.rs` covers the Mac.
#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct Terminal {
    master: fs::File,
    output: String,
}

impl Terminal {
    /// Collects what the program has printed. False once it has closed the terminal.
    fn read(&mut self) -> bool {
        let mut buffer = [0; 8192];
        match self.master.read(&mut buffer) {
            Ok(n) => self.output.push_str(&String::from_utf8_lossy(&buffer[..n])),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => return false,
        }
        true
    }

    /// Waits for `text` among what was printed since the last keys.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.output.contains(text) {
            assert!(
                self.read() && Instant::now() < deadline,
                "Missing {text:?}: {}",
                self.output
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, keys: &[u8]) {
        self.output.clear();
        self.master.write_all(keys).unwrap();
    }
}

#[test]
fn a_start_without_arguments_opens_the_menu_and_runs_a_pasted_model() {
    let cache = std::env::temp_dir().join(format!("boubou-menu-{}", std::process::id()));
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    let (master, slave) = unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    // Without a display, the library row names its address instead of starting a browser.
    let mut child = Command::new(env!("CARGO_BIN_EXE_boubou"))
        .arg("--cache-dir")
        .arg(&cache)
        .env("TERM", "xterm-256color")
        .env("BOUBOU_NO_UPDATE_CHECK", "1")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    let mut terminal = Terminal {
        master,
        output: String::new(),
    };
    // A busy computer may fit no voice; the library is the last row either way.
    terminal.expect("this computer");
    terminal.expect("More voices at runboubou.com/models");
    terminal.expect("ctrl+c exit");
    assert!(
        !terminal.output.contains("Device check unavailable"),
        "{}",
        terminal.output
    );
    terminal.send(b"\x1b[A\r");
    terminal.expect("Paste a run command");
    terminal.send(b"npx boubou@beta run LFM3-Audio-2B --offline\r");
    terminal.expect("offline mode is enabled");
    assert!(terminal.output.contains("recheck this computer"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        terminal.read();
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "The menu's run did not exit");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(1));
    assert!(
        !cache.join("models").exists(),
        "An offline start must not download"
    );
    let _ = fs::remove_dir_all(cache);
}
