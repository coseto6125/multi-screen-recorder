// FFmpeg encoder - converts WebM recordings to MP4 optimized for smooth seeking,
// and fixes WebM metadata (regenerate PTS) without re-encoding.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager};

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

fn captures_to_secs(caps: &regex::Captures) -> f64 {
    let h: f64 = caps[1].parse().unwrap_or(0.0);
    let m: f64 = caps[2].parse().unwrap_or(0.0);
    let s: f64 = caps[3].parse().unwrap_or(0.0);
    let frac_str = &caps[4];
    let frac: f64 = frac_str.parse::<f64>().unwrap_or(0.0) / 10f64.powi(frac_str.len() as i32);
    h * 3600.0 + m * 60.0 + s + frac
}

/// Run ffmpeg `[input_args] -i input [args] -y output`. FFmpeg applies an option to the
/// next file on the command line, so demuxer flags belong in `input_args`, before `-i`.
/// `stage` names the progress events to emit; `None` runs silently.
fn run_ffmpeg(
    app: &AppHandle,
    input_args: &[&str],
    input: &Path,
    output: &Path,
    args: &[&str],
    stage: Option<&str>,
) -> Result<(), String> {
    let ffmpeg = ffmpeg_path(app);
    let mut cmd = Command::new(&ffmpeg);
    for a in input_args {
        cmd.arg(a);
    }
    cmd.arg("-i").arg(input);
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("-y").arg(output);
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

    let file_name = input
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let dur_re = Regex::new(r"Duration:\s*(\d{2}):(\d{2}):(\d{2})\.(\d{1,3})").unwrap();
    let time_re = Regex::new(r"time=(\d{2}):(\d{2}):(\d{2})\.(\d{1,3})").unwrap();

    let emit = |percent: u32| {
        if let Some(stage) = stage {
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
            if let Some(caps) = dur_re.captures(&acc) {
                duration = Some(captures_to_secs(&caps));
            }
        }
        if stage.is_some() {
            if let Some(d) = duration {
                if d > 0.0 {
                    if let Some(caps) = time_re.captures_iter(&acc).last() {
                        let current = captures_to_secs(&caps);
                        let pct = ((current / d) * 100.0).round().min(99.0) as i64;
                        if pct != last_pct {
                            last_pct = pct;
                            emit(pct as u32);
                        }
                    }
                }
            }
        }
        // Keep a bounded tail. FFmpeg prints a progress line per frame, so holding the
        // whole log and rescanning it on every read is quadratic over a long recording.
        // Trim only once the duration header has been read, and never below a full line.
        let cap = if duration.is_some() { 8192 } else { 262_144 };
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
) -> Result<(), String> {
    let stage = if emit_progress { Some("convert") } else { None };
    // MediaRecorder output carries no duration; fill in the missing PTS while decoding
    let input_args = ["-fflags", "+genpts"];
    let args = [
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
    ];
    run_ffmpeg(app, &input_args, input, output, &args, stage)
}

/// Fast metadata fix for WebM: regenerate PTS for stable duration and seeking.
/// No re-encode: -fflags +genpts -c copy. Replaces the file in place.
pub fn fix_webm_metadata(app: &AppHandle, webm_path: &Path) -> Result<(), String> {
    if !webm_path.exists() {
        return Err("WebM file not found".into());
    }
    let fixed = webm_path.with_extension("genpts.webm");
    if let Err(e) = run_ffmpeg(
        app,
        &["-fflags", "+genpts"],
        webm_path,
        &fixed,
        &["-c", "copy"],
        Some("finalize"),
    ) {
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
