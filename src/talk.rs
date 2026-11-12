//! The conversation screen. On a terminal: the transcript scrolls above a small You / Boubou
//! panel with live level meters. Otherwise: the plain status lines, unchanged.

use crate::{
    progress::HERE,
    tui::{self, BOLD, DIM, Paint, RESET},
};
use std::{
    collections::VecDeque,
    io::{self, Write},
    time::{Duration, Instant},
};

/// About 18 frames per second.
const FRAME: Duration = Duration::from_millis(55);
const HISTORY: usize = 60;
/// Playback level resolution: 20 ms at 24 kHz.
const CHUNK: usize = 480;

/// Conversation events with a fixed plain-text form.
#[derive(Clone, Copy, PartialEq)]
pub enum Note {
    /// The session is ready for speech.
    Intro,
    Paused,
    Resumed,
    Interrupted,
    /// Manual mode: recording until Enter.
    Recording,
    NoAudioYet,
    ContextCleared,
    Thinking,
}

#[derive(Clone, Copy, PartialEq)]
enum Mic {
    Listening,
    Paused,
    Idle,
    Recording,
}

pub struct Panel {
    live: Option<Live>,
    manual: bool,
    reply_open: bool,
}

struct Live {
    paint: Paint,
    /// Rows of the live region currently on screen; the cursor sits on its last row.
    drawn: usize,
    who: Option<String>,
    partial: String,
    you: VecDeque<f32>,
    boubou: VecDeque<f32>,
    mic_peak: f32,
    mic: Mic,
    thinking: bool,
    speaking: bool,
    playback: Playback,
    last_frame: Instant,
}

impl Panel {
    /// Live when both output streams are interactive terminals.
    pub fn new(manual: bool) -> Self {
        let live =
            (tui::interactive(&io::stdout()) && tui::interactive(&io::stderr())).then(|| Live {
                paint: Paint::detect(),
                drawn: 0,
                who: None,
                partial: String::new(),
                you: VecDeque::from(vec![0.0; HISTORY]),
                boubou: VecDeque::from(vec![0.0; HISTORY]),
                mic_peak: 0.0,
                mic: if manual { Mic::Idle } else { Mic::Listening },
                thinking: false,
                speaking: false,
                playback: Playback::default(),
                last_frame: Instant::now() - FRAME,
            });
        let mut panel = Self {
            live,
            manual,
            reply_open: false,
        };
        if let Some(live) = &mut panel.live {
            live.write("\x1b[?25l");
            live.redraw();
        }
        panel
    }

    pub fn note(&mut self, note: Note) {
        // These notes end the reply line, so the next reply is labelled again.
        if matches!(note, Note::Paused | Note::Interrupted | Note::Recording) {
            self.reply_open = false;
        }
        let Some(live) = &mut self.live else {
            let text = match note {
                Note::Intro if self.manual => {
                    "\r\nManual · Enter starts/submits recording or interrupts a reply · Esc pauses · Ctrl+C exits\r\n"
                }
                Note::Intro => {
                    "\r\nListening · pause to send · speak to interrupt · Esc pauses · Enter resumes · Ctrl+C exits\r\n"
                }
                Note::Paused => "\r\nPaused · Press Enter to resume · Ctrl+C exits\r\n",
                Note::Resumed if self.manual => return,
                Note::Resumed => "\r\nListening…\r\n",
                Note::Interrupted => "\r\n[Interrupted]\r\n",
                Note::Recording => "\r\nRecording… Press Enter to send.\r\n",
                Note::NoAudioYet => {
                    "\r\nNo audio received yet. Keep recording, then press Enter to send.\r\n"
                }
                Note::ContextCleared => "\r\n[Earlier conversation context cleared.]\r\n",
                Note::Thinking => "\r\nThinking…\r\n",
            };
            eprint!("{text}");
            return;
        };
        match note {
            Note::Intro => {}
            Note::Paused => {
                live.mic = Mic::Paused;
                live.thinking = false;
                live.end_line();
            }
            Note::Resumed => {
                live.mic = if self.manual {
                    Mic::Idle
                } else {
                    Mic::Listening
                }
            }
            Note::Interrupted => {
                live.thinking = false;
                live.end_line();
                live.commit(&format!("{DIM}[Interrupted]"));
            }
            Note::Recording => {
                live.mic = Mic::Recording;
                live.end_line();
            }
            Note::NoAudioYet => live.commit(&format!(
                "{DIM}No audio yet. Keep talking, then press ↵ to send."
            )),
            Note::ContextCleared => {
                live.end_line();
                live.commit(&format!("{DIM}[Earlier conversation context cleared]"));
            }
            Note::Thinking => {
                live.thinking = true;
                if self.manual {
                    live.mic = Mic::Idle;
                }
            }
        }
        live.redraw();
    }

