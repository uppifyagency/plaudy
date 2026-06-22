# 11 — Frontend (React / TypeScript): App Shell, Zustand Stores, Settings UI, Overlay Window, i18n

> **Abstract.** Handy's frontend is a Tauri 2.x React 18 + TypeScript single-page app built with Vite, organized around two webview entry points: the **main settings/onboarding window** (`index.html` → `src/main.tsx` → `App.tsx`) and a separate, transparent, always-on-top **recording overlay window** (`src/overlay/index.html` → `src/overlay/main.tsx` → `RecordingOverlay.tsx`). It is intentionally *thin*: nearly all real logic (audio, VAD, transcription, models, history, post-processing) lives in Rust and is reached through an auto-generated, fully typed IPC layer in `src/bindings.ts` (tauri-specta). State is held in two Zustand stores — `settingsStore.ts` (application settings, audio devices, post-process providers) and `modelStore.ts` (speech models + download lifecycle) — both of which use the `subscribeWithSelector` middleware and subscribe to backend **events** (`listen(...)`) so the Rust side remains the single source of truth. The UI is fully internationalized via `i18next` (20 locales, RTL-aware) and gated behind a three-step onboarding state machine (`accessibility → model → done`). This document is a forensic, file-by-file account of that subsystem and the concrete points where a Plaud-style product (long-form/call capture, diarization, AI summaries, sync, mobile) would hook in.

---

## 1. Per-file responsibility

### Entry points & app shell
- **`src/main.tsx`** — Main-window React bootstrap. Sets `document.documentElement.dataset.platform` from `@tauri-apps/plugin-os` *before render* (so CSS can scope per-platform), imports `./i18n` (side-effecting init), eagerly calls `useModelStore.getState().initialize()`, and mounts `<App/>` in `<React.StrictMode>`.
- **`src/App.tsx`** — Root component. Owns the onboarding finite-state machine, the active settings section, global event toasts (recording/paste/model errors), the debug-mode keyboard shortcut, RTL initialization, and post-onboarding backend init (Enigo, shortcuts, audio device refresh). Renders `Sidebar` + active settings panel + `Footer` + `Toaster`.
- **`src/overlay/main.tsx`** — Overlay-window React bootstrap. Imports `@/i18n` and mounts `<RecordingOverlay/>` in StrictMode. Minimal — no store, no onboarding.
- **`src/overlay/RecordingOverlay.tsx`** — The floating recording HUD. Subscribes to `show-overlay`/`hide-overlay`/`mic-level` Rust events, renders a 9-bar animated mic visualizer / "transcribing" / "processing" states, and a cancel button that calls `commands.cancelOperation()`.

### Stores (Zustand)
- **`src/stores/settingsStore.ts`** — Single source of *cached* settings on the frontend. Mirrors the Rust `AppSettings`, exposes optimistic `updateSetting`/`updateBinding`, a per-key dispatch table (`settingUpdaters`) mapping each setting to a Tauri command, audio/output device lists, custom sound flags, and the entire post-processing provider/model/key/prompt surface.
- **`src/stores/modelStore.ts`** — Speech-model catalog + download/verify/extract lifecycle. Holds `models: ModelInfo[]`, current model, and per-model progress maps. Uses Immer for nested mutation and subscribes to ~10 `model-*` events from Rust.

### Hooks
- **`src/hooks/useSettings.ts`** — Thin façade hook over `settingsStore`. Triggers `initialize()` on first mount (when `isLoading`), and re-exposes a curated subset of store fields/actions with a stable typed interface (`UseSettingsReturn`).
- **`src/hooks/useOsType.ts`** — (referenced by `HistorySettings`) returns the OS type for platform branches (e.g. Linux audio blob handling).

### Settings UI
- **`src/components/Sidebar.tsx`** — Declarative section registry (`SECTIONS_CONFIG`) + sidebar nav. Each section has a `labelKey`, icon, component, and an `enabled(settings)` predicate that conditionally shows Post-Processing / Debug tabs.
- **`src/components/settings/general/GeneralSettings.tsx`** — Default panel: transcribe/cancel shortcuts, push-to-talk, model card, mic/output device selectors, audio feedback + volume, mute-while-recording.
- **`src/components/settings/post-processing/PostProcessingSettings.tsx`** — LLM post-processing UI: provider/base-url/api-key/model selection (the `*Api` half) and named prompt CRUD (the `*Prompts` half). This is the closest existing surface to "AI summaries."
- **`src/components/settings/history/HistorySettings.tsx`** — Paginated (cursor, `PAGE_SIZE = 30`) transcription history with infinite scroll (`IntersectionObserver`), per-entry copy/save/re-transcribe/delete, inline audio playback, and live updates via the `history-update-payload` event.
- **`src/components/settings/AppLanguageSelector.tsx`** — UI-language dropdown; calls `i18n.changeLanguage` + persists `app_language`.
- **`src/components/onboarding/Onboarding.tsx`** — First-run model picker; drives download→verify→extract→select then signals completion.
- **`src/components/onboarding/AccessibilityOnboarding.tsx`** — macOS/Windows permission gate (accessibility + microphone) with polling.

