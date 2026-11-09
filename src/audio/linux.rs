//! Linux audio through the default ALSA devices, which PipeWire or PulseAudio serve on
//! desktops and convert to these rates. Like the macOS helper, it removes the reply's echo
//! from the microphone with WebRTC AEC3 and marks frames that contain the person's own
//! speech, so a conversation can keep listening while it speaks.

use super::CapturedAudio;
use anyhow::{Result, bail};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use {
    super::CaptureFrame,
    anyhow::Context,
    cpal::traits::{DeviceTrait, HostTrait, StreamTrait},
    std::{
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::AtomicBool,
            mpsc::{self, Receiver, TrySendError},
        },
    },
};

const CAPTURE_RATE: u32 = 16_000;
const PLAYBACK_RATE: u32 = 24_000;
/// 20 ms at 16 kHz, the frame the conversation loop consumes.
const FRAME: usize = 320;
/// The echo canceller works on 10 ms chunks.
const RENDER_CHUNK: usize = PLAYBACK_RATE as usize / 100;
#[cfg(target_os = "linux")]
const CAPTURE_CHUNK: usize = CAPTURE_RATE as usize / 100;

/// Owns the microphone and speaker streams; dropping it closes both.
#[cfg(target_os = "linux")]
pub struct Audio {
    _streams: [cpal::Stream; 2],
    playback: Arc<Mutex<Playback>>,
    error: Arc<Mutex<Option<String>>>,
}

#[cfg(target_os = "linux")]
impl Audio {
    /// Echo cancellation and speech detection always run; manual turns ignore the detector.
    pub fn start(
        _cache_dir: &Path,
        _detect_voice: bool,
        stopped: &AtomicBool,
    ) -> Result<(Self, Receiver<CaptureFrame>)> {
        crate::check_cancel(stopped)?;
        let host = cpal::default_host();
        let error = Arc::new(Mutex::new(None));
        let report = |error: &Arc<Mutex<Option<String>>>| {
            let error = Arc::clone(error);
            move |cause: cpal::Error| {
                // cpal recovers from underruns and overruns itself; anything else stops a stream.
                if !matches!(cause.kind(), cpal::ErrorKind::Xrun) {
                    fail(&error, format!("The audio device stopped: {cause}"));
                }
            }
        };
        // Both callbacks share the canceller: the speaker's output is its reference signal.
        let echo = Arc::new(Mutex::new(Echo::new()));
        let (sender, receiver) = mpsc::sync_channel(64);
        let mut frames = Frames::default();
        let mut speech = Speech::default();
        let (cancelling, failure) = (Arc::clone(&echo), Arc::clone(&error));
        let input = host
            .default_input_device()
            .context("No microphone found. Connect one and check the sound settings.")?
            .build_input_stream(
                config(CAPTURE_RATE),
                move |pcm: &[i16], info: &cpal::InputCallbackInfo| {
                    let time = info.timestamp();
                    let first = Instant::now()
                        .checked_sub(time.callback.duration_since(time.capture))
                        .unwrap_or_else(Instant::now);
                    frames.push(pcm, first, |mut frame| {
                        let mut echo = cancelling.lock().expect("echo canceller lock");
                        if let Err(cause) = echo.cancel(&mut frame.samples) {
                            return fail(&failure, format!("Echo cancellation failed: {cause}"));
                        }
                        frame.near_end_speech = echo.gate.near_end(speech.push(&frame.samples));
                        drop(echo);
                        if let Err(TrySendError::Full(_)) =
                            sender.try_send(CaptureFrame::Samples(frame))
                        {
                            fail(
                                &failure,
                                "Microphone processing fell behind by more than one second; restart Boubou.".into(),
                            );
                        }
                    });
                },
                report(&error),
                None,
            )
            .context("Cannot record 16 kHz mono audio from the default microphone")?;
        let playback = Arc::new(Mutex::new(Playback::default()));
        let (queue, failure) = (Arc::clone(&playback), Arc::clone(&error));
        let output = host
            .default_output_device()
            .context("No speaker found. Connect one and check the sound settings.")?
            .build_output_stream(
                config(PLAYBACK_RATE),
                move |pcm: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    queue.lock().expect("audio playback lock").fill(pcm);
                    if let Err(cause) = echo.lock().expect("echo canceller lock").render(pcm) {
                        fail(&failure, format!("Echo cancellation failed: {cause}"));
                    }
                },
                report(&error),
                None,
            )
            .context("Cannot play 24 kHz mono audio on the default speaker")?;
        input.play().context("Cannot start the microphone")?;
        output.play().context("Cannot start the speaker")?;
        let audio = Self {
            _streams: [input, output],
            playback,
            error,
        };
        Ok((audio, receiver))
    }

    /// Schedules PCM16 mono audio at 24 kHz.
    pub fn enqueue(&mut self, pcm: &[i16]) -> Result<()> {
        self.check_error()?;
        self.playback.lock().expect("audio playback lock").push(pcm)
    }

    /// Discards scheduled speech; the device plays out at most its current buffer.
    pub fn clear(&mut self) -> Result<()> {
        self.playback.lock().expect("audio playback lock").0.clear();
        Ok(())
    }

    pub fn pending_samples(&self) -> usize {
        self.playback.lock().expect("audio playback lock").0.len()
    }

    pub fn check_error(&self) -> Result<()> {
        if let Some(message) = self.error.lock().expect("audio error lock").as_ref() {
            bail!("{message}");
        }
        Ok(())
    }
}

