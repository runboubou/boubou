use anyhow::{Context, Result, bail, ensure};
use std::collections::VecDeque;

pub enum Segment {
    Start(Vec<i16>),
    Continue(Vec<i16>),
    End(Vec<i16>),
}

#[derive(Default)]
pub struct PendingAudio {
    turns: VecDeque<(u64, Vec<i16>, bool)>,
    samples: usize,
    next_id: u64,
}

impl PendingAudio {
    pub fn clear(&mut self) {
        self.turns.clear();
        self.samples = 0;
    }

    pub fn push(&mut self, segment: Segment) -> Result<u64> {
        let (pcm, end) = match segment {
            Segment::Start(pcm) => {
                self.next_id += 1;
                self.turns.push_back((self.next_id, Vec::new(), false));
                (pcm, false)
            }
            Segment::Continue(pcm) => (pcm, false),
            Segment::End(pcm) => (pcm, true),
        };
        ensure!(
            self.samples + pcm.len() <= 16_000 * 30,
            "Too much audio accumulated while Liquid was stopping"
        );
        let (id, buffer, complete) = self
            .turns
            .back_mut()
            .context("Audio arrived without a user turn")?;
        self.samples += pcm.len();
        buffer.extend(pcm);
        *complete = end;
        Ok(*id)
    }

    pub fn take(&mut self, allow_finish: bool) -> Option<(u64, Vec<i16>, bool)> {
        let (id, buffer, complete) = self.turns.front_mut()?;
        if (*complete && !allow_finish) || (buffer.is_empty() && !*complete) {
            return None;
        }
        let pcm = std::mem::take(buffer);
        let end = *complete;
        let id = *id;
        self.samples -= pcm.len();
        if end {
            self.turns.pop_front();
        }
        Some((id, pcm, end))
    }
}

/// Endpoint 20 ms frames using macOS's echo-aware near-end speech indication.
pub struct Voice {
    preroll: VecDeque<Vec<i16>>,
    active: bool,
    speech: usize,
    silence: usize,
    frames: usize,
}

impl Voice {
    pub fn new() -> Self {
        Self {
            preroll: VecDeque::new(),
            active: false,
            speech: 0,
            silence: 0,
            frames: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn push(&mut self, pcm: Vec<i16>, near_end_speech: bool) -> Result<Option<Segment>> {
        ensure!(
            pcm.len() == 320,
            "Audio capture did not produce a 20 ms frame"
        );
        let voiced = near_end_speech;
        if !self.active {
            self.preroll.push_back(pcm);
            if self.preroll.len() > 10 {
                self.preroll.pop_front();
            }
            self.speech = if voiced { self.speech + 1 } else { 0 };
            if self.speech < 5 {
                return Ok(None);
            }
            self.active = true;
            self.silence = 0;
            self.frames = self.preroll.len();
            return Ok(Some(Segment::Start(
                self.preroll.drain(..).flatten().collect(),
            )));
        }
        self.frames += 1;
        if self.frames >= 1500 {
            bail!("A voice turn reached 30 seconds. Please pause between shorter turns.");
        }
        self.silence = if voiced { 0 } else { self.silence + 1 };
        if self.silence >= 30 {
            self.active = false;
            self.speech = 0;
            return Ok(Some(Segment::End(pcm)));
        }
        Ok(Some(Segment::Continue(pcm)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_never_opens_a_turn_and_preroll_stays_bounded() {
        let mut voice = Voice::new();
        for _ in 0..10_000 {
            assert!(voice.push(vec![0; 320], false).unwrap().is_none());
        }
        assert_eq!(voice.preroll.len(), 10);
    }

    #[test]
    fn wrong_frame_size_is_an_explicit_error() {
        assert!(Voice::new().push(vec![0; 441], false).is_err());
    }

    #[test]
    fn far_end_speech_does_not_start_a_turn_but_near_end_speech_does() {
        let mut voice = Voice::new();
        for _ in 0..200 {
            assert!(voice.push(vec![9000; 320], false).unwrap().is_none());
        }
        for _ in 0..4 {
            assert!(voice.push(vec![1000; 320], true).unwrap().is_none());
        }
        assert!(matches!(
            voice.push(vec![1000; 320], true).unwrap(),
            Some(Segment::Start(_))
        ));
        for _ in 0..29 {
            assert!(matches!(
                voice.push(vec![0; 320], false).unwrap(),
                Some(Segment::Continue(_))
            ));
        }
        assert!(matches!(
            voice.push(vec![0; 320], false).unwrap(),
            Some(Segment::End(_))
        ));
    }

    #[test]
    fn pauses_during_cancellation_keep_separate_turn_boundaries() {
        let mut pending = PendingAudio::default();
        pending.push(Segment::Start(vec![1; 320])).unwrap();
        pending.push(Segment::End(vec![2; 320])).unwrap();
        pending.push(Segment::Start(vec![3; 320])).unwrap();
        assert_eq!(
            pending.take(true),
            Some((1, [vec![1; 320], vec![2; 320]].concat(), true))
        );
        assert_eq!(pending.take(false), Some((2, vec![3; 320], false)));
        pending.push(Segment::End(vec![4; 320])).unwrap();
        assert_eq!(pending.take(true), Some((2, vec![4; 320], true)));
        assert_eq!(pending.take(true), None);
    }

    #[test]
    fn delayed_cancellation_waits_for_current_speech_and_preserves_both_turns() {
        let mut pending = PendingAudio::default();
        // A completes while cancellation is still pending; B starts before its acknowledgement.
        pending.push(Segment::Start(vec![1; 320])).unwrap();
        pending.push(Segment::End(vec![2; 320])).unwrap();
        let mut voice = Voice::new();
        for _ in 0..5 {
            if let Some(segment) = voice.push(vec![3; 320], true).unwrap() {
                pending.push(segment).unwrap();
            }
        }
        assert!(voice.is_active());
        // Once cancellation is acknowledged, A must still wait while B is being captured.
        assert_eq!(pending.take(!voice.is_active()), None);
        for _ in 0..30 {
            pending
                .push(voice.push(vec![4; 320], false).unwrap().unwrap())
                .unwrap();
        }
        assert!(!voice.is_active());
        assert_eq!(
            pending.take(!voice.is_active()),
            Some((1, [vec![1; 320], vec![2; 320]].concat(), true))
        );
        assert_eq!(
            pending.take(!voice.is_active()),
            Some((2, [vec![3; 5 * 320], vec![4; 30 * 320]].concat(), true))
        );
        assert_eq!(pending.take(true), None);
    }

    #[test]
    fn pause_discards_queued_input_without_reusing_its_identity() {
        let mut pending = PendingAudio::default();
        assert_eq!(pending.push(Segment::Start(vec![1; 320])).unwrap(), 1);
        pending.push(Segment::End(vec![2; 320])).unwrap();
        pending.clear();
        assert_eq!(pending.take(true), None);
        assert_eq!(pending.samples, 0);
        assert_eq!(pending.push(Segment::Start(vec![3; 320])).unwrap(), 2);
        pending.push(Segment::End(vec![4; 320])).unwrap();
        assert_eq!(
            pending.take(true),
            Some((2, [vec![3; 320], vec![4; 320]].concat(), true))
        );
    }
}