### i18n & RTL
- **`src/i18n/index.ts`** — i18next setup. Auto-discovers `locales/*/translation.json` via `import.meta.glob` (eager), builds the `SUPPORTED_LANGUAGES` list (priority-sorted), syncs language from `app_language` setting (falling back to OS locale), and wires `languageChanged` → document `dir`/`lang`.
- **`src/i18n/languages.ts`** — `LANGUAGE_METADATA` table: name, native name, dropdown priority, and `direction: 'rtl'` for Arabic/Hebrew.
- **`src/lib/utils/rtl.ts`** — `isRTLLanguage` / `getLanguageDirection` / `updateDocumentDirection` / `updateDocumentLanguage` / `initializeRTL`.

### Shared types / IPC
- **`src/lib/types/events.ts`** — Hand-written event payload interfaces (`ModelStateEvent`, `RecordingErrorEvent`) for events not covered by tauri-specta.
- **`src/bindings.ts`** — **Auto-generated** (tauri-specta). The `commands` object (typed wrappers returning `Result<T,string>`), the `events` object (`__makeEvents__`, e.g. `historyUpdatePayload`), and all shared types (`AppSettings`, `ModelInfo`, `HistoryEntry`, `EngineType`, etc.). Do not edit by hand.

---

## 2. Important types, structs & public functions (with file:line)

### 2.1 `App.tsx`

- **`type OnboardingStep = "accessibility" | "model" | "done"`** — `App.tsx:21`. The onboarding FSM alphabet.
- **`renderSettingsContent(section: SidebarSection): JSX.Element`** — `App.tsx:23-27`. Looks up `SECTIONS_CONFIG[section]?.component`, defaulting to `general`, and renders it. No props passed — every settings panel self-fetches via `useSettings()`.
- **`function App()`** — `App.tsx:29`. State: `onboardingStep` (`null` while checking), `isReturningUser`, `currentSection` (`"general"` default, `App.tsx:37-38`), plus `settings`/`updateSetting` from `useSettings()` (`App.tsx:39`) and `refreshAudioDevices`/`refreshOutputDevices` selected directly from the store (`App.tsx:41-46`).
- **`checkOnboardingStatus(): Promise<void>`** — `App.tsx:168-224`. Calls `commands.hasAnyModelsAvailable()`; if models exist → returning user → on macOS checks accessibility+mic (`checkAccessibilityPermission`/`checkMicrophonePermission`, `App.tsx:181-189`), on Windows checks `commands.getWindowsMicrophonePermissionStatus()` (`App.tsx:198-207`); routes to `"accessibility"` if a permission is missing, else `"done"`. No models → new user → `"accessibility"`.
- **`revealMainWindowForPermissions(): Promise<void>`** — `App.tsx:160-166`. Calls `commands.showMainWindowCommand()` so returning users with a hidden/tray-started window can grant permissions.
- **`handleAccessibilityComplete()`** — `App.tsx:226-230`. Returning users → `"done"`; new users → `"model"`.
- **`handleModelSelected()`** — `App.tsx:232-235`. Transitions to `"done"` once a download has started.
- Render branches: `null` while checking (`App.tsx:238-240`); `<AccessibilityOnboarding/>` (`App.tsx:242-244`); `<Onboarding/>` (`App.tsx:246-248`); the full settings shell with `dir={direction}` (`App.tsx:250-286`).

### 2.2 `settingsStore.ts`

