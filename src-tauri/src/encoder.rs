// FFmpeg encoder - converts WebM recordings to MP4 optimized for smooth seeking,
// and fixes WebM metadata (regenerate PTS) without re-encoding.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex::Regex;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager};

// Compiled once per process: run_ffmpeg runs per conversion, and recompiling the
// progress patterns on every call is pure overhead.
static DURATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Duration:\s*(\d{2}):(\d{2}):(\d{2})\.(\d{1,3})").unwrap());
static TIME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"time=(\d{2}):(\d{2}):(\d{2})\.(\d{1,3})").unwrap());

#[derive(Clone, serde::Serialize)]
struct ConvertProgress {
    #[serde(rename = "fileName")]
    file_name: String,
    percent: u32,
    /// "finalize" (metadata fix) or "convert" (MP4 transcode)
    stage: String,
}

/// Locate ffmpeg: bundled resource -> FFMPEG_PATH env -> system PATH
fn ffmpeg_path(app: &AppHandle) -> PathBuf {
    if let Ok(p) = app
        .path()
        .resolve("binaries/ffmpeg.exe", BaseDirectory::Resource)
    {
        if p.exists() {
            return p;
        }
    }
    if let Ok(p) = std::env::var("FFMPEG_PATH") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    PathBuf::from("ffmpeg")
}

/// The percentage to report from ffmpeg's stderr so far, or None when the buffer
/// carries no status line yet. Capped at 99: only a successful exit reports 100.
fn progress_percent(acc: &str, total: f64) -> Option<i64> {
    let caps = TIME_RE.captures_iter(acc).last()?;
    let current = captures_to_secs(&caps);
    Some(((current / total) * 100.0).round().min(99.0) as i64)
}

/// The seconds a `Duration:` or `time=` capture stands for, or None when the buffer
/// has not carried a duration header yet.
fn header_duration(acc: &str) -> Option<f64> {
    DURATION_RE.captures(acc).map(|caps| captures_to_secs(&caps))
}

fn captures_to_secs(caps: &regex::Captures) -> f64 {
    let h: f64 = caps[1].parse().unwrap_or(0.0);
    let m: f64 = caps[2].parse().unwrap_or(0.0);
    let s: f64 = caps[3].parse().unwrap_or(0.0);
    let frac_str = &caps[4];
    let frac: f64 = frac_str.parse::<f64>().unwrap_or(0.0) / 10f64.powi(frac_str.len() as i32);
    h * 3600.0 + m * 60.0 + s + frac
}

/// One ffmpeg invocation: run `[input_args] -i input [args] -y output`. FFmpeg applies
/// an option to the next file on the command line, so demuxer flags belong in
/// `input_args`, before `-i`; encoder and muxer flags follow in `args`.
/// `stage` names the progress events to emit; `None` runs silently.
struct FfmpegJob<'a> {
    input_args: &'a [&'a str],
    input: &'a Path,
    output: &'a Path,
    args: &'a [&'a str],
    stage: Option<&'a str>,
    /// Total seconds to measure progress against when the input carries no
    /// `Duration:` header. MediaRecorder writes a live WebM with an unknown-size
    /// Segment, so without this the percentage never leaves 0.
    fallback_duration: Option<f64>,
}

fn run_ffmpeg(app: &AppHandle, job: &FfmpegJob) -> Result<(), String> {
    let ffmpeg = ffmpeg_path(app);
    let mut cmd = Command::new(&ffmpeg);
    for a in job.input_args {
        cmd.arg(a);
    }
    cmd.arg("-i").arg(job.input);
    for a in job.args {
        cmd.arg(a);
    }
    cmd.arg("-y").arg(job.output);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("FFmpeg not found or failed to start: {e}"))?;
    let mut stderr = child.stderr.take().expect("stderr piped");

    let file_name = job
        .input
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let emit = |percent: u32| {
        if let Some(stage) = job.stage {
            let _ = app.emit(
                "convert-progress",
                ConvertProgress {
                    file_name: file_name.clone(),
                    percent,
                    stage: stage.to_string(),
                },
            );
        }
    };
    emit(0);

    let mut acc = String::new();
    let mut duration: Option<f64> = None;
    let mut last_pct: i64 = -1;
    let mut buf = [0u8; 4096];
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
        if duration.is_none() {
            duration = header_duration(&acc);
        }
        // A real header always wins; the caller's estimate only fills the gap
        let total = duration.or(job.fallback_duration).filter(|d| *d > 0.0);
        if let (Some(_), Some(d)) = (job.stage, total) {
            if let Some(pct) = progress_percent(&acc, d) {
                if pct != last_pct {
                    last_pct = pct;
                    emit(pct as u32);
                }
            }
        }
        // Keep a bounded tail. FFmpeg prints a status line about twice a second, so
        // holding the whole log and rescanning it on every read is quadratic over a
        // long encode. Trim once a total is known, from the header or the caller's
        // estimate. The cut lands on a char boundary rather than a line break, which
        // is safe: the scans above run first and take the newest match, and the
        // retained tail is about 4 KiB, hundreds of times the length of one match.
        let cap = if total.is_some() { 8192 } else { 262_144 };
        if acc.len() > cap {
            let target = acc.len() - cap / 2;
            let cut = (target..acc.len())
                .find(|i| acc.is_char_boundary(*i))
                .unwrap_or(acc.len());
            acc.drain(..cut);
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("FFmpeg process error: {e}"))?;

    if status.success() {
        emit(100);
        Ok(())
    } else {
        let tail: String = acc.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
        Err(format!(
            "FFmpeg failed (exit {}): {}",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
            if tail.is_empty() { "Unknown error" } else { &tail }
        ))
    }
}

