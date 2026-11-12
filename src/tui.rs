//! Terminal styling shared by guided startup, the startup checklist and the talk line.
//! Plain output (non-TTY, `TERM=dumb`) never reaches this module's escape codes.

use std::io::IsTerminal;

pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// True when `stream` is a terminal that can take cursor movement and redraws.
pub fn interactive(stream: &impl IsTerminal) -> bool {
    stream.is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb")
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Colors {
    /// `NO_COLOR`: layout, glyphs, bold and dim only.
    None,
    Basic,
    Ansi256,
    TrueColor,
}

impl Colors {
    pub fn detect() -> Self {
        let var = |name| std::env::var(name).ok();
        Self::from_env(
            var("NO_COLOR").as_deref(),
            var("COLORTERM").as_deref(),
            var("TERM").as_deref(),
        )
    }

    fn from_env(no_color: Option<&str>, colorterm: Option<&str>, term: Option<&str>) -> Self {
        if no_color.is_some_and(|v| !v.is_empty()) {
            Self::None
        } else if matches!(colorterm, Some("truecolor" | "24bit")) {
            Self::TrueColor
        } else if term.is_some_and(|t| t.contains("256color") || t.contains("direct")) {
            Self::Ansi256
        } else {
            Self::Basic
        }
    }
}

/// A colour token from the website demo, with the 16-colour code it falls back to.
#[derive(Clone, Copy)]
pub struct Tone(u8, u8, u8, u8);

/// Selected row: a raised grey pill with its own light text, readable on light and dark themes.
pub const ROW: Tone = Tone(0x2c, 0x2c, 0x32, 100);
pub const ROW_TEXT: Tone = Tone(0xf4, 0xf4, 0xf6, 97);
/// Boubou's own voice and the "On this Mac" light.
pub const ACCENT: Tone = Tone(0xf2, 0xc2, 0x1b, 33);
pub const TRACK: Tone = Tone(0x4a, 0x4a, 0x52, 100);
pub const WARN: Tone = Tone(0xe8, 0xa3, 0x3d, 33);
/// A QR code's dark and light modules: the same on every theme, as cameras expect.
pub const CODE_DARK: Tone = Tone(0x11, 0x11, 0x13, 30);
pub const CODE_LIGHT: Tone = Tone(0xff, 0xff, 0xff, 97);

#[derive(Clone, Copy)]
pub struct Paint(pub Colors);

impl Paint {
    pub fn detect() -> Self {
        Self(Colors::detect())
    }

    pub fn fg(self, tone: Tone) -> String {
        self.code(tone, 38)
    }

    pub fn bg(self, tone: Tone) -> String {
        self.code(tone, 48)
    }

    fn code(self, Tone(r, g, b, basic): Tone, layer: u8) -> String {
        match self.0 {
            Colors::None => String::new(),
            // Basic codes are foreground numbers; backgrounds sit 10 higher.
            Colors::Basic => format!("\x1b[{}m", basic + if layer == 48 { 10 } else { 0 }),
            Colors::Ansi256 => format!("\x1b[{layer};5;{}m", ansi256(r, g, b)),
            Colors::TrueColor => format!("\x1b[{layer};2;{r};{g};{b}m"),
        }
    }

    pub fn colored(self) -> bool {
        self.0 != Colors::None
    }
}

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const REVERSE: &str = "\x1b[7m";

/// Nearest xterm-256 colour: the 6×6×6 cube or the 24-step grey ramp.
fn ansi256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        (0..6)
            .min_by_key(|&i| STEPS[i].abs_diff(v))
            .expect("six steps")
    };
    let distance = |(x, y, z): (u8, u8, u8)| {
        let d = |a: u8, b: u8| i32::from(a.abs_diff(b)).pow(2);
        d(x, r) + d(y, g) + d(z, b)
    };
    let (i, j, k) = (nearest(r), nearest(g), nearest(b));
    let cube = (STEPS[i], STEPS[j], STEPS[k]);
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let grey_index = (average.saturating_sub(3) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_index;
    if distance((grey, grey, grey)) < distance(cube) {
        232 + grey_index
    } else {
        16 + 36 * i as u8 + 6 * j as u8 + k as u8
    }
}

/// Display columns for one character: wide East Asian and emoji take two, combining marks none.
fn char_width(c: char) -> usize {
    match u32::from(c) {
        0..=0x1f | 0x7f..=0x9f | 0x300..=0x36f | 0x200b..=0x200f | 0xfe00..=0xfe0f => 0,
        0x1100..=0x115f
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Fit `text` into `columns`, ending with … when it had to be cut.
pub fn fit(text: &str, columns: usize) -> String {
    if width(text) <= columns {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w + 1 > columns {
            break;
        }
        used += w;
        out.push(c);
    }
    if columns > 0 {
        out.push('…');
    }
    out
}

/// Fit and left-align `text` in exactly `columns`.
pub fn pad(text: &str, columns: usize) -> String {
    let text = fit(text, columns);
    let gap = columns.saturating_sub(width(&text));
    format!("{text}{}", " ".repeat(gap))
}

/// Fit and right-align `text` in exactly `columns`.
pub fn pad_left(text: &str, columns: usize) -> String {
    let text = fit(text, columns);
    let gap = columns.saturating_sub(width(&text));
    format!("{}{text}", " ".repeat(gap))
}

/// Break `text` at its spaces into lines of at most `columns`, cutting a word that is longer.
pub fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match lines.last_mut() {
            Some(line) if width(line) + 1 + width(word) <= columns => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(fit(word, columns)),
        }
    }
    lines
}