- **`interface SettingsStore`** — `settingsStore.ts:12-65`. The store's full shape: state (`settings`, `defaultSettings`, `isLoading`, `isUpdating: Record<string,boolean>`, `audioDevices`, `outputDevices`, `customSounds`, `postProcessModelOptions`) and ~30 actions.
- **`const DEFAULT_AUDIO_DEVICE: AudioDevice`** — `settingsStore.ts:70-74`. The synthetic `{index:"default", name:"Default", is_default:true}` always prepended to device lists.
- **`const settingUpdaters`** — `settingsStore.ts:76-158`. A partial map `{ [K in keyof Settings]?: (value) => Promise<unknown> }`. The heart of the write path: each settable key maps to its dedicated Tauri command (e.g. `always_on_microphone → commands.updateMicrophoneMode`, `selected_microphone → commands.setSelectedMicrophone` with `"Default"|null → "default"` normalization at `:92-97`). Keys **not** in this table that are also not `bindings`/`selected_model` log a "No handler" warning (`settingsStore.ts:291-292`).
- **`useSettingsStore = create<SettingsStore>()(subscribeWithSelector((set,get)=>({...})))`** — `settingsStore.ts:160`. Selector-subscription middleware lets components subscribe to a single slice.
- **`refreshSettings(): Promise<void>`** — `settingsStore.ts:188-210`. `commands.getAppSettings()` → normalizes nullable device fields to `"Default"` (`:193-200`) → sets `settings` + `isLoading:false`.
- **`refreshAudioDevices()` / `refreshOutputDevices()`** — `settingsStore.ts:213-252`. `commands.getAvailableMicrophones()` / `getAvailableOutputDevices()`, filtering out backend "Default"/"default" duplicates and prepending `DEFAULT_AUDIO_DEVICE`. On error, falls back to `[DEFAULT_AUDIO_DEVICE]`.
- **`updateSetting<K>(key, value): Promise<void>`** — `settingsStore.ts:273-302`. **Optimistic**: sets `isUpdating[key]=true`, mutates local `settings` immediately (`:284-286`), dispatches via `settingUpdaters[key]`, and on throw **rolls back** to `originalValue` (`:296-297`).
- **`updateBinding(id, binding)`** — `settingsStore.ts:316-377`. Optimistic shortcut update via `commands.changeBinding`; checks both `result.status` and `result.data.success`; rolls back `current_binding` on failure and re-throws.
- **`resetBinding(id)`** — `settingsStore.ts:380-394`. `commands.resetBinding` then `refreshSettings()`.
- **`setPostProcessProvider(providerId)`** — `settingsStore.ts:396-435`. Optimistic provider switch; clears cached model options for the new provider (`:418`) to avoid stale dropdowns; `commands.setPostProcessProvider` + `refreshSettings`; rolls back `previousId` on error.
- **`updatePostProcessBaseUrl(providerId, baseUrl)`** — `settingsStore.ts:467-511`. Persists base URL, then **resets the stored model to `""`** because a model valid for one endpoint is usually invalid for another (e.g. Groq→Cerebras), clears cached options, single `refreshSettings`.
- **`fetchPostProcessModels(providerId)`** — `settingsStore.ts:528-551`. Calls `commands.fetchPostProcessModels` (backend HTTP, *not* a frontend `fetch`), caches result in `postProcessModelOptions[providerId]`; returns `[]` on error without caching (so user can retry).
- **`loadDefaultSettings()`** — `settingsStore.ts:562-573`. `commands.getDefaultSettings()` — platform-specific defaults are computed in Rust (see comment `settingsStore.ts:67-68`).
- **`initialize()`** — `settingsStore.ts:576-594`. `Promise.all([loadDefaultSettings, refreshSettings, checkCustomSounds])`, then `listen("model-state-changed", ...)` → `refreshSettings()` (backend resets language during model switches; Rust is authoritative). **Note (`:579-582`):** audio devices are deliberately *not* refreshed here to avoid triggering macOS permission dialogs before onboarding completes.

### 2.3 `modelStore.ts`