/// Convert to MP4 (H.264) - 60 fps, keyframe every 2s (GOP=120), yuv420p, faststart
pub fn convert_to_mp4(
    app: &AppHandle,
    input: &Path,
    output: &Path,
    emit_progress: bool,
    fallback_duration: Option<f64>,
) -> Result<(), String> {
    let stage = if emit_progress { Some("convert") } else { None };
    let job = FfmpegJob {
        // MediaRecorder output carries no duration; fill in the missing PTS while decoding
        input_args: &["-fflags", "+genpts"],
        input,
        output,
        // H.264 - 60 fps, keyframe every 2s (GOP=120), yuv420p, faststart
        args: &[
            "-c:v", "libx264",
            "-preset", "fast",
            "-crf", "23",
            "-g", "120",
            "-keyint_min", "120",
            "-sc_threshold", "0",
            "-vsync", "cfr",
            "-r", "60",
            "-pix_fmt", "yuv420p",
            "-movflags", "+faststart",
            "-c:a", "aac",
            "-b:a", "128k",
        ],
        stage,
        fallback_duration,
    };
    run_ffmpeg(app, &job)
}

/// Fast metadata fix for WebM: regenerate PTS for stable duration and seeking.
/// No re-encode: -fflags +genpts -c copy. Replaces the file in place.
pub fn fix_webm_metadata(
    app: &AppHandle,
    webm_path: &Path,
    fallback_duration: Option<f64>,
) -> Result<(), String> {
    if !webm_path.exists() {
        return Err("WebM file not found".into());
    }
    let fixed = webm_path.with_extension("genpts.webm");
    let job = FfmpegJob {
        input_args: &["-fflags", "+genpts"],
        input: webm_path,
        output: &fixed,
        args: &["-c", "copy"],
        stage: Some("finalize"),
        fallback_duration,
    };
    if let Err(e) = run_ffmpeg(app, &job) {
        let _ = std::fs::remove_file(&fixed); // don't leave a half-written copy behind
        return Err(e);
    }
    // Rename over the original rather than deleting it first: a failure here leaves the
    // recording readable at its own path instead of leaving nothing at all.
    std::fs::rename(&fixed, webm_path).map_err(|e| {
        let _ = std::fs::remove_file(&fixed);
        e.to_string()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{header_duration, progress_percent};

    #[test]
    fn test_progress_percent_reads_the_newest_status_line() {
        // ffmpeg rewrites its status line, so acc holds every one printed so far.
        let acc = "frame=1 time=00:00:10.00 bitrate=1\rframe=2 time=00:00:30.00 bitrate=1\r";
        assert_eq!(progress_percent(acc, 60.0), Some(50));
    }

    #[test]
    fn test_progress_percent_without_a_status_line_is_none() {
        assert_eq!(progress_percent("Input #0, matroska,webm\n", 60.0), None);
    }

    #[test]
    fn test_progress_percent_caps_at_99_when_the_estimate_runs_short() {
        // The caller's elapsed-seconds estimate can undershoot the real stream, so
        // time= passes total. 100 belongs to a successful exit, not to a guess.
        assert_eq!(progress_percent("time=00:01:30.00\r", 60.0), Some(99));
    }

    #[test]
    fn test_progress_percent_parses_fractional_seconds_by_digit_count() {
        // ffmpeg prints centiseconds; the parser divides by 10^digits, so a two-digit
        // fraction must read as .50 rather than 50.
        assert_eq!(progress_percent("time=00:00:00.50\r", 1.0), Some(50));
    }

    #[test]
    fn test_progress_percent_counts_hours_and_minutes() {
        assert_eq!(progress_percent("time=01:30:00.00\r", 10800.0), Some(50));
    }

    #[test]
    fn test_header_duration_reads_the_input_header() {
        let acc = "  Duration: 00:01:40.00, start: 0.000000, bitrate: 385 kb/s\n";
        assert_eq!(header_duration(acc), Some(100.0));
    }

    #[test]
    fn test_header_duration_absent_for_a_live_webm() {
        // MediaRecorder writes an unknown-size Segment: this is the case the
        // caller-supplied fallback exists for.
        let acc = "  Duration: N/A, start: -0.007000, bitrate: N/A\n";
        assert_eq!(header_duration(acc), None);
    }
}
