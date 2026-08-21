// Multi Screen Recorder - Tauri main process
// Manages settings, filesystem, ffmpeg conversion

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encoder;

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

struct AppState {
    recordings_dir: Mutex<PathBuf>,
    /// Open recording file, present only between start_recording and finish/abort
    sink: Mutex<Option<RecordingSink>>,
}

/// A recording file kept open for the whole session so MediaRecorder chunks can be
/// appended as they arrive. Memory stays flat regardless of how long the recording runs.
///
/// Bytes land in `<name>.webm.part`. Only a clean finish renames it to `<name>.webm`,
/// so a file left behind by a crash is recognisable as incomplete instead of looking
/// like a finished recording that will not seek.
struct RecordingSink {
    file: File,
    part_path: PathBuf,
    final_path: PathBuf,
    partial_path: PathBuf,
    bytes: u64,
    /// A rollback failed, so the tail past `bytes` is a torn element
    torn: bool,
}

impl RecordingSink {
    /// Creates `<stem>.webm.part`, never overwriting an existing recording. All three
    /// names this take can end up under are claimed together, so an aborted take can
    /// never land on the partial file of an earlier one with the same timestamp.
    fn create(dir: &Path, stem: &str) -> std::io::Result<Self> {
        for attempt in 1..=100u32 {
            let stem = if attempt == 1 {
                stem.to_string()
            } else {
                format!("{stem}-{attempt}")
            };
            let final_path = dir.join(format!("{stem}.webm"));
            let partial_path = dir.join(format!("{stem}.partial.webm"));
            if final_path.exists() || partial_path.exists() {
                continue;
            }
            let part_path = dir.join(format!("{stem}.webm.part"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part_path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        part_path,
                        final_path,
                        partial_path,
                        bytes: 0,
                        torn: false,
                    })
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "too many recordings with the same timestamp",
        ))
    }

    /// Appends one chunk. A failed or short write is rolled back, so the bytes on disk
    /// always end on a chunk boundary rather than half-way through a WebM element.
    fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self.file.write_all(data) {
            Ok(()) => {
                self.bytes += data.len() as u64;
                Ok(())
            }
            Err(e) => {
                // If the rollback itself fails the tail is torn, and the file must not
                // go out under a name that says it is playable.
                if self.file.set_len(self.bytes).is_err()
                    || self.file.seek(SeekFrom::Start(self.bytes)).is_err()
                {
                    self.torn = true;
                }
                Err(e)
            }
        }
    }

    /// Flushes, closes, and renames the part file to the completed recording name.
    /// Every error names a file the user can still open: the bytes are never abandoned.
    fn commit(self) -> Result<(PathBuf, u64), String> {
        let (part_path, final_path, _, bytes, synced) = self.close();
        if let Err(e) = synced {
            // The bytes reached the file; give them their name so they stay findable
            let _ = fs::rename(&part_path, &final_path);
            return Err(format!(
                "Cannot flush recording: {e}. Saved as {}",
                final_path.display()
            ));
        }
        if bytes == 0 {
            let _ = fs::remove_file(&part_path);
            return Ok((final_path, 0));
        }
        fs::rename(&part_path, &final_path).map_err(|e| {
            format!(
                "Cannot rename recording: {e}. The data is at {}",
                part_path.display()
            )
        })?;
        Ok((final_path, bytes))
    }

    /// Flushes, closes, and renames the part file to a name that marks it incomplete.
    /// A torn tail keeps the `.part` name, which does not claim to be playable.
    fn salvage(self) -> (PathBuf, u64) {
        let torn = self.torn;
        let (part_path, _, partial_path, bytes, _) = self.close();
        if bytes == 0 {
            let _ = fs::remove_file(&part_path);
            return (part_path, 0);
        }
        if torn || fs::rename(&part_path, &partial_path).is_err() {
            return (part_path, bytes);
        }
        (partial_path, bytes)
    }

    /// Closes the file and hands back its names, the byte count, and how the flush went.
    /// A failed flush is reported rather than thrown away: the bytes are still on disk.
    fn close(self) -> (PathBuf, PathBuf, PathBuf, u64, std::io::Result<()>) {
        let Self {
            file,
            part_path,
            final_path,
            partial_path,
            bytes,
            torn: _,
        } = self;
        let synced = file.sync_all();
        drop(file);
        (part_path, final_path, partial_path, bytes, synced)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    #[serde(rename = "recordingsDir")]
    recordings_dir: Option<String>,
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

fn default_recordings_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("recordings")
}

