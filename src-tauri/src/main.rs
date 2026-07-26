// Multi Screen Recorder - Tauri main process
// Manages settings, filesystem, ffmpeg conversion

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encoder;

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

struct AppState {
    recordings_dir: Mutex<PathBuf>,
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

// Save recorded WebM bytes (raw IPC body), run fast metadata fix, return final path
#[tauri::command]
fn save_webm(
    app: AppHandle,
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
    let bytes: Vec<u8> = match request.body() {
        tauri::ipc::InvokeBody::Raw(data) => data.clone(),
        _ => return Err("Invalid recording data (expected raw bytes)".into()),
    };
    if bytes.is_empty() {
        return Err("Invalid recording data (empty)".into());
    }

    let dir = state.recordings_dir.lock().unwrap().clone();
    ensure_dir(&dir)?;

    let stamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
    let webm_path = dir.join(format!("recording-{stamp}.webm"));
    fs::write(&webm_path, &bytes).map_err(|e| format!("Cannot save recording: {e}"))?;

    // Fast metadata fix (regenerate PTS, no re-encode); keep original file on failure
    let _ = encoder::fix_webm_metadata(&app, &webm_path);

    Ok(webm_path.to_string_lossy().to_string())
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
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_recordings_path,
            open_recordings_folder,
            change_recordings_path,
            save_webm,
            convert_to_mp4,
            convert_file_to_mp4
        ])
        .run(tauri::generate_context!())
        .expect("error while running Multi Screen Recorder");
}