- **`interface DownloadProgress`** — `modelStore.ts:8-13`: `{model_id, downloaded, total, percentage}`. Matches the Rust `model-download-progress` payload.
- **`interface DownloadStats`** — `modelStore.ts:15-20`: `{startTime, lastUpdate, totalDownloaded, speed}` — frontend-only EMA speed estimate.
- **`interface ModelsStore`** — `modelStore.ts:23-57`. Uses `Record<string,true>` instead of `Set` for Immer compatibility (`:22`): `downloadingModels`, `verifyingModels`, `extractingModels`, plus `downloadProgress`/`downloadStats` records, `currentModel`, `hasAnyModels`, `isFirstRun`, `initialized`.
- **`loadModels()`** — `modelStore.ts:80-118`. `commands.getAvailableModels()`; reconciles frontend `downloadingModels` with the backend's `is_downloading` flag inside an Immer `produce` (`:87-108`) — keeps frontend-known downloads, drops ones the backend says are done.
- **`selectModel(modelId): Promise<boolean>`** — `modelStore.ts:146-165`. `commands.setActiveModel`; on success sets `currentModel`, `isFirstRun:false`, `hasAnyModels:true`.
- **`downloadModel(modelId): Promise<boolean>`** — `modelStore.ts:167-207`. Optimistically marks downloading + zero progress, calls `commands.downloadModel`; has **belt-and-suspenders cleanup** for both non-ok results (`:185-192`) and JS exceptions (`:196-205`) where the `model-download-failed` event may not fire.
- **`cancelDownload` / `deleteModel`** — `modelStore.ts:209-251`. Call the respective command and re-sync via `loadModels()`/`loadCurrentModel()`.
- **`initialize()`** — `modelStore.ts:273-430`. Idempotent (`if(get().initialized) return`). `Promise.all([loadModels, loadCurrentModel, checkFirstRun])`, then registers **all** `model-*` event listeners (see §4). The progress listener (`:282-325`) computes a smoothed MB/s speed (EMA: `current.speed*0.8 + validCurrentSpeed*0.2`, throttled to >0.5 s windows, `:307-320`).

### 2.4 `useSettings.ts`

- **`interface UseSettingsReturn`** — `useSettings.ts:5-44`. The hook's public contract.
- **`useSettings(): UseSettingsReturn`** — `useSettings.ts:46-78`. Subscribes to the whole store (`useSettingsStore()`), calls `store.initialize()` once on mount while `isLoading` (`:50-54`), and returns a curated projection including the derived `audioFeedbackEnabled = settings?.audio_feedback || false` (`:62`).

### 2.5 `RecordingOverlay.tsx`

- **`type OverlayState = "recording" | "transcribing" | "processing"`** — `RecordingOverlay.tsx:14`. Matches the `show-overlay` payload string from Rust.
- **`RecordingOverlay: React.FC`** — `RecordingOverlay.tsx:16`. State: `isVisible`, `state`, `levels: number[]` (16 zeros init), and a `smoothedLevelsRef` for jitter reduction.
- **`setupEventListeners()`** — `RecordingOverlay.tsx:25-60`. Registers `show-overlay` (calls `syncLanguageFromSettings()` each show, `:29`), `hide-overlay`, and `mic-level` (EMA smoothing `prev*0.7 + target*0.3`, then `slice(0,9)` for 9 bars, `:45-51`). Returns cleanup; note the cleanup is created inside an async function whose returned cleanup is *not* awaited back into the `useEffect` return (see §5 — a latent leak).
- Bars render: height `Math.min(20, 4 + Math.pow(v,0.7)*16)px`, opacity `Math.max(0.2, v*1.7)` (`RecordingOverlay.tsx:87-91`).
- Cancel button → `commands.cancelOperation()` (`RecordingOverlay.tsx:108-110`).

### 2.6 `Sidebar.tsx`

- **`interface SectionConfig`** — `Sidebar.tsx:27-32`: `{labelKey, icon, component, enabled(settings):boolean}`.
- **`const SECTIONS_CONFIG`** — `Sidebar.tsx:34-77` (`as const satisfies Record<string,SectionConfig>`). Seven sections: `general`, `models`, `advanced`, `history` (all `enabled: ()=>true`), `postprocessing` (`enabled: s => s?.post_process_enabled ?? false`, `:63`), `debug` (`enabled: s => s?.debug_mode ?? false`, `:69`), `about`.
- **`type SidebarSection = keyof typeof SECTIONS_CONFIG`** — `Sidebar.tsx:17`.
- **`Sidebar: React.FC<SidebarProps>`** — `Sidebar.tsx:84`. Filters sections by `enabled(settings)` (`:91-93`) and renders nav items; active item highlighted with `bg-logo-primary/80`.

### 2.7 `i18n/index.ts`