/// Boubou's toucan, a terminal cell per character: `#` body, `y` beak. Whole cells stay solid
/// where a terminal spaces its lines, which stripes block characters.
const TOUCAN: [&str; 6] = [
    "  ####",
    " ######yyyyyyyyy",
    "###  ##yyyyyyyyyyyy",
    "#######yyyyyyyyyyyyy",
    "########     yyyyyy",
    "##########",
];

/// The top of a screen: beside the toucan, the product name and the local-status `light` (its
/// style, then its text) over a rule, then what `rest` makes of the columns it is given.
pub fn header(
    paint: Paint,
    columns: usize,
    (style, light): (&str, &str),
    rest: impl FnOnce(usize) -> Vec<String>,
) -> Vec<String> {
    // Narrow terminals keep their columns for the text.
    let (toucan, gap): (&[&str], usize) = if columns < 60 {
        (&[], 0)
    } else {
        (&TOUCAN, 22)
    };
    // Margins come off the width, and so do the toucan and its gap.
    let room = columns.saturating_sub(4 + gap);
    let mut lines = vec![
        format!(
            "{BOLD}boubou{RESET}{}{style}{light}",
            " ".repeat(room.saturating_sub(6 + width(light)))
        ),
        format!("{DIM}{}", "─".repeat(room)),
    ];
    lines.extend(rest(room));
    lines.resize(lines.len().max(toucan.len()), String::new());
    // The body takes the terminal's own text colour; without colour the beak is its dim twin.
    let beak = if paint.colored() {
        paint.bg(ACCENT)
    } else {
        format!("{DIM}{REVERSE}")
    };
    toucan
        .iter()
        .chain(std::iter::repeat(&""))
        .zip(lines)
        .map(|(row, line)| {
            let art: String = format!("{row:gap$}")
                .chars()
                .map(|cell| match cell {
                    '#' => format!("{REVERSE} {RESET}"),
                    'y' => format!("{beak} {RESET}"),
                    _ => " ".to_owned(),
                })
                .collect();
            format!("  {art}{line}")
        })
        .collect()
}

/// Decimal gigabytes, as the model library lists sizes.
pub fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

/// Voice picker columns: mark, name, detail and a right-aligned size.
#[derive(Debug, PartialEq)]
pub struct Columns {
    pub name: usize,
    /// Zero hides the detail column on narrow terminals.
    pub detail: usize,
    pub size: usize,
}

/// Lead-in before the name: indent, mark and gap.
pub const ROW_LEAD: usize = 6;

impl Columns {
    pub fn fit<'a>(
        names: impl IntoIterator<Item = &'a str>,
        sizes: impl IntoIterator<Item = &'a str>,
        columns: usize,
    ) -> Self {
        let name = names.into_iter().map(width).max().unwrap_or(0).min(24);
        let size = sizes.into_iter().map(width).max().unwrap_or(0);
        // Lead, two gaps of two and a trailing space around the size.
        let fixed = ROW_LEAD + 2 + 2 + 1;
        let room = columns.saturating_sub(fixed + name + size);
        if room >= 12 {
            return Self {
                name,
                detail: room,
                size,
            };
        }
        let name = columns
            .saturating_sub(ROW_LEAD + 2 + 1 + size)
            .min(name.max(1));
        Self {
            name,
            detail: 0,
            size,
        }
    }
}

/// A smooth bar: full cells, one eighth-block partial cell, then empty track cells.
pub fn bar(done: u64, total: u64, cells: usize) -> (usize, Option<char>, usize) {
    let eighths = if total == 0 {
        cells * 8
    } else {
        (u128::from(done.min(total)) * cells as u128 * 8 / u128::from(total)) as usize
    };
    let full = eighths / 8;
    let partial = (full < cells && eighths % 8 > 0).then(|| EIGHTHS[eighths % 8 - 1]);
    (full, partial, cells - full - usize::from(partial.is_some()))
}

/// Loudness of PCM16 audio from 0 (silence) to 1 (loud speech), on a decibel scale.
pub fn level(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let power = pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / pcm.len() as f64;
    let db = 10.0 * (power.max(1.0) / f64::from(i16::MAX).powi(2)).log10();
    // -55 dBFS is a quiet room; -12 dBFS is close, loud speech.
    ((db + 55.0) / 43.0).clamp(0.0, 1.0) as f32
}

