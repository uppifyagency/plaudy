//! Tauri command surface for long-form recording sessions (Plaude Local, Fase 0).
//!
//! This is the product API the future session UI will drive. The headless demo
//! uses the `--toggle-session` CLI flag, which calls the same `SessionManager`.

use crate::managers::session::{SessionManager, Source};
use std::sync::Arc;
use tauri::{AppHandle, Manager};

#[tauri::command]
#[specta::specta]
pub fn start_session(app: AppHandle, source: Source) -> Result<(), String> {
    app.state::<Arc<SessionManager>>()
        .start(source)
        .map_err(|e| e.to_string())
}

/// Start a meeting capture: mic + system audio as two streams that finalize into one
/// speaker-attributed transcript. System audio is best-effort, so this still works (mic-only)
/// when nothing is playing — the seamless one-click capture, also reachable from the tray.
#[tauri::command]
#[specta::specta]
pub fn start_meeting(app: AppHandle) -> Result<(), String> {
    app.state::<Arc<SessionManager>>()
        .start_sources(&[Source::Mic, Source::SystemAudio])
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn stop_session(app: AppHandle) -> Result<(), String> {
    app.state::<Arc<SessionManager>>()
        .stop()
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn is_session_active(app: AppHandle) -> bool {
    app.state::<Arc<SessionManager>>().is_active()
}