- **`localeModules = import.meta.glob(...,{eager:true})`** — `i18n/index.ts:13-16`. Build-time discovery of all `locales/*/translation.json`.
- **`SUPPORTED_LANGUAGES`** — `i18n/index.ts:28-50`. Joins discovered codes with `LANGUAGE_METADATA`, warns on missing metadata (`:31`), sorts by `priority` then name.
- **`getSupportedLanguage(langCode): string|null`** — `i18n/index.ts:55-72`. Exact match → prefix (region-stripped) match → `null`.
- **`i18n.use(initReactI18next).init({...})`** — `i18n/index.ts:76-86`. `lng:"en"`, `fallbackLng:"en"`, `escapeValue:false`, `useSuspense:false`.
- **`syncLanguageFromSettings(): Promise<void>`** — `i18n/index.ts:89-108`. `commands.getAppSettings()` → uses `app_language` if set, else OS `locale()`; changes language only if supported and different. **Exported** and re-called by the overlay on each show.
- **`i18n.on("languageChanged", ...)`** — `i18n/index.ts:114-118`. Updates document `dir`/`lang`.

### 2.8 `bindings.ts` (generated — key types)

- **`AppSettings`** — `bindings.ts:835`. ~50 fields incl. `bindings`, `push_to_talk`, `selected_model`, device fields, `post_process_*`, `whisper_accelerator`, `ort_accelerator`, `app_language`, `experimental_enabled`.
- **`HistoryEntry`** — `bindings.ts:844`: `{id, file_name, timestamp, saved, title, transcription_text, post_processed_text|null, post_process_prompt|null, post_process_requested}`.
- **`HistoryUpdatePayload`** — `bindings.ts:845`: discriminated union `added|updated|deleted|toggled`.
- **`ModelInfo`** — `bindings.ts:857`: incl. `engine_type`, `accuracy_score`, `speed_score`, `supports_translation`, `is_recommended`, `supported_languages`, `is_custom`.
- **`EngineType`** — `bindings.ts:842`: `"Whisper"|"Parakeet"|"Moonshine"|"MoonshineStreaming"|"SenseVoice"|"GigaAM"|"Canary"|"Cohere"`. (No diarization engine.)
- **`LLMPrompt`** — `bindings.ts:855`: `{id, name, prompt}`.
- **`PostProcessProvider`** — `bindings.ts:865`: `{id, label, base_url, allow_base_url_edit?, models_endpoint?, supports_structured_output?}`.
- **`RecordingRetentionPeriod`** — `bindings.ts:866`: `"never"|"preserve_limit"|"days_3"|"weeks_2"|"months_3"`.
- **`events`** (`__makeEvents__`) — `bindings.ts:823-826`: only `historyUpdatePayload → "history-update-payload"` is strongly typed; all other events are subscribed via raw string `listen(...)`.

---

## 3. Threading / concurrency model

The frontend is **single-threaded JS** (no Web Workers). "Concurrency" is async + event-driven:

- **IPC calls** are `async` Tauri commands (`commands.*`) returning `Promise<Result<T,string>>`. Errors come back as `{status:"error", error}` rather than throwing (network/IPC failures still throw and are caught separately).
- **Backend → frontend events** via `listen<T>(name, cb)` (`@tauri-apps/api/event`). Subscriptions are registered in store `initialize()` and in component `useEffect`s; each returns an unlisten promise that must be resolved and called in cleanup.
- **Optimistic concurrency** is the dominant pattern: local Zustand state is mutated first, then the command is awaited, then rolled back on failure (`updateSetting` `settingsStore.ts:273-302`; `updateBinding` `:316-377`; `toggleSaved`/`deleteAudioEntry` in `HistorySettings.tsx:153-217`).
- **Per-key in-flight tracking**: `isUpdating: Record<string,boolean>` (`settingsStore.ts:175-178`) guards individual settings to debounce/disable controls during a write.
- **Immer `produce`** (`modelStore.ts`) gives transactional nested updates to the progress/verify/extract maps without manual spread.
- **Polling loop**: `AccessibilityOnboarding` uses `setInterval(...,1000)` (`AccessibilityOnboarding.tsx:165-235`) with `pollingRef`/`timeoutRef`/`errorCountRef` and a `MAX_POLLING_ERRORS=3` circuit breaker.
- **Cross-window**: the overlay is a **separate webview** with its own JS runtime/store-less context; it shares state with the main window only through Rust events and `syncLanguageFromSettings()`.

---

## 4. Data flow IN / OUT (callers, callees, message types)

### Inbound (Rust → React) events

