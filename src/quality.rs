//! Whether a recording will clone well: loud enough, not distorted, quiet behind the voice,
//! with enough speech, and saying the words it should. Cloning copies everything it hears, so
//! the clone page says what to fix before a voice is saved.

use anyhow::{Context, Result};
use serde_json::json;
use std::{fs, io, path::Path};

/// The voice's level, in dBFS, below which Boubou hears it faintly.
const FAINT: f64 = -40.0;
/// The share of samples at full scale above which a recording is distorted.
const DISTORTED: f64 = 0.001;
/// How far, in dB, the voice must stand above the noise behind it.
const CLEAR: f64 = 20.0;
/// The seconds of speech a voice needs.
const SPEECH: f64 = 3.0;
/// How close the words heard must be to the words to say.
const SAME: f64 = 0.8;

/// What a recording sounds like.
#[derive(Debug, PartialEq)]
pub struct Measure {
    pub seconds: f64,
    /// Seconds in which the voice speaks.
    pub speech: f64,
    /// The voice's level, in dBFS: the loud end of the recording.
    pub level: f64,
    /// The noise behind it, in dBFS: the quiet end.
    pub noise: f64,
    /// The share of samples at full scale.
    pub clipped: f64,
}

/// Measures a WAV recording in 20 ms frames.
pub fn measure(wav: impl io::Read) -> Result<Measure> {
    let mut reader = hound::WavReader::new(io::BufReader::new(wav))
        .context("Boubou cannot read this WAV file")?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let full = 2f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|sample| sample as f32 / full))
                .collect::<Result<_, _>>()?
        }
    };
    let clipped = samples
        .iter()
        .filter(|sample| sample.abs() >= 0.999)
        .count();
    let channels = usize::from(spec.channels.max(1));
    let mono: Vec<f32> = samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect();
    let rate = f64::from(spec.sample_rate.max(1));
    let size = (spec.sample_rate as usize / 50).max(1);
    let mut levels: Vec<f64> = mono
        .chunks(size)
        .map(|frame| {
            let power =
                frame.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / frame.len() as f64;
            (10.0 * power.log10()).max(-100.0)
        })
        .collect();
    levels.sort_by(f64::total_cmp);
    let at = |share: f64| {
        levels
            .get(((levels.len().saturating_sub(1)) as f64 * share).round() as usize)
            .copied()
            .unwrap_or(-100.0)
    };
    let (level, noise) = (at(0.9), at(0.1));
    // A frame speaks when it stands well above the noise, or near the voice's level in a noisy
    // recording, and is not silence.
    let threshold = (noise + 12.0).min(level - 10.0).max(-70.0);
    let speaking = levels.iter().filter(|frame| **frame > threshold).count();
    Ok(Measure {
        seconds: mono.len() as f64 / rate,
        speech: (speaking * size) as f64 / rate,
        level,
        noise,
        clipped: clipped as f64 / samples.len().max(1) as f64,
    })
}

/// How alike two texts are, from 0 to 1: letters and digits only, ignoring case, so it suits
/// languages written with spaces and without.
pub fn alike(a: &str, b: &str) -> f64 {
    let letters = |text: &str| -> Vec<char> {
        text.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let (a, b) = (letters(a), letters(b));
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    // Levenshtein distance, a row at a time.
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(x != y));
            diagonal = above;
        }
    }
    1.0 - row[b.len()] as f64 / a.len().max(b.len()) as f64
}

/// What to fix before cloning, if anything. `words` are what the recording should say, and
/// `heard` what Whisper heard.
pub fn advice(measure: &Measure, words: Option<(&str, &str)>) -> Vec<String> {
    let mut advice = Vec::new();
    if measure.level < FAINT {
        advice.push(
            "Boubou hears the voice faintly. Move closer to the microphone, or turn its level up."
                .into(),
        );
    }
    if measure.clipped > DISTORTED {
        advice.push(
            "The voice is distorted: it was too loud. Move back a little from the microphone."
                .into(),
        );
    }
    if measure.level - measure.noise < CLEAR && measure.level >= FAINT {
        advice.push("There is noise behind the voice, and the clone would copy it. Record somewhere quieter.".into());
    }
    if measure.speech < SPEECH {
        advice.push(format!(
            "Boubou heard only {:.1} seconds of speech. Give about 10.",
            measure.speech
        ));
    }
    if let Some((words, heard)) = words
        && !words.trim().is_empty()
        && alike(words, heard) < SAME
    {
        advice.push(format!(
            "Boubou heard other words: “{}” The clone learns from the words as written, so read them exactly, or keep the words Boubou heard.",
            heard.trim()
        ));
    }
    advice
}