    /// What the speech recogniser heard (Qwen pipelines).
    pub fn heard(&mut self, text: &str) {
        let Some(live) = &mut self.live else {
            if self.reply_open {
                print!("\r\n");
                self.reply_open = false;
            }
            print!("You: {text}\r\n");
            let _ = io::stdout().flush();
            return;
        };
        self.reply_open = false;
        live.end_line();
        live.thinking = true;
        live.who = Some("You".into());
        live.append(text);
        live.end_line();
        live.redraw();
    }

    /// Streamed reply text from `who`.
    pub fn reply(&mut self, who: &str, text: &str) {
        let open = std::mem::replace(&mut self.reply_open, true);
        let Some(live) = &mut self.live else {
            if !open {
                eprint!("{who}: ");
            }
            print!("{}", text.replace('\n', "\r\n"));
            let _ = io::stdout().flush();
            return;
        };
        live.thinking = false;
        if !open {
            live.end_line();
            live.who = Some(who.into());
        }
        live.append(text);
        live.redraw();
    }

    /// The reply finished. Plain output always ends the line, as the conversation loops did.
    pub fn reply_done(&mut self) {
        self.reply_open = false;
        match &mut self.live {
            Some(live) => {
                live.thinking = false;
                live.end_line();
                live.redraw();
            }
            None => {
                print!("\r\n");
                let _ = io::stdout().flush();
            }
        }
    }

    /// Closes an open reply line, if any (Qwen ends replies this way).
    pub fn reply_close(&mut self) {
        if self.reply_open {
            self.reply_done();
        }
    }

    /// One captured microphone frame.
    pub fn mic(&mut self, pcm: &[i16]) {
        if let Some(live) = &mut self.live {
            live.mic_peak = live.mic_peak.max(tui::level(pcm));
        }
    }

    /// Speech handed to the speaker.
    pub fn played(&mut self, pcm: &[i16]) {
        if let Some(live) = &mut self.live {
            live.playback.push(pcm);
            live.thinking = false;
        }
    }

    /// Queued speech was discarded.
    pub fn silenced(&mut self) {
        if let Some(live) = &mut self.live {
            live.playback = Playback::default();
        }
    }

    /// Advance the meters. `pending` is the speaker's unplayed sample count.
    pub fn tick(&mut self, pending: usize) {
        let Some(live) = &mut self.live else {
            return;
        };
        if live.last_frame.elapsed() < FRAME {
            return;
        }
        live.last_frame = Instant::now();
        if let Some(notice) = crate::update::take_notice() {
            for line in tui::wrap(&notice, Live::width().saturating_sub(4).max(1)) {
                live.commit(&line);
            }
        }
        let you = if matches!(live.mic, Mic::Paused | Mic::Idle) {
            0.0
        } else {
            std::mem::take(&mut live.mic_peak)
        };
        live.mic_peak = 0.0;
        let boubou = live.playback.level(pending);
        live.speaking = pending > 0;
        for (history, level) in [(&mut live.you, you), (&mut live.boubou, boubou)] {
            history.pop_front();
            history.push_back(level);
        }
        live.redraw();
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        if let Some(live) = &mut self.live {
            live.end_line();
            live.clear();
            live.write("\x1b[?25h");
        }
    }
}

impl Live {
    fn write(&self, text: &str) {
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(text.as_bytes());
        let _ = stderr.flush();
    }

    fn width() -> usize {
        crossterm::terminal::size().map_or(80, |(w, _)| w as usize)
    }

    /// Erase the live region, leaving the cursor where it began.
    fn clear(&mut self) {
        if self.drawn > 0 {
            let up = if self.drawn > 1 {
                format!("\x1b[{}A", self.drawn - 1)
            } else {
                String::new()
            };
            self.write(&format!("\r{up}\x1b[J"));
            self.drawn = 0;
        }
    }

    /// Print a finished transcript line above the panel.
    fn commit(&mut self, line: &str) {
        self.clear();
        self.write(&format!("  {line}{RESET}\r\n"));
    }