/// Asks for 20 ms periods so the canceller sees speaker and microphone audio interleaved.
/// PipeWire's default periods deliver both in 100 ms bursts, more jitter than AEC3 tolerates.
#[cfg(target_os = "linux")]
fn config(sample_rate: u32) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: 1,
        sample_rate,
        buffer_size: cpal::BufferSize::Fixed(sample_rate / 50),
    }
}

#[cfg(target_os = "linux")]
fn fail(error: &Mutex<Option<String>>, message: String) {
    error
        .lock()
        .expect("audio error lock")
        .get_or_insert(message);
}

/// WebRTC AEC3, noise suppression and a high-pass filter, fed with what the speaker plays.
#[cfg(target_os = "linux")]
struct Echo {
    canceller: sonora::AudioProcessing,
    rendered: Vec<i16>,
    gate: Gate,
}

#[cfg(target_os = "linux")]
impl Echo {
    fn new() -> Self {
        let config = sonora::Config {
            echo_canceller: Some(Default::default()),
            noise_suppression: Some(Default::default()),
            high_pass_filter: Some(Default::default()),
            ..Default::default()
        };
        Self {
            canceller: sonora::AudioProcessing::builder()
                .config(config)
                .capture_config(sonora::StreamConfig::new(CAPTURE_RATE, 1))
                .render_config(sonora::StreamConfig::new(PLAYBACK_RATE, 1))
                .build(),
            rendered: Vec::with_capacity(RENDER_CHUNK),
            gate: Gate::default(),
        }
    }

    /// Gives the canceller the speaker's output in 10 ms chunks.
    fn render(&mut self, pcm: &[i16]) -> Result<()> {
        let mut processed = [0; RENDER_CHUNK];
        for &sample in pcm {
            self.rendered.push(sample);
            if self.rendered.len() == RENDER_CHUNK {
                self.gate.render(&self.rendered);
                self.canceller
                    .process_render_i16(&self.rendered, &mut processed)?;
                self.rendered.clear();
            }
        }
        Ok(())
    }

    /// Removes the speaker's echo from a 20 ms microphone frame in place.
    fn cancel(&mut self, frame: &mut [i16]) -> Result<()> {
        for chunk in frame.as_chunks_mut::<CAPTURE_CHUNK>().0 {
            let input = *chunk;
            self.canceller.process_capture_i16(&input, chunk)?;
        }
        Ok(())
    }
}

