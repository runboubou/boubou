use crate::{assets::Assets, profiles::Profile};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

pub struct Input {
    path: PathBuf,
    video: bool,
}

impl Input {
    pub fn new(profile: &Profile, path: &Path, question: &str) -> Result<Self> {
        ensure!(
            profile.media_cli.is_some(),
            "{} does not support image/video questions",
            profile.id
        );
        ensure!(
            !question.trim().is_empty() && question.len() <= 4096,
            "The question must contain 1–4096 UTF-8 bytes"
        );
        let metadata =
            fs::metadata(path).with_context(|| format!("Cannot read {}", path.display()))?;
        ensure!(
            metadata.is_file() && metadata.len() > 0 && metadata.len() <= 64 * 1024 * 1024,
            "Media must be a nonempty regular file of at most 64 MiB"
        );
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        ensure!(
            matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "mp4" | "mov"),
            "Supported media: PNG/JPEG images and MP4/MOV videos"
        );
        let path = path.canonicalize()?;
        ensure!(path.to_str().is_some(), "Media paths must use valid UTF-8");
        Ok(Self {
            path,
            video: matches!(extension.as_str(), "mp4" | "mov"),
        })
    }

    pub fn answer(
        &self,
        assets: &Assets,
        cache: &Path,
        question: &str,
        stopped: &AtomicBool,
    ) -> Result<()> {
        let profile = assets.profile;
        let media_tokens = if self.video {
            let output = tool_output(
                Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-show_entries",
                        "stream=codec_type,width,height,r_frame_rate,avg_frame_rate,nb_frames:format=duration",
                        "-of",
                        "json",
                    ])
                    .arg(&self.path)
                    .stderr(Stdio::null()),
                stopped,
                Duration::from_secs(10),
            )
            .context(
                "Cannot inspect video; install ffmpeg and ffprobe in PATH and check the file",
            )?;
            let frames = video_frames(
                &serde_json::from_slice(&output).context("Invalid ffprobe metadata")?,
            )?;
            // Fail before inference if a soundtrack or picture track cannot be decoded.
            tool_output(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-nostdin", "-i"])
                    .arg(&self.path)
                    .args([
                        "-map", "0:v:0", "-map", "0:a:0?", "-t", "10", "-f", "null", "-",
                    ]),
                stopped,
                Duration::from_secs(15),
            )
            .context("Cannot decode this video with ffmpeg")?;
            // Reserve 1024 tokens per sampled frame, plus the whole 10-second soundtrack.
            frames * 1024 + 160_000_usize.div_ceil(profile.audio_samples_per_token)
        } else {
            let output = tool_output(
                Command::new("/usr/bin/sips")
                    .args(["-g", "pixelWidth", "-g", "pixelHeight"])
                    .arg(&self.path)
                    .stderr(Stdio::null()),
                stopped,
                Duration::from_secs(10),
            )
            .context("Cannot inspect this PNG/JPEG image")?;
            image_tokens(std::str::from_utf8(&output).context("Invalid image metadata")?)?
        };
        check_context(profile, question.len(), media_tokens)?;
        let executable = profile
            .media_cli
            .context("This model does not support media questions")?;
        let output = tool_output(
            Command::new(cache.join(profile.runtime.directory).join(executable))
                .arg("--model")
                .arg(&assets.model)
                .arg("--mmproj")
                .arg(&assets.projector)
                .arg(if self.video { "--video" } else { "--image" })
                // Upstream parses this argument as CSV even for a single file.
                .arg(format!(
                    "\"{}\"",
                    self.path
                        .to_str()
                        .expect("validated UTF-8")
                        .replace('"', "\"\"")
                ))
                .args([
                    "--prompt",
                    question,
                    "--system-prompt",
                    "Respond with text. Answer the question using what you see and hear.",
                    "--ctx-size",
                    &profile.context_tokens.to_string(),
                    "--n-predict",
                    &profile.max_output_tokens.to_string(),
                    "--temp",
                    "0",
                    "--offline",
                    "--log-verbosity",
                    "1",
                ])
                .env_remove("LLAMA_ARG_LOG_FILE"),
            stopped,
            Duration::from_secs(180),
        )
        .context("Local media inference failed; check decoding support and input limits")?;
        let text = String::from_utf8(output).context("The model returned invalid UTF-8")?;
        let text = text.trim();
        // The pinned non-reasoning Omni CLI emits this empty template prefix.
        let text = text.strip_prefix("<think></think>").unwrap_or(text).trim();
        ensure!(!text.is_empty(), "The model returned no answer");
        println!("{text}");
        Ok(())
    }
}