Subscribed in `modelStore.initialize()` (`modelStore.ts:282-427`) unless noted:
- `model-download-progress` → updates `downloadProgress`/`downloadStats`.
- `model-download-complete` (`:327`), `model-download-failed` (`:340`, also `toast.error`), `model-download-cancelled` (`:407`).
- `model-verification-started`/`-completed` (`:357`/`:366`).
- `model-extraction-started`/`-completed`/`-failed` (`:375`/`:384`/`:394`).
- `model-deleted` (`:419`), `model-state-changed` (`:424`, also in `settingsStore.ts:591` and `App.tsx:142`).

Subscribed in `App.tsx`:
- `recording-error` (`App.tsx:100-119`) — payload `RecordingErrorEvent` (`error_type`: `microphone_permission_denied` | `no_input_device` | other) → localized toast.
- `paste-error` (`App.tsx:130-134`) → localized toast (technical detail stays in `handy.log`).
- `model-state-changed` (`App.tsx:142-154`) — payload `ModelStateEvent`; on `loading_failed` → toast.

Subscribed in `RecordingOverlay.tsx`: `show-overlay`/`hide-overlay`/`mic-level` (emitted from Rust `overlay.rs:338/378/390-394`).

Subscribed in `HistorySettings.tsx`: `events.historyUpdatePayload.listen` (`:135-151`) — typed `added`/`updated` applied; `deleted`/`toggled` ignored (handled by optimistic UI).

### Outbound (React → Rust) commands

Representative callers → commands:
- Settings writes: `settingUpdaters[key]` → ~40 distinct `commands.*` (`settingsStore.ts:76-158`).
- Bindings: `commands.changeBinding` / `commands.resetBinding`.
- Devices: `commands.getAvailableMicrophones` / `getAvailableOutputDevices` / `playTestSound` / `checkCustomSounds`.
- Models: `getAvailableModels`, `getCurrentModel`, `hasAnyModelsAvailable`, `setActiveModel`, `downloadModel`, `cancelDownload`, `deleteModel`.
- History: `getHistoryEntries(cursor,limit)`, `toggleHistoryEntrySaved`, `deleteHistoryEntry`, `retryHistoryEntryTranscription`, `getAudioFilePath`, `openRecordingsFolder`.
- Post-processing: `setPostProcessProvider`, `changePostProcess{BaseUrl,ApiKey,Model}Setting`, `fetchPostProcessModels`, `addPostProcessPrompt`, `updatePostProcessPrompt`, `deletePostProcessPrompt`, `setPostProcessSelectedPrompt`.
- Lifecycle/permissions: `initializeEnigo`, `initializeShortcuts` (`App.tsx:63-64`), `showMainWindowCommand`, `getWindowsMicrophonePermissionStatus`, `openMicrophonePrivacySettings`, `cancelOperation` (overlay).

**Window topology** (`vite.config.ts:22-25`, `tauri.conf.json`): Vite builds two entry points — `main → index.html` and `overlay → src/overlay/index.html`. The `app.windows` array in `tauri.conf.json` is **empty** (`tauri.conf.json:14`); windows are created **programmatically in Rust** (`overlay.rs:240-243`, label `"recording_overlay"`, URL `src/overlay/index.html`, `decorations(false).always_on_top(true).skip_taskbar(true).transparent(true).focused(false).visible(false)`).

---

## 5. Error handling & edge cases

- **Result-not-throw discipline**: every command result is checked for `status==="ok"` before reading `.data`. Network/IPC exceptions are caught in `try/catch` and logged (`console.error`).
- **Optimistic rollback** on settings (`:296-297`) and bindings (`:355-369`); history operations revert on failure (`HistorySettings.tsx:160-172, 209-216`).
- **Download cleanup redundancy** (`modelStore.ts:185-205`): cleans local state both when the command returns non-ok *and* when JS throws, because the `model-download-failed` event might not arrive (e.g. listener not yet registered, IPC error).
- **Permission-check failures are non-fatal**: if `checkAccessibilityPermission`/`checkMicrophonePermission` throw, `App.tsx` proceeds to the main app and lets the user fix it there (`App.tsx:190-193, 208-211`).
- **Polling circuit breaker**: `AccessibilityOnboarding` stops after 3 consecutive errors and toasts (`AccessibilityOnboarding.tsx:226-233`).
- **i18n fallback chain**: missing key → `fallbackLng:"en"`; missing locale metadata → warn + use code as name (`i18n/index.ts:31`); unsupported `app_language` → OS locale → English.
- **Latent overlay listener leak** (`RecordingOverlay.tsx:24-63`): `setupEventListeners` is `async` and returns a cleanup, but the `useEffect` callback does not capture/await that returned cleanup — the three `listen` subscriptions are effectively never torn down. Harmless in practice (overlay window is long-lived, StrictMode double-mount aside) but worth noting for any refactor that mounts/unmounts the overlay.
- **`refreshSettings` device normalization** (`settingsStore.ts:193-200`) guards against `null` device fields that would otherwise break selectors.

