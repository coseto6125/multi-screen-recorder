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
    /// One take's slot: the backend side of the recording-open fact
    take: Mutex<TakeSlot>,
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

/// One take's slot on the backend side. The webview owns the phase transitions;
/// this slot owns whether a backend file is open, and what a close request means
/// while a start is still in flight. Every pairing rule between commands lives
/// here so the commands stay thin adapters over it.
struct TakeSlot {
    sink: Option<RecordingSink>,
    /// True between the start of start_recording and register(): no file is
    /// open yet, but one is about to be.
    starting: bool,
    /// A close was requested during that window; register() must salvage the
    /// fresh file instead of opening a take nobody will ever write to.
    close_pending: bool,
}

enum AbortDecision {
    /// A file is open: salvage it.
    Salvage(RecordingSink),
    /// A start is in flight and no file exists yet: remember the close request.
    PendingStart,
    /// Idle: nothing to do, and the next take must not be poisoned.
    Nothing,
}

impl TakeSlot {
    fn new() -> Self {
        Self { sink: None, starting: false, close_pending: false }
    }

    /// Begins a start. Refused while another file is open or another start is
    /// already in flight.
    fn begin(&mut self) -> Result<(), String> {
        if self.sink.is_some() || self.starting {
            return Err("The previous recording is still being finalized".into());
        }
        self.starting = true;
        Ok(())
    }

    /// The file could not be created: reset the slot so a later take starts clean.
    fn cancel(&mut self) {
        self.starting = false;
        self.close_pending = false;
    }

    /// Registers a freshly created sink. Err(sink) means a close was requested
    /// while this start was in flight: salvage it immediately, never store it.
    fn register(&mut self, sink: RecordingSink) -> Result<(), RecordingSink> {
        self.starting = false;
        if self.close_pending {
            self.close_pending = false;
            return Err(sink);
        }
        self.sink = Some(sink);
        Ok(())
    }

    fn abort(&mut self) -> AbortDecision {
        if let Some(sink) = self.sink.take() {
            AbortDecision::Salvage(sink)
        } else if self.starting {
            self.close_pending = true;
            AbortDecision::PendingStart
        } else {
            AbortDecision::Nothing
        }
    }

    fn current(&mut self) -> Result<&mut RecordingSink, String> {
        self.sink.as_mut().ok_or_else(|| "No recording is in progress".to_string())
    }

