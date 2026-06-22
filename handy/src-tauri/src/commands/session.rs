//! Tauri command surface for long-form recording sessions (Plaude Local, Fase 0).
//!
//! This is the product API the future session UI will drive. The headless demo
//! uses the `--toggle-session` CLI flag, which calls the same `SessionManager`.

use crate::managers::session::{SessionManager, Source};
use std::sync::Arc;
use tauri::{AppHandle, Manager};

#[tauri::command]
#[specta::specta]
pub fn start_session(app: AppHandle) -> Result<(), String> {
    app.state::<Arc<SessionManager>>()
        .start(Source::Mic)
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