---

## 6. State & persistence touched

- **Backend settings store** (`tauri-plugin-store`): the frontend never persists directly. Every write goes Zustand → `commands.*` → Rust → store file. `refreshSettings`/`refreshAudioDevices` read it back.
- **SQLite history**: read via `getHistoryEntries` (cursor pagination), mutated via toggle/delete/retry commands; surfaced in `HistorySettings`.
- **Audio files on disk**: `getAudioFilePath(fileName)` → `convertFileSrc(...,"asset")` (or, on Linux, `readFile` + Blob URL, `HistorySettings.tsx:188-194`); `openRecordingsFolder` reveals the directory.
- **Model files on disk**: catalog + download state from `getAvailableModels`; lifecycle owned by Rust, mirrored in `modelStore`.
- **Secrets**: post-process API keys live in `AppSettings.post_process_api_keys: SecretMap` (`bindings.ts:835/867`) — written via `changePostProcessApiKeySetting`, never echoed back in plaintext to the model dropdown logic.
- **Browser-local**: only `document.documentElement.dataset.platform`/`dir`/`lang` attributes; no `localStorage` usage in the read files. Zustand state is in-memory and reconstructed from Rust on each launch.

---

## 7. Platform-specific branches

- **`main.tsx:7`** — sets `dataset.platform` for per-platform CSS (e.g. scrollbars).
- **macOS**: accessibility + microphone permission flow via `tauri-plugin-macos-permissions-api` (`App.tsx:7-9, 181-189`; `AccessibilityOnboarding.tsx:4-9, 96-135`). Enigo/shortcuts initialized only after accessibility granted (`AccessibilityOnboarding.tsx:104-113, 193-202`).
- **Windows**: mic permission via `commands.getWindowsMicrophonePermissionStatus()` + `openMicrophonePrivacySettings()` (`App.tsx:196-212`; `AccessibilityOnboarding.tsx:66-75, 261-275`). `WindowsMicrophonePermissionStatus` type at `bindings.ts:872`.
- **Linux**:
  - History audio loaded as a Blob URL instead of `asset://` (`HistorySettings.tsx:188-194`) via `useOsType()`.
  - Cancel shortcut hidden on Linux due to "dynamic shortcut instability" (`GeneralSettings.tsx:19, 26`).
  - Overlay uses GTK layer shell in Rust (`overlay.rs:49-113`), out of frontend scope.
- **"other" platforms**: `AccessibilityOnboarding` short-circuits to `onComplete()` (no permissions UI) (`AccessibilityOnboarding.tsx:90-93`).
- **RTL**: Arabic/Hebrew flip `dir` (`languages.ts:35-36`; `rtl.ts`).
- **iOS / Android: none.** There are no mobile cfg branches anywhere in the frontend — the platform helper only knows macos/windows/linux/other.

---

## 8. PLAUD relevance — concrete extension points

The frontend is a thin client, so most Plaud features land in Rust, but the React layer needs deliberate new surfaces. Concrete hooks:

1. **Long-form / call / conversation capture.**
   - The overlay's `OverlayState` union (`RecordingOverlay.tsx:14`) and the `show-overlay` payload would gain a long-running `"conversation"` state with elapsed-time + pause/resume controls. Wrap the cancel-only `overlay-right` block (`RecordingOverlay.tsx:104-115`) to add stop/pause buttons calling new `commands.pauseRecording()/resumeRecording()`.
   - Add a top-level "Record" section to `SECTIONS_CONFIG` (`Sidebar.tsx:34-77`) — a primary capture view rather than burying recording behind a global shortcut. This is the single biggest UX shift toward Plaud.
   - `RecordingRetentionPeriod` (`bindings.ts:866`) caps at `months_3`; extend the enum + `RecordingRetentionPeriod.tsx` for indefinite long-form retention.

2. **System-audio / call audio capture.** No frontend control exists today (only mic selectors: `MicrophoneSelector`, `OutputDeviceSelector`). Add a capture-source selector component beside `MicrophoneSelector` in `GeneralSettings.tsx:31-41`, backed by a new `selected_capture_source` setting wired through `settingUpdaters` (`settingsStore.ts:76-158`) to a new Rust command.