fn load_recordings_dir(app: &AppHandle) -> PathBuf {
    if let Some(path) = settings_path(app) {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(settings) = serde_json::from_str::<Settings>(&raw) {
                if let Some(dir) = settings.recordings_dir {
                    let dir = PathBuf::from(dir);
                    if dir.exists() {
                        return dir;
                    }
                }
            }
        }
    }
    default_recordings_dir(app)
}

fn save_settings(app: &AppHandle, recordings_dir: &PathBuf) {
    if let Some(path) = settings_path(app) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let settings = Settings {
            recordings_dir: Some(recordings_dir.to_string_lossy().to_string()),
        };
        if let Ok(json) = serde_json::to_string_pretty(&settings) {
            let _ = fs::write(path, json);
        }
    }
}

fn ensure_dir(dir: &PathBuf) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("Cannot create recordings folder: {e}"))
}

#[tauri::command]
fn get_recordings_path(state: State<'_, AppState>) -> String {
    state
        .recordings_dir
        .lock()
        .unwrap()
        .to_string_lossy()
        .to_string()
}

#[tauri::command]
fn open_recordings_folder(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let dir = state.recordings_dir.lock().unwrap().clone();
    ensure_dir(&dir)?;
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct PickResult {
    canceled: bool,
    path: Option<String>,
}

#[tauri::command]
async fn change_recordings_path(app: AppHandle) -> Result<PickResult, String> {
    let dialog = app.dialog().clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialog
            .file()
            .set_title("Select folder to save recordings")
            .blocking_pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;

    match picked {
        Some(folder) => {
            let path = folder.into_path().map_err(|e| e.to_string())?;
            ensure_dir(&path)?;
            {
                let state: State<'_, AppState> = app.state();
                *state.recordings_dir.lock().unwrap() = path.clone();
            }
            save_settings(&app, &path);
            Ok(PickResult {
                canceled: false,
                path: Some(path.to_string_lossy().to_string()),
            })
        }
        None => Ok(PickResult {
            canceled: true,
            path: None,
        }),
    }
}

// Open the output file before recording starts, so a disk error surfaces immediately
// instead of after the user has already recorded for an hour.
#[tauri::command(async)]
fn start_recording(state: State<'_, AppState>) -> Result<String, String> {
    let dir = state.recordings_dir.lock().unwrap().clone();
    ensure_dir(&dir)?;

    // Held across the create so two takes can never share a sink
    let mut guard = state.sink.lock().unwrap();
    if guard.is_some() {
        return Err("The previous recording is still being finalized".into());
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
    let sink = RecordingSink::create(&dir, &format!("recording-{stamp}"))
        .map_err(|e| format!("Cannot create recording file: {e}"))?;
    let path = sink.final_path.to_string_lossy().to_string();
    *guard = Some(sink);
    Ok(path)
}

// Append one MediaRecorder chunk (raw IPC body) to the open file. Writes straight from
// the borrowed IPC buffer, so nothing is copied and nothing accumulates in memory.
#[tauri::command(async)]
fn append_recording_chunk(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(data) = request.body() else {
        return Err("Invalid recording data (expected raw bytes)".into());
    };
    let mut guard = state.sink.lock().unwrap();
    let sink = guard
        .as_mut()
        .ok_or_else(|| "No recording is in progress".to_string())?;
    sink.append(data)
        .map_err(|e| format!("Cannot write recording: {e}"))
}

// Close the file and return the final path. `remux` runs the metadata fix; skip it when
// the caller converts to MP4 next, because that pass rewrites the timestamps anyway.
#[tauri::command]
async fn finish_recording(app: AppHandle, remux: bool) -> Result<String, String> {
    let sink = {
        let state: State<'_, AppState> = app.state();
        let sink = state.sink.lock().unwrap().take();
        sink
    };
    let sink = sink.ok_or_else(|| "No recording is in progress".to_string())?;

    tauri::async_runtime::spawn_blocking(move || {
        let (path, bytes) = sink.commit()?;
        if bytes == 0 {
            return Err("Recording is empty (no data was captured)".into());
        }
        if remux {
            // Regenerate PTS for stable duration and seeking; keep the file on failure
            let _ = encoder::fix_webm_metadata(&app, &path);
        }
        Ok(path.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// Close the file after a failed or interrupted take. The bytes already written stay on
// disk under a `.partial.webm` name, so a failure costs the tail, not the whole session.
#[tauri::command(async)]
fn abort_recording(state: State<'_, AppState>) -> Option<String> {
    let sink = state.sink.lock().unwrap().take()?;
    let (path, bytes) = sink.salvage();
    if bytes == 0 {
        return None;
    }
    Some(path.to_string_lossy().to_string())
}

// Convert WebM to MP4; emits convert-progress {fileName, percent}; deletes source on success
#[tauri::command]
async fn convert_to_mp4(app: AppHandle, webm_path: String) -> Result<String, String> {
    let input = PathBuf::from(&webm_path);
    if !input.exists() {
        return Err("WebM file not found".into());
    }
    let output = input.with_extension("mp4");
    let app2 = app.clone();
    let in2 = input.clone();
    let out2 = output.clone();
    tauri::async_runtime::spawn_blocking(move || encoder::convert_to_mp4(&app2, &in2, &out2, true))
        .await
        .map_err(|e| e.to_string())??;
    let _ = fs::remove_file(&input);
    Ok(output.to_string_lossy().to_string())
}

#[derive(Serialize)]
struct ConvertFileResult {
    canceled: bool,
    path: Option<String>,
    error: Option<String>,
}

// Pick a WebM/video file and convert to MP4 (source kept)
#[tauri::command]
async fn convert_file_to_mp4(app: AppHandle) -> Result<ConvertFileResult, String> {
    let start_dir = {
        let state: State<'_, AppState> = app.state();
        let dir = state.recordings_dir.lock().unwrap().clone();
        dir
    };
    let dialog = app.dialog().clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialog
            .file()
            .set_title("Select WebM file to convert to MP4")
            .set_directory(start_dir)
            .add_filter("WebM / Video", &["webm", "mkv", "avi"])
            .blocking_pick_file()
    })
    .await
    .map_err(|e| e.to_string())?;

    let Some(file) = picked else {
        return Ok(ConvertFileResult {
            canceled: true,
            path: None,
            error: None,
        });
    };
    let input = file.into_path().map_err(|e| e.to_string())?;
    if !input.exists() {
        return Ok(ConvertFileResult {
            canceled: false,
            path: None,
            error: Some("File not found".into()),
        });
    }
    let output = input.with_extension("mp4");
    let app2 = app.clone();
    let in2 = input.clone();
    let out2 = output.clone();
    let result =
        tauri::async_runtime::spawn_blocking(move || encoder::convert_to_mp4(&app2, &in2, &out2, true))
            .await
            .map_err(|e| e.to_string())?;

    match result {
        Ok(()) => Ok(ConvertFileResult {
            canceled: false,
            path: Some(output.to_string_lossy().to_string()),
            error: None,
        }),
        Err(e) => Ok(ConvertFileResult {
            canceled: false,
            path: None,
            error: Some(e),
        }),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let dir = load_recordings_dir(&handle);
            let _ = ensure_dir(&dir);
            app.manage(AppState {
                recordings_dir: Mutex::new(dir),
                sink: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_recordings_path,
            open_recordings_folder,
            change_recordings_path,
            start_recording,
            append_recording_chunk,
            finish_recording,
            abort_recording,
            convert_to_mp4,
            convert_file_to_mp4
        ])
        .run(tauri::generate_context!())
        .expect("error while running Multi Screen Recorder");
}

#[cfg(test)]
mod tests {
    use super::RecordingSink;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("msr-test-{tag}-{n}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_append_multiple_chunks_writes_them_concatenated_in_order() {
        let dir = temp_dir("order");
        let mut sink = RecordingSink::create(&dir, "rec").unwrap();
        for chunk in [b"header".as_slice(), b"cluster-1", b"cluster-2"] {
            sink.append(chunk).unwrap();
        }
        let (path, bytes) = sink.commit().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"headercluster-1cluster-2");
        assert_eq!(bytes, 24);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_append_empty_chunk_leaves_byte_count_unchanged() {
        let dir = temp_dir("empty-chunk");
        let mut sink = RecordingSink::create(&dir, "rec").unwrap();
        sink.append(b"data").unwrap();
        sink.append(b"").unwrap();
        let (_, bytes) = sink.commit().unwrap();

        assert_eq!(bytes, 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_chunks_stay_in_a_part_file_until_commit() {
        let dir = temp_dir("part");
        let mut sink = RecordingSink::create(&dir, "rec").unwrap();
        sink.append(b"data").unwrap();

        assert!(dir.join("rec.webm.part").exists());
        assert!(!dir.join("rec.webm").exists());

        let (path, _) = sink.commit().unwrap();
        assert_eq!(path, dir.join("rec.webm"));
        assert!(!dir.join("rec.webm.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_salvage_renames_the_part_file_to_a_partial_name() {
        let dir = temp_dir("salvage");
        let mut sink = RecordingSink::create(&dir, "rec").unwrap();
        sink.append(b"half a recording").unwrap();
        let (path, bytes) = sink.salvage();

        assert_eq!(path, dir.join("rec.partial.webm"));
        assert_eq!(bytes, 16);
        assert_eq!(fs::read(&path).unwrap(), b"half a recording");
        assert!(!dir.join("rec.webm.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_commit_without_any_append_removes_the_empty_part_file() {
        let dir = temp_dir("zero");
        let sink = RecordingSink::create(&dir, "rec").unwrap();
        let (path, bytes) = sink.commit().unwrap();

        assert_eq!(bytes, 0);
        assert!(!path.exists());
        assert!(!dir.join("rec.webm.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_beside_a_finished_recording_picks_the_next_name() {
        let dir = temp_dir("collide-final");
        fs::write(dir.join("rec.webm"), b"an earlier take").unwrap();
        let sink = RecordingSink::create(&dir, "rec").unwrap();
        let (path, _) = sink.commit().unwrap();

        assert_eq!(path, dir.join("rec-2.webm"));
        assert_eq!(fs::read(dir.join("rec.webm")).unwrap(), b"an earlier take");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_salvage_with_a_torn_tail_keeps_the_part_name() {
        let dir = temp_dir("torn");
        let mut sink = RecordingSink::create(&dir, "rec").unwrap();
        sink.append(b"a chunk").unwrap();
        sink.torn = true; // a rollback failed, so the tail past `bytes` is incomplete
        let (path, bytes) = sink.salvage();

        assert_eq!(path, dir.join("rec.webm.part"));
        assert_eq!(bytes, 7);
        assert!(!dir.join("rec.partial.webm").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_beside_a_salvaged_partial_picks_the_next_name() {
        let dir = temp_dir("collide-partial");
        fs::write(dir.join("rec.partial.webm"), b"an aborted take").unwrap();
        let sink = RecordingSink::create(&dir, "rec").unwrap();
        let (path, _) = sink.salvage();

        assert_eq!(path, dir.join("rec-2.webm.part"));
        assert_eq!(fs::read(dir.join("rec.partial.webm")).unwrap(), b"an aborted take");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_beside_an_open_part_file_picks_the_next_name() {
        let dir = temp_dir("collide-part");
        let first = RecordingSink::create(&dir, "rec").unwrap();
        let second = RecordingSink::create(&dir, "rec").unwrap();

        assert_eq!(first.part_path, dir.join("rec.webm.part"));
        assert_eq!(second.part_path, dir.join("rec-2.webm.part"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_in_a_missing_directory_returns_an_error() {
        let parent = temp_dir("missing");
        assert!(RecordingSink::create(&parent.join("not-created"), "rec").is_err());
        fs::remove_dir_all(&parent).unwrap();
    }
}
