//! Push-to-talk audio through the default ALSA devices, which PipeWire or PulseAudio serve
//! on Linux desktops and convert to these rates. Linux has no echo cancellation or voice
//! detection here, so a turn is recorded only after Enter has cleared playback.

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

/// Owns the microphone and speaker streams; dropping it closes both.
#[cfg(target_os = "linux")]
pub struct Audio {
    _streams: [cpal::Stream; 2],
    playback: Arc<Mutex<Playback>>,
    error: Arc<Mutex<Option<String>>>,
}

#[cfg(target_os = "linux")]
impl Audio {
    /// Linux records only manual turns, so `detect_voice` must be false.
    pub fn start(
        _cache_dir: &Path,
        detect_voice: bool,
        stopped: &AtomicBool,
    ) -> Result<(Self, Receiver<CaptureFrame>)> {
        debug_assert!(!detect_voice, "Linux has no voice detection");
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
        let (sender, receiver) = mpsc::sync_channel(64);
        let mut frames = Frames::default();
        let overflow = Arc::clone(&error);
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
                    frames.push(pcm, first, |frame| {
                        if let Err(TrySendError::Full(_)) =
                            sender.try_send(CaptureFrame::Samples(frame))
                        {
                            fail(
                                &overflow,
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
        let speech = Arc::clone(&playback);
        let output = host
            .default_output_device()
            .context("No speaker found. Connect one and check the sound settings.")?
            .build_output_stream(
                config(PLAYBACK_RATE),
                move |pcm: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    speech.lock().expect("audio playback lock").fill(pcm);
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

#[cfg(target_os = "linux")]
fn config(sample_rate: u32) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: 1,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    }
}

#[cfg(target_os = "linux")]
fn fail(error: &Mutex<Option<String>>, message: String) {
    error
        .lock()
        .expect("audio error lock")
        .get_or_insert(message);
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
        assert!(
            captured
                .iter()
                .all(|frame| frame.samples.len() == FRAME && !frame.near_end_speech)
        );
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
}