3. **Multi-speaker conversations & diarization.**
   - `HistoryEntry` (`bindings.ts:844`) has only a flat `transcription_text`. A Plaud transcript needs `segments: {speaker, start, end, text}[]`. Extend the Rust type (regenerates `bindings.ts`) and replace the single `<p>` render in `HistoryEntryComponent` (`HistorySettings.tsx:413-440`) with a speaker-labeled segment list.
   - `EngineType` (`bindings.ts:842`) has no diarization engine; the model store/onboarding (`Onboarding.tsx`, `modelStore.ts`) would need a parallel "diarization model" concept (a second `ModelInfo` family or a sibling store) since it currently assumes one active transcription model.

4. **AI summaries.** The post-processing subsystem is the natural home.
   - `PostProcessingSettingsPrompts` (`PostProcessingSettings.tsx:146-415`) already does named LLM-prompt CRUD against `commands.addPostProcessPrompt`/`updatePostProcessPrompt`. Ship default "Meeting summary", "Action items", "Key decisions" prompts and surface a **per-history-entry "Summarize"** action in `HistorySettings.tsx` (add an `IconButton` near `:360-409` calling a new `commands.summarizeHistoryEntry(id, promptId)`).
   - `post_processed_text`/`post_process_prompt` already exist on `HistoryEntry` (`bindings.ts:844`) but are **not rendered** — add a collapsible summary panel below the transcript in `HistoryEntryComponent`.
   - `PostProcessProvider.supports_structured_output` (`bindings.ts:865`) is the hook for JSON-schema summaries (title + bullets + action items).

5. **Cloud / local sync.** No sync UI exists. Add a `sync` section to `SECTIONS_CONFIG` (`Sidebar.tsx`) and a `syncStore.ts` modeled on `modelStore.ts` (status/progress maps + `listen("sync-*")` events). The history store would need to merge remote entries into the optimistic `setEntries` flow (`HistorySettings.tsx:135-151`).

6. **Mobile (iPhone).** The frontend is reusable in principle (React + Tauri), but: there are **no iOS platform branches**, permission flows are macOS/Windows-only (`AccessibilityOnboarding.tsx`), the overlay window model is desktop-specific, and Tauri mobile would need its own window/permission story. Realistically the React **settings/history/summary views** could be shared into a Tauri-iOS or React Native shell, but capture + overlay must be rebuilt natively. Start by factoring `HistorySettings`/`PostProcessingSettings` to not assume desktop-only commands.

7. **i18n already covers it.** 20 locales + RTL (`i18n/index.ts`, `languages.ts`) — any new Plaud strings just need keys in `locales/en/translation.json`; ESLint enforces no hardcoded JSX strings (see `AGENTS.md`).

---

## 9. Gaps vs a Plaud-style product

- **No first-class recording UI.** Capture is invisible — triggered only by a global shortcut and shown via a tiny overlay. There is no "press record, watch a live transcript, stop, review" screen.
- **No live / streaming transcript view.** The overlay shows only a mic-level visualizer and "transcribing…" text (`RecordingOverlay.tsx:96-101`); the transcript appears only after completion in History.
- **No speaker diarization anywhere** — no engine (`EngineType` `bindings.ts:842`), no data model (`HistoryEntry` is flat `bindings.ts:844`), no UI.
- **No system/loopback/call audio capture controls** — only physical mic + output device selectors.
- **No long-form recording affordances** — no pause/resume, no chaptering, no in-app elapsed timer; retention caps at 3 months (`bindings.ts:866`).
- **Summaries are under-surfaced** — the data exists (`post_processed_text`, `post_process_prompt`) but is neither rendered in History nor invokable per-entry; there's no summary-on-demand button, only an inline post-process-during-transcription hotkey.
- **No sync / multi-device** — no cloud store, no account, no conflict handling; all state is local and rebuilt from Rust per launch.
- **No mobile** — zero iOS/Android branches; permission and overlay flows are desktop-only.
- **No tagging / search / folders** in History — only cursor pagination, a `saved` star, and client-side copy (`HistorySettings.tsx`). No full-text search over transcripts.
- **No export** beyond clipboard copy and "open folder" — no Markdown/PDF/share export of transcript+summary.
- **Title field unused** — `HistoryEntry.title` exists (`bindings.ts:844`) but isn't shown/edited; Plaud relies heavily on auto-generated titles.
