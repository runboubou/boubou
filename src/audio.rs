#[cfg(target_os = "macos")]
use anyhow::{Context, bail, ensure};
use anyhow::{Result, anyhow};
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};
use std::io::Read;
#[cfg(target_os = "macos")]
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::AtomicBool,
        mpsc::{self, Receiver, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
pub struct CapturedAudio {
    pub samples: Vec<i16>,
    pub captured_at: Instant,
    pub near_end_speech: bool,
}

#[cfg(target_os = "macos")]
pub enum CaptureFrame {
    /// 20 ms of echo-cancelled mono PCM16 at 16 kHz.
    Samples(CapturedAudio),
    Error(String),
}

/// Owns the native audio process and its bounded capture and playback queues.
#[cfg(target_os = "macos")]
pub struct Audio {
    child: Child,
    input: ChildStdin,
    reader: Option<JoinHandle<()>>,
    playback: Arc<Mutex<Playback>>,
    error: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
struct Playback {
    epoch: u64,
    pending: usize,
}

impl Playback {
    fn acknowledge(&mut self, epoch: u64, count: usize) {
        if epoch == self.epoch {
            self.pending = self.pending.saturating_sub(count);
        }
    }

    fn clear(&mut self) -> u64 {
        self.epoch = self.epoch.wrapping_add(1);
        self.pending = 0;
        self.epoch
    }
}

#[cfg(target_os = "macos")]
impl Audio {
    #[cfg(target_os = "macos")]
    pub fn start(
        cache_dir: &Path,
        detect_voice: bool,
        stopped: &AtomicBool,
    ) -> Result<(Self, Receiver<CaptureFrame>)> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        crate::check_cancel(stopped)?;
        let clock = HostClock::new()?;
        let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/boubou-audio"));
        let hash = crate::assets::hex(&Sha256::digest(bytes));
        let directory = cache_dir.join("native").join(&hash[..16]);
        fs::create_dir_all(&directory).context("Could not create the native audio cache")?;
        let executable = directory.join("boubou-audio");
        if !executable.exists() {
            let temporary = directory.join(format!("audio-{}.part", std::process::id()));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&temporary)
                .context("Could not extract the native audio helper")?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &executable)?;
        }
        let actual = fs::read(&executable).context("Could not read the native audio helper")?;
        if Sha256::digest(&actual) != Sha256::digest(bytes) {
            bail!(
                "The cached native audio helper is damaged. Remove {} and retry.",
                directory.display()
            );
        }
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
        let mut command = Command::new(&executable);
        if !detect_voice {
            command.arg("--manual");
        }
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Could not start native audio")?;
        Self::from_child(child, clock, stopped)
    }

    #[cfg(target_os = "macos")]
    fn from_child(
        mut child: Child,
        clock: HostClock,
        stopped: &AtomicBool,
    ) -> Result<(Self, Receiver<CaptureFrame>)> {
        let mut output = child.stdout.take().expect("piped stdout");
        let input = child.stdin.take().expect("piped stdin");
        let playback = Arc::new(Mutex::new(Playback::default()));
        let error = Arc::new(Mutex::new(None));
        let (sender, receiver) = mpsc::sync_channel(64);
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let progress = Arc::clone(&playback);
        let failure = Arc::clone(&error);
        let mut audio = Self {
            child,
            input,
            reader: None,
            playback,
            error,
        };
        audio.reader = Some(thread::spawn(move || {
            let result = (|| -> Result<()> {
                let (kind, payload) =
                    read_packet(&mut output).context("Native audio exited before starting")?;
                if kind == 4 {
                    bail!("{}", String::from_utf8_lossy(&payload));
                }
                ensure!(
                    kind == 1 && payload.is_empty(),
                    "The native audio helper returned an invalid startup event"
                );
                ready_sender
                    .send(())
                    .map_err(|_| anyhow!("Audio startup cancelled"))?;
                loop {
                    let (kind, payload) = read_packet(&mut output)?;
                    match kind {
                        2 => {
                            match sender.try_send(CaptureFrame::Samples(clock.decode(&payload)?)) {
                                Ok(()) => {}
                                Err(TrySendError::Disconnected(_)) => return Ok(()),
                                Err(TrySendError::Full(_)) => bail!(
                                    "Microphone processing fell behind by more than one second; restart Boubou."
                                ),
                            }
                        }
                        3 => {
                            if payload.len() != 16 {
                                bail!(
                                    "The audio helper returned an invalid playback acknowledgment"
                                );
                            }
                            let acknowledged_epoch =
                                u64::from_le_bytes(payload[..8].try_into().expect("eight bytes"));
                            let count =
                                u64::from_le_bytes(payload[8..].try_into().expect("eight bytes"))
                                    as usize;
                            let mut state = progress.lock().expect("audio playback lock");
                            state.acknowledge(acknowledged_epoch, count);
                        }
                        4 => bail!("{}", String::from_utf8_lossy(&payload)),
                        5 => {
                            ensure!(payload.len() == 8, "Invalid playback clear acknowledgment");
                            let epoch =
                                u64::from_le_bytes(payload.try_into().expect("eight bytes"));
                            crate::trace("playback.clear.applied", format_args!("epoch={epoch}"));
                        }
                        _ => bail!("The audio helper returned an unknown event"),
                    }
                }
            })();
            if let Err(reason) = result {
                let message = format!("Native audio stopped: {reason:#}");
                *failure.lock().expect("audio error lock") = Some(message.clone());
                let _ = sender.try_send(CaptureFrame::Error(message));
            }
        }));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            crate::check_cancel(stopped)?;
            audio.check_error()?;
            ensure!(
                Instant::now() < deadline,
                "Microphone setup timed out. Check microphone permission and the selected audio devices."
            );
            match ready.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return Ok((audio, receiver)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    audio.check_error()?;
                    bail!("Native audio exited before starting");
                }
            }
        }
    }

    /// Schedules PCM16 mono audio at 24 kHz. Playback progress comes from the device.
    pub fn enqueue(&mut self, pcm: &[i16]) -> Result<()> {
        self.check_error()?;
        if pcm.is_empty() {
            return Ok(());
        }
        let mut state = self.playback.lock().expect("audio playback lock");
        if state.pending + pcm.len() > 24_000 * 30 {
            bail!(
                "The response exceeded the 30-second playback queue. Interrupt it and try a shorter question."
            );
        }
        state.pending += pcm.len();
        let epoch = state.epoch;
        drop(state);
        let mut payload = Vec::with_capacity(8 + pcm.len() * 2);
        payload.extend_from_slice(&epoch.to_le_bytes());
        for sample in pcm {
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        self.command(1, &payload)
            .context("Could not send speech to the audio device")
    }

    /// Discards scheduled audio and stops the currently playing native buffer.
    pub fn clear(&mut self) -> Result<()> {
        let mut state = self.playback.lock().expect("audio playback lock");
        let epoch = state.clear();
        drop(state);
        crate::trace("playback.clear.send", format_args!("epoch={epoch}"));
        self.command(2, &epoch.to_le_bytes())
            .context("Could not interrupt audio playback")?;
        crate::trace("playback.clear.sent", format_args!("epoch={epoch}"));
        Ok(())
    }

    pub fn pending_samples(&self) -> usize {
        self.playback.lock().expect("audio playback lock").pending
    }

    /// Must be checked by the session loop even when the capture channel is full.
    pub fn check_error(&self) -> Result<()> {
        if let Some(message) = self.error.lock().expect("audio error lock").as_ref() {
            bail!("{message}");
        }
        Ok(())
    }

    fn command(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        self.input.write_all(&[kind])?;
        self.input
            .write_all(&(payload.len() as u32).to_le_bytes())?;
        self.input.write_all(payload)?;
        self.input.flush()?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for Audio {
    fn drop(&mut self) {
        // Normal shutdown lets the native process remove its HAL listener.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = self.input.as_raw_fd();
            // An unhealthy helper may stop reading. Do not block cleanup on a full pipe.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags >= 0
                && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0
            {
                let _ = self.command(3, &[]);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct HostClock {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut HostClock) -> i32;
}

#[cfg(target_os = "macos")]
impl HostClock {
    fn new() -> Result<Self> {
        let mut clock = Self { numer: 0, denom: 0 };
        // The native helper uses AVAudioTime.hostTime, the same Mach host clock.
        if unsafe { mach_timebase_info(&mut clock) } != 0 || clock.denom == 0 {
            bail!("Cannot read the macOS audio clock");
        }
        Ok(clock)
    }

    fn decode(&self, payload: &[u8]) -> Result<CapturedAudio> {
        if payload.len() != 649 || payload[8] > 1 {
            bail!("The audio helper returned an invalid timestamped capture frame");
        }
        let ticks = u64::from_le_bytes(payload[..8].try_into().expect("eight bytes"));
        let age_ticks = unsafe { mach_absolute_time() }.saturating_sub(ticks);
        let nanos = (age_ticks as u128 * self.numer as u128 / self.denom as u128)
            .min(u64::MAX as u128) as u64;
        let captured_at = Instant::now()
            .checked_sub(Duration::from_nanos(nanos))
            .context("Invalid microphone timestamp")?;
        Ok(CapturedAudio {
            samples: payload[9..]
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(i16::from_le_bytes)
                .collect(),
            captured_at,
            near_end_speech: payload[8] != 0,
        })
    }
}

fn read_packet(reader: &mut impl Read) -> Result<(u8, Vec<u8>)> {
    let mut header = [0; 5];
    reader.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if length > 65_536 {
        return Err(anyhow!("Native audio event exceeds the protocol limit"));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn cancelling_before_audio_ready_reaps_the_owned_helper() {
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let began = Instant::now();
        let error = Audio::from_child(child, HostClock::new().unwrap(), &stopped)
            .err()
            .expect("Startup must be cancelled");
        worker.join().unwrap();
        assert!(error.is::<crate::Cancelled>());
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "Owned helper is still alive"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Uses the real output device with silence; does not open the microphone"]
    fn cancelled_playback_does_not_overflow_capture_events() {
        let executable = std::path::PathBuf::from(env!("OUT_DIR"))
            .join(format!("audio-cancel-test-{}", std::process::id()));
        let compiled = Command::new("clang")
            .args([
                "-O2",
                "-fobjc-arc",
                "-mmacosx-version-min=13.0",
                "-framework",
                "AVFoundation",
                "-framework",
                "Foundation",
                "-framework",
                "CoreAudio",
            ])
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("native/audio_tests.m"))
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let result = Command::new(&executable).output();
        let _ = fs::remove_file(executable);
        let result = result.unwrap();
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
        assert!(
            result.status.success(),
            "Native cancellation regression failed"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Plays a supplied model reply on real speakers and checks microphone VAD"]
    fn speaker_echo_probe() {
        use std::time::{Duration, Instant};
        let path = std::env::var_os("BOUBOU_SPEAKER_TEST_WAV")
            .expect("Set BOUBOU_SPEAKER_TEST_WAV to a PCM16 mono 24kHz model reply");
        let mut wav = hound::WavReader::open(path).unwrap();
        assert_eq!(wav.spec().channels, 1);
        assert_eq!(wav.spec().sample_rate, 24_000);
        let pcm: Vec<i16> = wav
            .samples()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let cache = dirs::cache_dir().unwrap().join("boubou");
        let (mut audio, captured) = Audio::start(&cache, true, &AtomicBool::new(false)).unwrap();
        let mut voice = crate::voice::Voice::new();
        let mut frames = 0;
        let mut starts = 0;
        let mut peak_rms = 0.0_f64;
        let mut flagged_frames = 0;
        let mut flagged_energy = 0.0_f64;
        let warmup = Instant::now() + Duration::from_secs(1);
        while Instant::now() < warmup {
            if let Ok(CaptureFrame::Samples(frame)) =
                captured.recv_timeout(Duration::from_millis(100))
            {
                let _ = voice.push(frame.samples, frame.near_end_speech).unwrap();
            }
        }
        audio.enqueue(&pcm).unwrap();
        let deadline = Instant::now() + Duration::from_secs_f64(pcm.len() as f64 / 24_000.0 + 2.0);
        while Instant::now() < deadline {
            audio.check_error().unwrap();
            match captured.recv_timeout(Duration::from_millis(100)) {
                Ok(CaptureFrame::Samples(frame)) => {
                    frames += 1;
                    assert!(frame.captured_at.elapsed() < Duration::from_millis(500));
                    let rms = (frame
                        .samples
                        .iter()
                        .map(|&s| (s as f64 / 32768.0).powi(2))
                        .sum::<f64>()
                        / frame.samples.len() as f64)
                        .sqrt();
                    peak_rms = peak_rms.max(rms);
                    if frame.near_end_speech {
                        flagged_frames += 1;
                        flagged_energy += rms * rms;
                    }
                    if matches!(
                        voice.push(frame.samples, frame.near_end_speech).unwrap(),
                        Some(crate::voice::Segment::Start(_))
                    ) {
                        starts += 1;
                    }
                }
                Ok(CaptureFrame::Error(e)) => panic!("{e}"),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("Microphone disconnected"),
            }
        }
        assert_eq!(
            audio.pending_samples(),
            0,
            "Test speech did not finish playing"
        );
        audio.clear().unwrap();
        eprintln!(
            "SPEAKER_ECHO frames={frames} false_turn_starts={starts} peak_capture_rms={peak_rms:.5} flagged_frames={flagged_frames} flagged_rms={:.5}",
            (flagged_energy / f64::from(flagged_frames.max(1))).sqrt()
        );
        assert!(frames > 100);
        assert_eq!(starts, 0, "Speaker playback triggered a false user turn");
    }

    #[test]
    fn an_interrupted_buffer_cannot_retire_new_playback() {
        let mut playback = Playback {
            epoch: 0,
            pending: 2400,
        };
        let epoch = playback.clear();
        assert_eq!(playback.pending, 0);
        playback.pending = 480;
        playback.acknowledge(0, 2400);
        assert_eq!(playback.pending, 480);
        playback.acknowledge(epoch, 320);
        assert_eq!(playback.pending, 160);
        playback.acknowledge(epoch, 320);
        assert_eq!(playback.pending, 0);
    }

    #[test]
    fn rejects_truncated_or_oversized_native_events() {
        assert!(read_packet(&mut &[2, 4, 0, 0, 0, 1][..]).is_err());
        assert!(read_packet(&mut &[2, 0, 0, 2, 0][..]).is_err());
        let (kind, data) = read_packet(&mut &[4, 3, 0, 0, 0, b'b', b'a', b'd'][..]).unwrap();
        assert_eq!((kind, data), (4, b"bad".to_vec()));
    }
}