/// One meter cell for a level from 0 to 1.
pub fn level_glyph(level: f32) -> char {
    LEVELS[((level.clamp(0.0, 1.0) * 7.0).round()) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_depth_honours_no_color_and_falls_back_without_truecolor() {
        let depth = Colors::from_env;
        assert_eq!(
            depth(Some("1"), Some("truecolor"), Some("xterm-256color")),
            Colors::None
        );
        assert_eq!(
            depth(Some(""), Some("truecolor"), None),
            Colors::TrueColor,
            "An empty NO_COLOR is unset"
        );
        assert_eq!(depth(None, Some("24bit"), Some("xterm")), Colors::TrueColor);
        assert_eq!(depth(None, None, Some("xterm-256color")), Colors::Ansi256);
        assert_eq!(depth(None, None, Some("xterm")), Colors::Basic);
        assert_eq!(Paint(Colors::None).fg(ACCENT), "");
        assert_eq!(Paint(Colors::Basic).bg(ACCENT), "\x1b[43m");
        assert_eq!(Paint(Colors::TrueColor).fg(ACCENT), "\x1b[38;2;242;194;27m");
    }

    #[test]
    fn nearest_256_colour_uses_the_cube_or_grey_ramp() {
        assert_eq!(ansi256(0, 0, 0), 16);
        assert_eq!(ansi256(255, 255, 255), 231);
        assert_eq!(ansi256(255, 0, 0), 196);
        assert_eq!(ansi256(0x2c, 0x2c, 0x32), 236);
        assert_eq!(ansi256(0xf2, 0xc2, 0x1b), 214);
    }

    #[test]
    fn fitting_truncates_with_an_ellipsis_by_display_width() {
        assert_eq!(fit("Liquid Audio 2B", 20), "Liquid Audio 2B");
        assert_eq!(fit("Liquid Audio 2B", 8), "Liquid …");
        assert_eq!(fit("Liquid", 1), "…");
        assert_eq!(fit("Liquid", 0), "");
        assert_eq!(fit("你好世界", 5), "你好…");
        assert_eq!(width("你好"), 4);
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad_left("ab", 4), "  ab");
        assert_eq!(pad_left("abcdef", 4), "abc…");
        assert_eq!(gb(1_620_000_000), "1.62 GB");
        assert_eq!(
            wrap("boubou serve qwen3-tts:0.6b-customvoice --voice Vivian", 32),
            [
                "boubou serve",
                "qwen3-tts:0.6b-customvoice",
                "--voice Vivian"
            ]
        );
        assert_eq!(
            wrap("boubou run a-long-name", 8),
            ["boubou", "run", "a-long-…"]
        );
    }

    #[test]
    fn columns_keep_sizes_aligned_and_drop_detail_when_narrow() {
        let names = ["Qwen 0.6B", "Liquid Audio 2B"];
        let sizes = ["3.10 GB", "12.40 GB"];
        let wide = Columns::fit(names, sizes, 80);
        assert_eq!(wide.name, 15);
        assert_eq!(wide.size, 8);
        assert_eq!(
            ROW_LEAD + wide.name + 2 + wide.detail + 2 + wide.size + 1,
            80
        );
        let narrow = Columns::fit(names, sizes, 36);
        assert_eq!(narrow.detail, 0);
        assert_eq!(narrow.name, 15);
        let tiny = Columns::fit(names, sizes, 24);
        assert_eq!(ROW_LEAD + tiny.name + 2 + tiny.size + 1, 24);
        assert_eq!(tiny.detail, 0);
    }

    #[test]
    fn bar_moves_in_eighths_and_stays_within_its_cells() {
        assert_eq!(bar(0, 100, 10), (0, None, 10));
        assert_eq!(bar(100, 100, 10), (10, None, 0));
        assert_eq!(bar(500, 100, 10), (10, None, 0));
        assert_eq!(bar(0, 0, 4), (4, None, 0));
        assert_eq!(bar(50, 100, 10), (5, None, 5));
        // 1/80 of the way is one eighth of the first cell.
        assert_eq!(bar(1, 80, 10), (0, Some('▏'), 9));
        assert_eq!(bar(15, 80, 10), (1, Some('▉'), 8));
        for done in 0..=80 {
            let (full, partial, empty) = bar(done, 80, 10);
            assert_eq!(full + usize::from(partial.is_some()) + empty, 10);
        }
    }

    #[test]
    fn level_meter_maps_silence_to_the_floor_and_speech_upwards() {
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0; 320]), 0.0);
        assert_eq!(level(&[i16::MAX; 320]), 1.0);
        let quiet = level(&[300; 320]);
        let loud = level(&[8000; 320]);
        assert!(0.0 < quiet && quiet < loud && loud < 1.0);
        assert_eq!(level_glyph(0.0), '▁');
        assert_eq!(level_glyph(1.0), '█');
        assert_eq!(level_glyph(2.0), '█');
        assert_eq!(level_glyph(0.5), '▅');
    }
}
