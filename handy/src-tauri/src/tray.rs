use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::model::ModelManager;
use crate::managers::session::SessionManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings;
use crate::tray_i18n::get_tray_translations;
use log::{error, info, warn};
use std::sync::Arc;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon};
use tauri::{AppHandle, Manager, Theme};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Clone, Debug, PartialEq)]
pub enum TrayIconState {
    Idle,
    Recording,
    Transcribing,
    /// A long-form session is capturing (mic / system / meeting). The menu-bar mark becomes an
    /// ear so you can tell, at a glance, that it's listening to you. Dictation keeps `Recording`.
    Listening,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppTheme {
    Dark,
    Light,
    Colored, // Pink/colored theme for Linux
}

/// Gets the current app theme, with Linux defaulting to Colored theme
pub fn get_current_theme(app: &AppHandle) -> AppTheme {
    if cfg!(target_os = "linux") {
        // On Linux, always use the colored theme
        AppTheme::Colored
    } else {
        // On other platforms, map system theme to our app theme
        if let Some(main_window) = app.get_webview_window("main") {
            match main_window.theme().unwrap_or(Theme::Dark) {
                Theme::Light => AppTheme::Light,
                Theme::Dark => AppTheme::Dark,
                _ => AppTheme::Dark, // Default fallback
            }
        } else {
            AppTheme::Dark
        }
    }
}

/// Gets the appropriate icon path for the given theme and state
pub fn get_icon_path(theme: AppTheme, state: TrayIconState) -> &'static str {
    match (theme, state) {
        // Dark theme uses light icons
        (AppTheme::Dark, TrayIconState::Idle) => "resources/tray_idle.png",
        (AppTheme::Dark, TrayIconState::Recording) => "resources/tray_recording.png",
        (AppTheme::Dark, TrayIconState::Transcribing) => "resources/tray_transcribing.png",
        // Light theme uses dark icons
        (AppTheme::Light, TrayIconState::Idle) => "resources/tray_idle_dark.png",
        (AppTheme::Light, TrayIconState::Recording) => "resources/tray_recording_dark.png",
        (AppTheme::Light, TrayIconState::Transcribing) => "resources/tray_transcribing_dark.png",
        // Colored theme (Linux) — the ear is the brand mark; idle reuses the dimmed variant.
        (AppTheme::Colored, TrayIconState::Idle) => "resources/tray_idle.png",
        (AppTheme::Colored, TrayIconState::Recording) => "resources/recording.png",
        (AppTheme::Colored, TrayIconState::Transcribing) => "resources/transcribing.png",
        // Listening (a session is capturing) — the ear glyph. macOS renders the tray as a template
        // (alpha mask, auto light/dark), so one asset serves every theme.
        (AppTheme::Dark, TrayIconState::Listening) => "resources/tray_listening.png",
        (AppTheme::Light, TrayIconState::Listening) => "resources/tray_listening.png",
        (AppTheme::Colored, TrayIconState::Listening) => "resources/tray_listening.png",
    }
}

/// Derive the icon actually shown from what a caller *requested* and the live session state.
/// Callers only know their own flow (dictation says Idle when a dictation finishes) — but the
/// ear must never disappear while a long-form session is still capturing: that icon is the
/// product's honest "I'm listening" signal, so it is derived here, never trusted from a caller.
fn resolve_tray_state(requested: TrayIconState, session_active: bool) -> TrayIconState {
    match (requested, session_active) {
        (TrayIconState::Idle, true) => TrayIconState::Listening,
        (other, _) => other,
    }
}

/// The "graffetta" gesture: a plain left-click on the menu-bar icon toggles the meeting
/// session directly — the menu stays on right-click. Fires on release (Up) so a press that
/// turns into a drag never toggles. Linux tray backends don't deliver reliable click events,
/// so there the menu remains the entry point and this never fires.
pub fn is_session_toggle_click(button: &MouseButton, state: &MouseButtonState) -> bool {
    !cfg!(target_os = "linux")
        && matches!(button, MouseButton::Left)
        && matches!(state, MouseButtonState::Up)
}

fn session_is_active(app: &AppHandle) -> bool {
    app.try_state::<Arc<SessionManager>>()
        .map(|sm| sm.is_active())
        .unwrap_or(false)
}

pub fn change_tray_icon(app: &AppHandle, requested: TrayIconState) {
    let session_active = session_is_active(app);
    let state = resolve_tray_state(requested, session_active);

    let tray = app.state::<TrayIcon>();
    let theme = get_current_theme(app);
    let icon_path = get_icon_path(theme, state.clone());

    // A missing/corrupt resource degrades to keeping the previous icon — never a panic
    // mid-session over a PNG.
    match app
        .path()
        .resolve(icon_path, tauri::path::BaseDirectory::Resource)
    {
        Ok(path) => match Image::from_path(&path) {
            Ok(img) => {
                let _ = tray.set_icon(Some(img));
            }
            Err(e) => error!("Failed to load tray icon {}: {e}", path.display()),
        },
        Err(e) => error!("Failed to resolve tray icon {icon_path}: {e}"),
    }

    // Rebuild the menu from the SAME session snapshot the icon used, so the two can't
    // disagree within one update (icon Idle + label "Stop recording").
    update_tray_menu_with(app, &state, None, session_active);
}