fn image_tokens(metadata: &str) -> Result<usize> {
    let dimension = |key: &str| {
        metadata
            .lines()
            .rev()
            .find_map(|line| line.trim().strip_prefix(key)?.trim().parse::<usize>().ok())
            .context("Cannot read image dimensions")
    };
    let (width, height) = (dimension("pixelWidth:")?, dimension("pixelHeight:")?);
    ensure!(
        width > 0 && height > 0 && width <= 2048 && height <= 2048,
        "Images must be at most 2048 × 2048 pixels"
    );
    // Reserve the largest accepted image (16-pixel patches merged in 2 × 2 groups),
    // including headroom for the model's resizing of very small images.
    Ok(4096)
}

fn video_frames(metadata: &Value) -> Result<usize> {
    let stream = metadata["streams"]
        .as_array()
        .and_then(|streams| {
            streams
                .iter()
                .find(|stream| stream["codec_type"] == "video")
        })
        .context("No video stream found")?;
    let width = stream["width"].as_u64().unwrap_or(0);
    let height = stream["height"].as_u64().unwrap_or(0);
    ensure!(
        width > 0 && height > 0 && width <= 512 && height <= 512,
        "Videos must be at most 512 × 512 pixels"
    );
    let frames = stream["nb_frames"]
        .as_str()
        .and_then(|n| n.parse::<u64>().ok())
        .context("Video must have a known frame count (use a constant-frame-rate MP4/MOV)")?;
    let fps = stream["r_frame_rate"]
        .as_str()
        .and_then(|s| s.split_once('/'))
        .and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?))
        .context("Video has no valid frame rate")?;
    let duration = metadata["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .context("Video has no valid duration")?;
    ensure!(
        fps.is_finite()
            && fps > 0.0
            && fps <= 60.0
            && stream["avg_frame_rate"] == stream["r_frame_rate"]
            && (4..=600).contains(&frames)
            && duration.is_finite()
            && duration > 0.0
            && duration <= 10.0
            && frames as f64 / fps <= 10.0
            && (frames as f64 / fps - duration).abs() <= 0.25,
        "Videos must be at most 10 seconds, with 4–600 frames at a constant rate up to 60 fps"
    );
    // Matches the pinned LFM3-Omni frame sampler, including its minimum of four.
    Ok(((frames as f64 / fps * 2.0) as usize)
        .clamp(4, 384)
        .min(frames as usize))
}