/// Neural speech detection on the echo-cancelled microphone, in 16 ms windows.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Speech {
    detector: earshot::Detector,
    window: Vec<i16>,
}

#[cfg(target_os = "linux")]
impl Speech {
    /// The highest speech probability among windows completed by these samples.
    fn push(&mut self, pcm: &[i16]) -> f32 {
        let mut score = 0.0_f32;
        for &sample in pcm {
            self.window.push(sample);
            if self.window.len() == 256 {
                score = score.max(self.detector.predict_i16(&self.window));
                self.window.clear();
            }
        }
        score
    }
}

/// AEC3 needs about a second of reply to find the echo path. Until two seconds of audible
/// reply have played, speech counts only after half a second without playback. Even then,
/// echo can leak through briefly as a reply starts or resumes, so speech over it must be clear.
struct Gate {
    audible: u32,
    quiet: u32,
}

impl Gate {
    /// In 10 ms playback chunks.
    const CONVERGED: u32 = 200;
    const SETTLED: u32 = 50;
    /// Speech probabilities; echo leaks in a real room scored up to 0.75, people over 0.8.
    const SPEECH: f32 = 0.5;
    const OVER_REPLY: f32 = 0.8;

    fn render(&mut self, chunk: &[i16]) {
        if chunk.iter().any(|&sample| sample != 0) {
            self.audible = self.audible.saturating_add(1);
            self.quiet = 0;
        } else {
            self.quiet = self.quiet.saturating_add(1);
        }
    }

    fn near_end(&self, score: f32) -> bool {
        if self.quiet >= Self::SETTLED {
            score >= Self::SPEECH
        } else {
            self.audible >= Self::CONVERGED && score >= Self::OVER_REPLY
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            audible: 0,
            quiet: Self::SETTLED,
        }
    }
}

/// Splits microphone callbacks into 20 ms frames, each timed by its first sample.
#[derive(Default)]
struct Frames {
    samples: Vec<i16>,
    started: Option<Instant>,
}

impl Frames {
    fn push(&mut self, pcm: &[i16], first: Instant, mut emit: impl FnMut(CapturedAudio)) {
        for (index, &sample) in pcm.iter().enumerate() {
            let started = *self.started.get_or_insert_with(|| {
                first + Duration::from_nanos(index as u64 * 1_000_000_000 / u64::from(CAPTURE_RATE))
            });
            self.samples.push(sample);
            if self.samples.len() == FRAME {
                self.started = None;
                emit(CapturedAudio {
                    samples: std::mem::take(&mut self.samples),
                    captured_at: started,
                    near_end_speech: false,
                });
            }
        }
    }
}

/// Speech waiting for the speaker, bounded like the macOS playback queue.
#[derive(Default)]
struct Playback(VecDeque<i16>);

impl Playback {
    fn push(&mut self, pcm: &[i16]) -> Result<()> {
        if self.0.len() + pcm.len() > PLAYBACK_RATE as usize * 30 {
            bail!(
                "The response exceeded the 30-second playback queue. Interrupt it and try a shorter question."
            );
        }
        self.0.extend(pcm);
        Ok(())
    }

    /// Fills a device buffer, padding with silence once speech runs out.
    fn fill(&mut self, pcm: &mut [i16]) {
        for sample in pcm {
            *sample = self.0.pop_front().unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_become_20_ms_frames_timed_by_their_first_sample() {
        let mut frames = Frames::default();
        let mut captured = Vec::new();
        let start = Instant::now();
        frames.push(&[1; 100], start, |frame| captured.push(frame));
        assert!(captured.is_empty());
        let later = start + Duration::from_millis(30);
        frames.push(&[2; 600], later, |frame| captured.push(frame));
        frames.push(&[3; 260], later + Duration::from_millis(40), |frame| {
            captured.push(frame)
        });
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].samples, [vec![1; 100], vec![2; 220]].concat());
        assert_eq!(captured[0].captured_at, start);
        // Later frames start 220 and 540 samples into the second callback.
        assert_eq!(
            captured[1].captured_at,
            later + Duration::from_micros(13_750)
        );
        assert_eq!(
            captured[2].captured_at,
            later + Duration::from_micros(33_750)
        );
        assert_eq!(captured[2].samples, [vec![2; 60], vec![3; 260]].concat());
        assert!(captured.iter().all(|frame| frame.samples.len() == FRAME));
    }