    fn finish(&mut self) -> Result<RecordingSink, String> {
        self.sink.take().ok_or_else(|| "No recording is in progress".to_string())
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

    // Held across the create so two takes can never share a slot
    let mut guard = state.take.lock().unwrap();
    guard.begin()?;
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
    let sink = match RecordingSink::create(&dir, &format!("recording-{stamp}")) {
        Ok(sink) => sink,
        Err(e) => {
            guard.cancel();
            return Err(format!("Cannot create recording file: {e}"));
        }
    };
    let path = sink.final_path.to_string_lossy().to_string();
    if let Err(sink) = guard.register(sink) {
        // A close was requested while this start was in flight: salvage the
        // fresh file now so no orphan part is left behind
        sink.salvage();
        return Err("Recording was closed before it could start".into());
    }
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
    state
        .take
        .lock()
        .unwrap()
        .current()?
        .append(data)
        .map_err(|e| format!("Cannot write recording: {e}"))
}

// Close the file and return the final path. `remux` runs the metadata fix; skip it when
// the caller converts to MP4 next, because that pass rewrites the timestamps anyway.
#[tauri::command]
async fn finish_recording(app: AppHandle, remux: bool) -> Result<String, String> {
    let sink = {
        let state: State<'_, AppState> = app.state();
        let mut slot = state.take.lock().unwrap();
        slot.finish()?
    };

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
    match state.take.lock().unwrap().abort() {
        AbortDecision::Salvage(sink) => {
            let (path, bytes) = sink.salvage();
            (bytes > 0).then(|| path.to_string_lossy().to_string())
        }
        // A start still in flight salvages its own fresh file on register()
        _ => None,
    }
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
                take: Mutex::new(TakeSlot::new()),
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
    use super::{AbortDecision, RecordingSink, TakeSlot};
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
        // Windows refuses to delete files that are still open, so close both sinks first
        drop(first);
        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_in_a_missing_directory_returns_an_error() {
        let parent = temp_dir("missing");
        assert!(RecordingSink::create(&parent.join("not-created"), "rec").is_err());
        fs::remove_dir_all(&parent).unwrap();
    }

    fn open_sink(dir: &std::path::Path) -> RecordingSink {
        RecordingSink::create(dir, "rec").unwrap()
    }

    #[test]
    fn test_abort_with_open_sink_salvages_the_file() {
        let dir = temp_dir("slot-abort-open");
        let mut slot = TakeSlot::new();
        slot.begin().unwrap();
        assert!(slot.register(open_sink(&dir)).is_ok());
        slot.current().unwrap().append(b"keep").unwrap();

        match slot.abort() {
            AbortDecision::Salvage(sink) => {
                let (partial, bytes) = sink.salvage();
                assert_eq!(bytes, 4);
                assert!(partial.to_string_lossy().contains("rec.partial.webm"));
            }
            _ => panic!("open sink must salvage"),
        }
        assert!(slot.finish().is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_close_during_start_salvages_the_fresh_file_on_register() {
        let dir = temp_dir("slot-close-during-start");
        let mut slot = TakeSlot::new();
        slot.begin().unwrap();
        // The close request arrives while the start is still in flight
        assert!(matches!(slot.abort(), AbortDecision::PendingStart));

        let sink = open_sink(&dir);
        let part = sink.part_path.clone();
        assert!(part.exists());
        let Err(sink) = slot.register(sink) else {
            panic!("a pending close must reject the fresh sink");
        };
        // Salvaging the zero-byte fresh file removes it: no orphan part is left
        let (gone, bytes) = sink.salvage();
        assert_eq!(bytes, 0);
        assert!(!gone.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_abort_when_idle_does_not_poison_the_next_take() {
        let dir = temp_dir("slot-abort-idle");
        let mut slot = TakeSlot::new();
        assert!(matches!(slot.abort(), AbortDecision::Nothing));

        slot.begin().unwrap();
        assert!(slot.register(open_sink(&dir)).is_ok());
        assert!(slot.finish().is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_pending_close_is_consumed_by_register() {
        let dir = temp_dir("slot-pending-once");
        let mut slot = TakeSlot::new();
        slot.begin().unwrap();
        assert!(matches!(slot.abort(), AbortDecision::PendingStart));
        assert!(slot.register(open_sink(&dir)).is_err());

        // The pending close applied to that start only
        slot.begin().unwrap();
        assert!(slot.register(open_sink(&dir)).is_ok());
        drop(slot.finish().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_second_begin_while_open_or_starting_is_refused() {
        let dir = temp_dir("slot-double-begin");
        let mut slot = TakeSlot::new();
        slot.begin().unwrap();
        assert!(slot.register(open_sink(&dir)).is_ok());
        assert!(slot.begin().is_err());
        drop(slot.finish().unwrap());
        fs::remove_dir_all(&dir).unwrap();

        let mut slot2 = TakeSlot::new();
        slot2.begin().unwrap();
        assert!(slot2.begin().is_err());
        slot2.cancel();
        assert!(slot2.begin().is_ok());
    }

    #[test]
    fn test_cancel_after_failed_create_resets_the_slot() {
        let mut slot = TakeSlot::new();
        slot.begin().unwrap();
        slot.cancel();
        // A close request must not survive into the next take either
        slot.begin().unwrap();
        slot.cancel();
        let dir = temp_dir("slot-cancel-clean");
        slot.begin().unwrap();
        assert!(slot.register(open_sink(&dir)).is_ok());
        drop(slot.finish().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_append_and_finish_without_a_sink_error() {
        let mut slot = TakeSlot::new();
        assert!(slot.current().is_err());
        assert!(slot.finish().is_err());
    }
}