fn check_context(profile: &Profile, question_bytes: usize, media_tokens: usize) -> Result<()> {
    ensure!(
        media_tokens + question_bytes + profile.max_output_tokens + 1024 < profile.context_tokens,
        "Media and question exceed this model's context capacity"
    );
    Ok(())
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tool_output(command: &mut Command, stopped: &AtomicBool, timeout: Duration) -> Result<Vec<u8>> {
    const MAX_OUTPUT: u64 = 64 * 1024;
    crate::check_cancel(stopped)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut process = Process(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("Cannot start {}", command.get_program().to_string_lossy()))?,
    );
    let output = process.0.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let result = (|| {
        loop {
            crate::check_cancel(stopped)?;
            if let Some(status) = process.0.try_wait()? {
                ensure!(status.success(), "Local media tool exited ({status})");
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "Local media tool timed out");
            thread::sleep(Duration::from_millis(20));
        }
    })();
    drop(process); // Close all decoder descendants before joining the pipe reader.
    let output = reader
        .join()
        .map_err(|_| anyhow!("Cannot read media tool output"))??;
    result?;
    crate::check_cancel(stopped)?;
    ensure!(
        output.len() as u64 <= MAX_OUTPUT,
        "Media tool output exceeded 64 KiB"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{DEFAULT, OMNI};
    use serde_json::json;

    #[test]
    fn validates_model_and_local_input_before_asset_preparation() {
        let directory =
            std::env::temp_dir().join(format!("boubou-media-input-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("a picture.PNG");
        let error = Input::new(&DEFAULT, &path, "What is here?").err().unwrap();
        assert!(error.to_string().contains("does not support"));
        assert!(Input::new(&OMNI, &path, "What is here?").is_err());
        fs::write(&path, []).unwrap();
        assert!(Input::new(&OMNI, &path, "What is here?").is_err());
        fs::write(&path, b"image").unwrap();
        assert!(Input::new(&OMNI, &path, "What is here?").is_ok());
        assert!(Input::new(&OMNI, &path, "图片中有什么？").is_ok());
        for question in [" ".to_owned(), "x".repeat(4097), "é".repeat(2049)] {
            assert!(Input::new(&OMNI, &path, &question).is_err());
        }
        assert!(Input::new(&OMNI, &path, &"x".repeat(4096)).is_ok());
        assert!(Input::new(&OMNI, &directory, "What is here?").is_err());
        let unsupported = directory.join("picture.gif");
        fs::write(&unsupported, b"image").unwrap();
        assert!(Input::new(&OMNI, &unsupported, "What is here?").is_err());
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        assert!(Input::new(&OMNI, &path, "What is here?").is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_invalid_image_dimensions_and_reserves_context() {
        assert_eq!(
            image_tokens("pixelWidth: 2048\npixelHeight: 2048").unwrap(),
            4096
        );
        for metadata in [
            "",
            "pixelWidth: 1",
            "pixelWidth: 0\npixelHeight: 1",
            "pixelWidth: 2049\npixelHeight: 1",
        ] {
            assert!(image_tokens(metadata).is_err());
        }
        assert!(check_context(&OMNI, 4096, 20 * 1024 + 125).is_ok());
        let small = Profile {
            context_tokens: 4096,
            ..OMNI
        };
        assert!(check_context(&small, 1, 4096).is_err());
    }

    #[test]
    fn video_limits_preserve_the_pinned_frame_sampler() {
        let metadata = json!({"streams":[{
            "codec_type":"video", "width":320, "height":240,
            "r_frame_rate":"8/1", "avg_frame_rate":"8/1", "nb_frames":"32"
        }],"format":{"duration":"4.0"}});
        assert_eq!(video_frames(&metadata).unwrap(), 8);
        let mut short = metadata.clone();
        short["streams"][0]["nb_frames"] = json!("4");
        short["format"]["duration"] = json!("0.5");
        assert_eq!(video_frames(&short).unwrap(), 4);
        for (key, value) in [
            ("width", json!(513)),
            ("height", json!(0)),
            ("codec_type", json!("audio")),
            ("r_frame_rate", json!("0/0")),
            ("r_frame_rate", json!("NaN/1")),
            ("avg_frame_rate", json!("7/1")),
            ("nb_frames", json!("N/A")),
            ("nb_frames", json!("601")),
        ] {
            let mut invalid = metadata.clone();
            invalid["streams"][0][key] = value;
            assert!(video_frames(&invalid).is_err(), "{invalid}");
        }
        for duration in ["NaN", "-1", "0", "10.01", "2"] {
            let mut invalid = metadata.clone();
            invalid["format"]["duration"] = json!(duration);
            assert!(video_frames(&invalid).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn media_tools_bound_output_and_report_failures() {
        let stopped = AtomicBool::new(false);
        let result = tool_output(
            Command::new("sh").args(["-c", "printf answer"]),
            &stopped,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(result, b"answer");
        assert!(
            tool_output(
                Command::new("sh").args(["-c", "exit 3"]),
                &stopped,
                Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(
            tool_output(
                Command::new("sh").args(["-c", "head -c 70000 /dev/zero"]),
                &stopped,
                Duration::from_secs(2)
            )
            .is_err()
        );
        let began = Instant::now();
        assert!(
            tool_output(
                Command::new("sleep").arg("30"),
                &stopped,
                Duration::from_millis(30)
            )
            .is_err()
        );
        assert!(began.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_the_owned_runner_and_decoder() {
        use std::sync::{Arc, atomic::Ordering};
        let path = std::env::temp_dir().join(format!("boubou-media-pid-{}", std::process::id()));
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        let marker = path.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !fs::read_to_string(&marker).is_ok_and(|s| {
                s.split_whitespace().count() == 2
                    && s.split_whitespace().all(|pid| pid.parse::<i32>().is_ok())
            }) {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
            signal.store(true, Ordering::SeqCst);
        });
        let began = Instant::now();
        let error = tool_output(
            Command::new("sh")
                .args([
                    "-c",
                    "sleep 30 & printf '%s %s\\n' \"$$\" \"$!\" > \"$1\"; wait",
                    "boubou-media-test",
                ])
                .arg(&path),
            &stopped,
            Duration::from_secs(3),
        )
        .unwrap_err();
        worker.join().unwrap();
        assert!(error.is::<crate::Cancelled>());
        // The decoder inherits stdout; joining promptly proves it was stopped too.
        assert!(began.elapsed() < Duration::from_secs(2));
        let pid: i32 = fs::read_to_string(&path)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fs::remove_file(path).unwrap();
    }
}