pub fn tray_tooltip() -> String {
    version_label()
}

fn version_label() -> String {
    if cfg!(debug_assertions) {
        format!("Plaudy v{} (Dev)", env!("CARGO_PKG_VERSION"))
    } else {
        format!("Plaudy v{}", env!("CARGO_PKG_VERSION"))
    }
}

pub fn update_tray_menu(app: &AppHandle, state: &TrayIconState, locale: Option<&str>) {
    let session_active = session_is_active(app);
    let state = resolve_tray_state(state.clone(), session_active);
    update_tray_menu_with(app, &state, locale, session_active);
}

fn update_tray_menu_with(
    app: &AppHandle,
    state: &TrayIconState,
    locale: Option<&str>,
    session_active: bool,
) {
    // Menu construction is fallible (i18n, model list, OS menu APIs) but never worth a
    // panic: on error keep the previous menu and log.
    if let Err(e) = build_and_set_tray_menu(app, state, locale, session_active) {
        error!("Failed to rebuild tray menu; keeping the previous one: {e}");
    }
}

fn build_and_set_tray_menu(
    app: &AppHandle,
    state: &TrayIconState,
    locale: Option<&str>,
    session_active: bool,
) -> tauri::Result<()> {
    let settings = settings::get_settings(app);

    let locale = locale.unwrap_or(&settings.app_language);
    let strings = get_tray_translations(Some(locale.to_string()));

    // Platform-specific accelerators
    #[cfg(target_os = "macos")]
    let (settings_accelerator, quit_accelerator) = (Some("Cmd+,"), Some("Cmd+Q"));
    #[cfg(not(target_os = "macos"))]
    let (settings_accelerator, quit_accelerator) = (Some("Ctrl+,"), Some("Ctrl+Q"));

    // Create common menu items
    let version_label = version_label();
    let version_i = MenuItem::with_id(app, "version", &version_label, false, None::<&str>)?;
    let settings_i = MenuItem::with_id(
        app,
        "settings",
        &strings.settings,
        true,
        settings_accelerator,
    )?;
    let check_updates_i = MenuItem::with_id(
        app,
        "check_updates",
        &strings.check_updates,
        settings.update_checks_enabled,
        None::<&str>,
    )?;
    let copy_last_transcript_i = MenuItem::with_id(
        app,
        "copy_last_transcript",
        &strings.copy_last_transcript,
        true,
        None::<&str>,
    )?;
    let model_loaded = app.state::<Arc<TranscriptionManager>>().is_model_loaded();
    let quit_i = MenuItem::with_id(app, "quit", &strings.quit, true, quit_accelerator)?;
    // Up to 6 separators per layout; build them fallibly once.
    let seps = (0..6)
        .map(|_| PredefinedMenuItem::separator(app))
        .collect::<Result<Vec<_>, _>>()?;

    // Build model submenu — label is the active model name
    let model_manager = app.state::<Arc<ModelManager>>();
    let models = model_manager.get_available_models();
    let current_model_id = &settings.selected_model;

    let mut downloaded: Vec<_> = models.into_iter().filter(|m| m.is_downloaded).collect();
    downloaded.sort_by(|a, b| a.name.cmp(&b.name));

    let submenu_label = downloaded
        .iter()
        .find(|m| m.id == *current_model_id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| strings.model.clone());

    let model_submenu = {
        let submenu = Submenu::with_id(app, "model_submenu", &submenu_label, true)?;

        for model in &downloaded {
            let is_active = model.id == *current_model_id;
            let item_id = format!("model_select:{}", model.id);
            let item =
                CheckMenuItem::with_id(app, &item_id, &model.name, true, is_active, None::<&str>)?;
            let _ = submenu.append(&item);
        }

        submenu
    };

    let unload_model_i = MenuItem::with_id(
        app,
        "unload_model",
        &strings.unload_model,
        model_loaded,
        None::<&str>,
    )?;

    // Long-form session toggle — the menu-bar "graffetta": one click starts/stops a recording
    // session. The label comes from the SAME session snapshot the caller resolved the icon
    // with, so icon and label can never disagree within one update.
    let session_label = if session_active {
        &strings.stop_recording
    } else {
        &strings.start_recording
    };
    let toggle_session_i =
        MenuItem::with_id(app, "toggle_session", session_label, true, None::<&str>)?;

    let menu = match state {
        TrayIconState::Recording | TrayIconState::Transcribing | TrayIconState::Listening => {
            let cancel_i = MenuItem::with_id(app, "cancel", &strings.cancel, true, None::<&str>)?;
            Menu::with_items(
                app,
                &[
                    &version_i,
                    &seps[0],
                    &toggle_session_i,
                    &seps[1],
                    &cancel_i,
                    &seps[2],
                    &copy_last_transcript_i,
                    &seps[3],
                    &settings_i,
                    &check_updates_i,
                    &seps[4],
                    &quit_i,
                ],
            )?
        }
        TrayIconState::Idle => Menu::with_items(
            app,
            &[
                &version_i,
                &seps[0],
                &toggle_session_i,
                &seps[1],
                &copy_last_transcript_i,
                &seps[2],
                &model_submenu,
                &unload_model_i,
                &seps[3],
                &settings_i,
                &check_updates_i,
                &seps[4],
                &quit_i,
            ],
        )?,
    };

    let tray = app.state::<TrayIcon>();
    let _ = tray.set_menu(Some(menu));
    let _ = tray.set_icon_as_template(true);
    let _ = tray.set_tooltip(Some(version_label));
    Ok(())
}