    #[test]
    fn playback_drains_in_order_pads_silence_and_stays_bounded() {
        let mut playback = Playback::default();
        playback.push(&[1, 2, 3]).unwrap();
        let mut device = [9; 2];
        playback.fill(&mut device);
        assert_eq!((device, playback.0.len()), ([1, 2], 1));
        let mut device = [9; 3];
        playback.fill(&mut device);
        assert_eq!(device, [3, 0, 0]);
        playback.push(&vec![1; 24_000 * 30]).unwrap();
        assert!(
            playback
                .push(&[1])
                .unwrap_err()
                .to_string()
                .contains("30-second playback queue")
        );
    }

    #[test]
    fn speech_interrupts_playback_only_once_cancelled_and_clear() {
        let mut gate = Gate::default();
        assert!(gate.near_end(0.6), "nothing has played yet");
        assert!(!gate.near_end(0.4));
        for _ in 0..10 {
            gate.render(&[1000; RENDER_CHUNK]);
        }
        assert!(
            !gate.near_end(0.95),
            "the first reply's echo is not cancelled yet"
        );
        for _ in 0..Gate::SETTLED {
            gate.render(&[0; RENDER_CHUNK]);
        }
        assert!(
            gate.near_end(0.6),
            "a turn may start once playback has stopped"
        );
        for _ in 0..Gate::CONVERGED {
            gate.render(&[1000; RENDER_CHUNK]);
        }
        assert!(
            !gate.near_end(0.75),
            "a leak of echo does not interrupt playback"
        );
        assert!(
            gate.near_end(0.9),
            "after convergence, clear speech interrupts playback"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_canceller_removes_a_delayed_copy_of_the_speaker_signal() {
        // Twenty unrelated tones across the speech band, evaluated at both device rates.
        let mut seed = 7_u64;
        let mut random = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 11) as f64 / (1_u64 << 53) as f64
        };
        let tones: Vec<(f64, f64)> = (0..20)
            .map(|_| (200.0 + random() * 3200.0, random() * std::f64::consts::TAU))
            .collect();
        let signal = |seconds: f64, gain: f64| {
            let value: f64 = tones
                .iter()
                .map(|(hz, phase)| (seconds * hz * std::f64::consts::TAU + phase).sin())
                .sum();
            (value * gain * 300.0) as i16
        };
        let energy = |pcm: &[i16]| pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        let mut echo = Echo::new();
        let (mut before, mut after) = (0.0, 0.0);
        // Five seconds in 20 ms device periods; the microphone hears half the speaker 40 ms later.
        for step in 0..250 {
            let speaker: Vec<i16> = (0..2 * RENDER_CHUNK)
                .map(|n| signal((step * 2 * RENDER_CHUNK + n) as f64 / 24_000.0, 1.0))
                .collect();
            echo.render(&speaker).unwrap();
            let mut microphone: Vec<i16> = (0..FRAME)
                .map(|n| signal((step * FRAME + n) as f64 / 16_000.0 - 0.04, 0.5))
                .collect();
            let heard = energy(&microphone);
            echo.cancel(&mut microphone).unwrap();
            if step >= 200 {
                before += heard;
                after += energy(&microphone);
            }
        }
        assert!(echo.gate.audible >= Gate::CONVERGED);
        assert!(
            after * 100.0 < before,
            "only {:.1} dB of echo removed",
            10.0 * (before / after).log10()
        );
    }
}