    /// Add reply text, committing every full or wrapped line.
    fn append(&mut self, text: &str) {
        let mut pieces = text.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            self.partial.push_str(piece);
            self.wrap();
            if pieces.peek().is_some() {
                self.end_line();
            }
        }
    }

    fn prefix(&self) -> (String, usize) {
        match &self.who {
            Some(who) => {
                let tone = if who == "You" {
                    String::new()
                } else {
                    self.paint.fg(tui::ACCENT)
                };
                (format!("{tone}{BOLD}{who}{RESET}  "), tui::width(who) + 2)
            }
            None => (String::new(), 0),
        }
    }

    fn wrap(&mut self) {
        let room = Self::width().saturating_sub(6).max(20);
        loop {
            let (_, lead) = self.prefix();
            if lead + tui::width(&self.partial) <= room {
                return;
            }
            // Break at the last space that fits, or hard-break a long word.
            let mut used = lead;
            let mut cut = 0;
            let mut space = None;
            for (i, c) in self.partial.char_indices() {
                used += tui::width(&c.to_string());
                if used > room {
                    break;
                }
                cut = i + c.len_utf8();
                if c == ' ' {
                    space = Some(i);
                }
            }
            let at = space.filter(|&i| i > 0).unwrap_or(cut);
            let rest = self.partial[at..].trim_start().to_owned();
            self.partial.truncate(at);
            self.end_line();
            // Continuation lines line up under the text, not the speaker.
            self.partial = rest;
            self.who = Some(String::new());
        }
    }

    fn end_line(&mut self) {
        if self.partial.trim().is_empty() {
            self.partial.clear();
            self.who = None;
            return;
        }
        let (prefix, _) = self.prefix();
        let line = format!("{prefix}{}", std::mem::take(&mut self.partial));
        self.commit(&line);
        self.who = None;
    }

    fn meter(&self, history: &VecDeque<f32>, cells: usize, tone: Option<tui::Tone>) -> String {
        let color = tone.map_or_else(String::new, |tone| self.paint.fg(tone));
        let mut out = String::new();
        let mut lit = None;
        for &level in history.iter().skip(HISTORY - cells) {
            let on = level > 0.02;
            if lit != Some(on) {
                out.push_str(RESET);
                out.push_str(if on { &color } else { DIM });
                lit = Some(on);
            }
            out.push(if on { tui::level_glyph(level) } else { '▁' });
        }
        out.push_str(RESET);
        out
    }

    fn redraw(&mut self) {
        let lines = self.lines(Self::width());
        let mut frame = String::new();
        if self.drawn > 0 {
            frame.push('\r');
            if self.drawn > 1 {
                frame.push_str(&format!("\x1b[{}A", self.drawn - 1));
            }
        }
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                frame.push_str("\r\n");
            }
            frame.push_str("\r\x1b[2K");
            frame.push_str(line);
            frame.push_str(RESET);
        }
        self.drawn = lines.len();
        self.write(&frame);
    }

    /// The live region: the reply being written, the panel and the key hints.
    fn lines(&self, width: usize) -> [String; 6] {
        let inner = width.saturating_sub(6);
        let here = HERE.replacen('O', "o", 1);
        let you_note = match self.mic {
            Mic::Listening => "microphone",
            Mic::Paused => "paused · ↵ resume",
            Mic::Idle => "↵ record",
            Mic::Recording => "recording · ↵ send",
        };
        let boubou_note = if self.thinking {
            "thinking…".to_owned()
        } else {
            here
        };
        let label = 8;
        // Narrow terminals drop the notes before squeezing the meters.
        let note_width = if inner >= 2 + label + 16 + 2 + 18 + 2 {
            18
        } else {
            0
        };
        let cells = inner
            .saturating_sub(2 + label + 2 + note_width + 2)
            .min(HISTORY);
        let filler = inner.saturating_sub(2 + label + cells + 2 + note_width + 2);
        let row = |name: &str, meter: String, note: &str| {
            format!(
                "  {DIM}│{RESET}  {BOLD}{}{RESET}{meter}{}  {DIM}{}{RESET}  {DIM}│",
                tui::pad(name, label),
                " ".repeat(filler),
                tui::pad_left(note, note_width)
            )
        };
        let rule = "─".repeat(inner);
        let hint = match (self.mic, self.manual_hint()) {
            (Mic::Paused, _) => "↵ resume · ctrl+c exit",
            (_, true) => "↵ record or send · ↵ interrupts a reply · esc pause · ctrl+c exit",
            _ => "pause to send · speak to interrupt · esc pause · ctrl+c exit",
        };
        let (prefix, lead) = self.prefix();
        let partial = tui::fit(&self.partial, width.saturating_sub(4 + lead));
        [
            if self.partial.is_empty() {
                String::new()
            } else {
                format!("  {prefix}{partial}")
            },
            format!("  {DIM}╭{rule}╮"),
            row("You", self.meter(&self.you, cells, None), you_note),
            row(
                "Boubou",
                self.meter(&self.boubou, cells, Some(tui::ACCENT)),
                &boubou_note,
            ),
            format!("  {DIM}╰{rule}╯"),
            format!("  {DIM}{}", tui::fit(hint, width.saturating_sub(4))),
        ]
    }

    fn manual_hint(&self) -> bool {
        matches!(self.mic, Mic::Idle | Mic::Recording)
    }
}