/// `boubou voices --check`: says whether a recording will clone well.
pub fn command(path: &Path, words: Option<&str>, heard: Option<&str>, json: bool) -> Result<()> {
    let measure =
        measure(fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?)?;
    let words = words.zip(Some(heard.unwrap_or_default()));
    let advice = advice(&measure, words);
    // Whether the page should offer the words Whisper heard instead.
    let differ =
        words.is_some_and(|(words, heard)| !words.trim().is_empty() && alike(words, heard) < SAME);
    if json {
        let Measure {
            seconds,
            speech,
            level,
            noise,
            clipped,
        } = measure;
        let answer = json!({
            "seconds": seconds,
            "speech": speech,
            "level": level,
            "noise": noise,
            "clipped": clipped,
            "advice": advice,
            "differ": differ,
        });
        println!("{answer}");
        return Ok(());
    }
    println!(
        "{}: {:.1} s, {:.1} s of speech, voice at {:.0} dBFS, noise at {:.0} dBFS.",
        path.display(),
        measure.seconds,
        measure.speech,
        measure.level,
        measure.noise
    );
    if advice.is_empty() {
        println!("It should clone well.");
    }
    for line in advice {
        println!("- {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten seconds at 24 kHz: a voice, as tones that speak for 0.4 s and pause for 0.1 s, at
    /// `voice`, for `speaking` seconds, over noise at `noise`, clipped at full scale.
    fn take(voice: f32, noise: f32, speaking: f32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut file = io::Cursor::new(Vec::new());
        let mut wav = hound::WavWriter::new(&mut file, spec).unwrap();
        let mut seed = 1u32;
        for i in 0..240_000 {
            let time = i as f32 / 24000.0;
            let on = time < speaking && time % 0.5 < 0.4;
            let tone = if on {
                (time * 220.0 * std::f32::consts::TAU).sin() * voice
            } else {
                0.0
            };
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let hiss = (seed >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
            let sample = (tone + hiss * noise).clamp(-1.0, 1.0);
            wav.write_sample((sample * 32767.0) as i16).unwrap();
        }
        wav.finalize().unwrap();
        file.into_inner()
    }

    fn checked(wav: Vec<u8>) -> (Measure, Vec<String>) {
        let measure = measure(wav.as_slice()).unwrap();
        let advice = advice(&measure, None);
        (measure, advice)
    }

    #[test]
    fn a_clear_take_needs_nothing() {
        let (measure, advice) = checked(take(0.3, 0.0003, 10.0));
        assert!((measure.seconds - 10.0).abs() < 0.01);
        assert!((7.5..8.5).contains(&measure.speech), "{measure:?}");
        assert!((-17.0..-12.0).contains(&measure.level), "{measure:?}");
        assert!(measure.noise < -60.0, "{measure:?}");
        assert_eq!(measure.clipped, 0.0);
        assert!(advice.is_empty(), "{advice:?}");
    }

    #[test]
    fn faint_distorted_noisy_and_short_takes_say_what_to_fix() {
        let says = |wav, what: &str| {
            let (measure, advice) = checked(wav);
            assert_eq!(advice.len(), 1, "{measure:?} {advice:?}");
            assert!(advice[0].contains(what), "{advice:?}");
        };
        says(take(0.003, 0.000_003, 10.0), "faintly");
        says(take(3.0, 0.0003, 10.0), "distorted");
        says(take(0.3, 0.06, 10.0), "noise behind the voice");
        says(take(0.3, 0.0003, 2.0), "only 1.6 seconds");
        // Silence is faint and has no speech.
        let (_, advice) = checked(take(0.0, 0.0, 0.0));
        assert_eq!(advice.len(), 2, "{advice:?}");
        assert!(measure(&b"not audio"[..]).is_err());
    }

    #[test]
    fn the_words_heard_are_compared_with_the_words_to_say() {
        let say = "The weather is lovely today, so let's take a slow walk to the park.";
        assert_eq!(
            alike(
                say,
                "the weather is lovely today so lets take a slow walk to the park"
            ),
            1.0
        );
        assert!(
            alike(
                say,
                "The weather's lovely today, so let's take a slow walk to the park."
            ) > 0.95
        );
        assert!(alike(say, "Hello, is this thing on?") < 0.5);
        assert!(
            alike(
                "今天天气很好，我们一起去公园散步吧。",
                "今天天气很好我们一起去公园散步吧"
            ) == 1.0
        );
        assert!(
            alike(
                "今天天气很好，我们一起去公园散步吧。",
                "今天天气不好，我们回家吧。"
            ) < SAME
        );
        assert_eq!(alike("", ""), 1.0);

        let clear = measure(take(0.3, 0.0003, 10.0).as_slice()).unwrap();
        assert!(advice(&clear, Some((say, say))).is_empty());
        let other = advice(&clear, Some((say, " Hello, is this thing on? ")));
        assert_eq!(other.len(), 1);
        assert!(other[0].starts_with("Boubou heard other words: “Hello, is this thing on?”"));
        // Without words to say, as for an uploaded clip, any words will do.
        assert!(advice(&clear, Some(("", "Anything."))).is_empty());
    }
}