fn last_transcript_text(entry: &HistoryEntry) -> &str {
    entry
        .post_processed_text
        .as_deref()
        .unwrap_or(&entry.transcription_text)
}

pub fn set_tray_visibility(app: &AppHandle, visible: bool) {
    let tray = app.state::<TrayIcon>();
    if let Err(e) = tray.set_visible(visible) {
        error!("Failed to set tray visibility: {}", e);
    } else {
        info!("Tray visibility set to: {}", visible);
    }
}

pub fn copy_last_transcript(app: &AppHandle) {
    let history_manager = app.state::<Arc<HistoryManager>>();
    let entry = match history_manager.get_latest_completed_entry() {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            warn!("No completed transcription history entries available for tray copy.");
            return;
        }
        Err(err) => {
            error!(
                "Failed to fetch last completed transcription entry: {}",
                err
            );
            return;
        }
    };

    let text = last_transcript_text(&entry);
    if text.trim().is_empty() {
        warn!("Last completed transcription is empty; skipping tray copy.");
        return;
    }

    if let Err(err) = app.clipboard().write_text(text) {
        error!("Failed to copy last transcript to clipboard: {}", err);
        return;
    }

    info!("Copied last transcript to clipboard via tray.");
}

#[cfg(test)]
mod tests {
    use super::last_transcript_text;
    use crate::managers::history::{HistoryEntry, TranscriptionStatus};

    fn build_entry(transcription: &str, post_processed: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            file_name: "handy-1.wav".to_string(),
            timestamp: 0,
            saved: false,
            title: "Recording".to_string(),
            transcription_text: transcription.to_string(),
            post_processed_text: post_processed.map(|text| text.to_string()),
            post_process_prompt: None,
            post_process_requested: false,
            status: TranscriptionStatus::Done,
            source: crate::managers::history::EntrySource::Dictation,
        }
    }

    #[test]
    fn uses_post_processed_text_when_available() {
        let entry = build_entry("raw", Some("processed"));
        assert_eq!(last_transcript_text(&entry), "processed");
    }

    #[test]
    fn falls_back_to_raw_transcription() {
        let entry = build_entry("raw", None);
        assert_eq!(last_transcript_text(&entry), "raw");
    }

    use super::{resolve_tray_state, TrayIconState};

    #[test]
    fn dictation_going_idle_during_a_session_keeps_the_ear() {
        // THE tray-honesty regression: a quick dictation finishing mid-session used to
        // install Idle while the session was still capturing.
        assert_eq!(
            resolve_tray_state(TrayIconState::Idle, true),
            TrayIconState::Listening
        );
    }

    #[test]
    fn idle_stays_idle_when_no_session_is_active() {
        assert_eq!(
            resolve_tray_state(TrayIconState::Idle, false),
            TrayIconState::Idle
        );
    }

    #[test]
    fn listening_maps_to_the_ear_asset_on_every_theme() {
        // The behavioral claim (not the compiler's exhaustiveness): whatever the theme, a
        // recording session shows the ear — the product's honest "I'm listening" mark.
        use super::{get_icon_path, AppTheme};
        for theme in [AppTheme::Dark, AppTheme::Light, AppTheme::Colored] {
            assert_eq!(
                get_icon_path(theme, TrayIconState::Listening),
                "resources/tray_listening.png"
            );
        }
    }

    use super::is_session_toggle_click;
    use tauri::tray::{MouseButton, MouseButtonState};

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn left_click_release_on_the_graffetta_toggles_the_session() {
        assert!(is_session_toggle_click(
            &MouseButton::Left,
            &MouseButtonState::Up
        ));
    }

    #[test]
    fn other_buttons_or_a_press_never_toggle() {
        // Right-click owns the menu; a press (Down) may become a drag — only Left+Up toggles.
        assert!(!is_session_toggle_click(
            &MouseButton::Right,
            &MouseButtonState::Up
        ));
        assert!(!is_session_toggle_click(
            &MouseButton::Left,
            &MouseButtonState::Down
        ));
    }

    #[test]
    fn active_dictation_states_may_override_the_ear_temporarily() {
        // While a dictation is actually recording/transcribing, showing that state is
        // honest too — Idle is the only request a live session must veto.
        assert_eq!(
            resolve_tray_state(TrayIconState::Recording, true),
            TrayIconState::Recording
        );
        assert_eq!(
            resolve_tray_state(TrayIconState::Transcribing, true),
            TrayIconState::Transcribing
        );
    }
}