/// Levels of queued speech, so the meter follows what the speaker is playing now.
#[derive(Default)]
struct Playback {
    chunks: VecDeque<(f32, usize)>,
    queued: usize,
}

impl Playback {
    fn push(&mut self, pcm: &[i16]) {
        for chunk in pcm.chunks(CHUNK) {
            self.chunks.push_back((tui::level(chunk), chunk.len()));
            self.queued += chunk.len();
        }
    }

    /// The level now playing, given how many samples are still waiting.
    fn level(&mut self, pending: usize) -> f32 {
        if pending == 0 {
            *self = Self::default();
            return 0.0;
        }
        while let Some(&(_, len)) = self.chunks.front() {
            if self.queued - len < pending {
                break;
            }
            self.queued -= len;
            self.chunks.pop_front();
        }
        self.chunks.front().map_or(0.0, |&(level, _)| level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_replies_are_labelled_again_after_a_pause_or_recording() {
        for note in [Note::Paused, Note::Interrupted, Note::Recording] {
            let mut panel = Panel {
                live: None,
                manual: true,
                reply_open: false,
            };
            panel.reply("Liquid", "Hello");
            assert!(panel.reply_open);
            panel.note(note);
            assert!(!panel.reply_open);
        }
    }

    #[test]
    fn speaker_meter_follows_the_sample_now_playing() {
        let mut playback = Playback::default();
        let mut speech = vec![0_i16; CHUNK];
        speech.extend(vec![8000; CHUNK]);
        speech.extend(vec![0; CHUNK]);
        playback.push(&speech);
        // Nothing played yet: the first (silent) chunk is playing.
        assert_eq!(playback.level(3 * CHUNK), 0.0);
        // One chunk played: the loud one is next.
        assert!(playback.level(2 * CHUNK) > 0.5);
        assert!(playback.level(2 * CHUNK - 10) > 0.5);
        assert_eq!(playback.level(CHUNK), 0.0);
        assert_eq!(playback.level(0), 0.0);
        assert!(playback.chunks.is_empty(), "Drained once playback ends");
    }

    fn live(paint: Paint) -> Live {
        Live {
            paint,
            drawn: 0,
            who: Some("Liquid".into()),
            partial: "Paris is the capital of France.".into(),
            you: VecDeque::from(vec![0.0; HISTORY]),
            boubou: (0..HISTORY).map(|i| i as f32 / HISTORY as f32).collect(),
            mic_peak: 0.0,
            mic: Mic::Listening,
            thinking: false,
            speaking: true,
            playback: Playback::default(),
            last_frame: Instant::now(),
        }
    }

    fn visible(line: &str) -> String {
        let mut out = String::new();
        let mut escape = false;
        for c in line.chars() {
            match (escape, c) {
                (false, '\x1b') => escape = true,
                (true, 'm') => escape = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn panel_fits_the_terminal_and_rests_flat_without_sound() {
        for width in [40, 80, 160] {
            let lines = live(Paint(tui::Colors::TrueColor)).lines(width);
            for line in &lines {
                assert!(tui::width(&visible(line)) <= width - 2, "{width}: {line}");
            }
            let you = visible(&lines[2]);
            let boubou = visible(&lines[3]);
            assert!(you.contains("You") && boubou.contains("Boubou"));
            assert!(!you.contains('▂'), "Silence draws a flat line");
            assert!(boubou.contains('█'));
            // Narrow terminals drop the notes before the meters.
            assert_eq!(you.contains("microphone"), width >= 80);
            assert_eq!(boubou.contains("on this"), width >= 80);
        }
        let lines = live(Paint(tui::Colors::TrueColor)).lines(100);
        assert!(
            lines[3].contains("\x1b[38;2;242;194;27m"),
            "Boubou speaks in yellow"
        );
        let plain = live(Paint(tui::Colors::None)).lines(100);
        assert!(!plain.iter().any(|line| line.contains("38;")), "NO_COLOR");
    }

    #[test]
    fn meter_height_rises_with_rms() {
        let heights: Vec<char> = [0, 30, 300, 3000, 30000]
            .iter()
            .map(|&amplitude| tui::level_glyph(tui::level(&[amplitude; 480])))
            .collect();
        assert_eq!(heights[0], '▁', "Silence rests on the floor");
        assert!(
            heights.windows(2).all(|pair| pair[0] <= pair[1]),
            "{heights:?}"
        );
        assert_eq!(heights[4], '█', "Near full scale fills the cell");
    }
}
