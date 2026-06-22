# 12 — Build System, Dependency Graph, Platform/GPU Backends, Nix, Packaging

> **Abstract.** Handy is a Tauri 2.x application: a Rust backend (`src-tauri/`) compiled into a static/cdylib library (`handy_app_lib`) plus a thin `handy` binary, and a React/TypeScript frontend bundled by Vite. The build subsystem is responsible for (a) compiling the Rust workspace with the correct **per-platform GPU acceleration backend** for speech-to-text (Metal on macOS, Vulkan on Linux/Windows, plus DirectML/CoreML/CUDA via ONNX Runtime), (b) running a `build.rs` that generates tray-menu translation tables and, on Apple Silicon, compiles a **Swift ↔ Rust FoundationModels bridge** for on-device Apple Intelligence summarization, (c) bundling the app into platform installers (`dmg`/`app` on macOS, `deb`/`rpm`/`AppImage` on Linux, `nsis` `.exe` on Windows) with a signed auto-updater, and (d) reproducibly building the Linux package via a **Nix flake** with `bun2nix`-generated JS dependencies. This document is a forensic, file-cited map of that subsystem, with explicit hooks for turning Handy into a Plaud-style always-on recorder/note-taker and the gaps that block it (no iOS build, no cloud sync, model/binaries fetched from a single private blob host).

---

## 1. Per-file responsibility map

| Path | Responsibility |
| --- | --- |
| `handy/src-tauri/Cargo.toml` | Rust crate manifest. Declares the `handy` binary + `handy_app_lib` lib, all dependencies, **per-target `transcribe-rs` feature flags that select the GPU backend**, release profile (LTO, strip), and a `[patch.crates-io]` that pins Tauri's runtime crates to a Handy fork. |
| `handy/src-tauri/build.rs` | Cargo build script. Generates `tray_translations.rs` from frontend locale JSON, and on `macos+aarch64` compiles the Swift Apple-Intelligence bridge into a static lib, then runs `tauri_build::build()`. |
| `handy/src-tauri/tauri.conf.json` | Tauri bundle/runtime config: product identity, bundle targets (`all`), bundled `resources/**/*`, macOS hardened-runtime + entitlements, Linux deb/rpm/appimage opts, Windows signing + custom NSIS template, and the updater pubkey + GitHub release endpoint. |
| `handy/src-tauri/Entitlements.plist` | macOS code-signing entitlements: microphone + audio-input device access. |
| `handy/src-tauri/Info.plist` | macOS usage-description strings (`NSMicrophoneUsageDescription`). |
| `handy/src-tauri/capabilities/default.json` | Tauri v2 permission capability for the `main` + `recording_overlay` windows (store, updater, fs, global-shortcut, macos-permissions, asset scope). |
| `handy/src-tauri/capabilities/desktop.json` | Desktop-only capability (autostart, global-shortcut, updater). |
| `handy/src-tauri/nsis/installer.nsi` | Forked Tauri NSIS Windows installer template with bespoke **portable-install** pages and a `/PORTABLE` silent flag. |
| `handy/src-tauri/gen/apple/PrivacyInfo.xcprivacy` | Apple privacy-manifest (declares file-timestamp API usage) — scaffolding for an eventual App Store / notarized build. |
| `handy/src-tauri/icons/{android,ios}/` | Pre-generated Android mipmap + iOS AppIcon sets. **Icon scaffolding only — there is no mobile build target wired up.** |
| `handy/src-tauri/swift/apple_intelligence.swift` | Real Swift FoundationModels implementation (`is_apple_intelligence_available`, `process_text_with_system_prompt_apple`, `free_apple_llm_response`) used when the SDK exposes `FoundationModels`. |
| `handy/src-tauri/swift/apple_intelligence_stub.swift` | Stub that returns "unavailable" — compiled when the macOS SDK lacks FoundationModels (keeps the symbols present so Rust links). |
| `handy/src-tauri/swift/apple_intelligence_bridge.h` | C ABI header (`AppleLLMResponse`, the three extern-C functions) imported by `swiftc -import-objc-header` and matched on the Rust side. |
| `handy/src-tauri/src/main.rs` | Process entry point. Parses CLI args, sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` on Linux, calls `handy_app_lib::run`. `windows_subsystem = "windows"` in release suppresses the console window. |
| `handy/src-tauri/src/lib.rs` | Library entry point / Tauri builder. Registers all plugins, collects specta commands/events, exports TS bindings in debug builds, wires platform-gated plugins (`tauri-nspanel` on macOS), and bootstraps managers + tray. |
| `handy/src-tauri/src/cli.rs` | `clap`-derived `CliArgs` struct (the six runtime flags). |
| `handy/src-tauri/src/portable.rs` | Portable-mode detection (marker file next to exe → `Data/` dir) used by the store, log targets, and webview cache path. |
| `handy/src-tauri/src/managers/transcription.rs` (accelerator section) | Maps user accelerator settings onto `transcribe-rs` global atomics, enumerates GPU devices (FMA3-guarded), reports compiled-in accelerators to the UI. |
| `handy/src-tauri/src/settings.rs` (enums) | `WhisperAcceleratorSetting`, `OrtAcceleratorSetting`, `whisper_gpu_device` persisted settings that drive backend selection. |
| `handy/package.json` | Frontend/JS manifest. Dev/build/lint/format scripts, Tauri CLI, Vite, the `@tauri-apps/plugin-*` JS bindings, and the **`postinstall` hook → `scripts/check-nix-deps.ts`**. |
| `handy/vite.config.ts` | Vite config: React + Tailwind plugins, `@` path alias, **two HTML entry points (`main` + `overlay`)**, fixed Tauri dev port 1420 / HMR 1421, ignores `src-tauri/`. |
| `handy/flake.nix` | Nix flake: reproducible Linux package via `rustPlatform.buildRustPackage` + `cargo-tauri.hook` + `bun2nix`, a dev shell, and NixOS/home-manager modules. |
| `handy/nix/module.nix` | NixOS module: installs the package + adds a `/dev/uinput` udev rule (rdev virtual input). |
| `handy/nix/hm-module.nix` | Home-manager module: systemd **user** service for autostart on Linux. |
| `handy/.nix/bun.nix` | Auto-generated (`bun2nix`) per-package `fetchurl` expressions for every JS dependency. |
| `handy/.nix/bun-lock-hash` | sha256 of `bun.lock`; lets `check-nix-deps.ts` skip regeneration when unchanged. |
| `handy/scripts/check-nix-deps.ts` | Postinstall hook that regenerates `.nix/bun.nix` when `bun.lock` changes. |
| `handy/BUILD.md` | Human build instructions (per-OS prerequisites, Intel-Mac ORT linking, Linux install-from-deb, AppImage troubleshooting). |
| `handy/AGENTS.md` / `handy/CLAUDE.md` | Agent-facing build/dev command reference. |

---

## 2. The Rust manifest: dependency graph & backend selection

### 2.1 Crate shape (`Cargo.toml:1-24`)

- `name = "handy"`, `version = "0.8.3"`, `edition = "2021"`, `default-run = "handy"` (`Cargo.toml:2-8`).
- `[lib] name = "handy_app_lib"`, `crate-type = ["staticlib", "cdylib", "rlib"]` (`Cargo.toml:19-20`). The `_lib` suffix and triple crate-type exist to avoid the Windows bin/lib name collision (cargo issue 8519, cited in the comment at `Cargo.toml:16-18`) and to support a future mobile (cdylib/staticlib) target.
- A commented-out `[[bin]] cli` (`Cargo.toml:22-24`) shows an audio-toolkit CLI that is currently disabled.

### 2.2 Build dependencies (`Cargo.toml:26-29`)

`tauri-build = "2"`, plus `serde`/`serde_json` — the latter two are needed by `build.rs`'s tray-translation generator.

### 2.3 Cross-platform dependencies (`Cargo.toml:31-79`)

Notable for this subsystem:
- **Tauri core** `tauri = "2.10.2"` with features `protocol-asset`, `macos-private-api`, `tray-icon`, `image-png` (`Cargo.toml:33-38`).
- **Tauri plugins** (cross-platform): `log`, `opener`, `store`, `os`, `clipboard-manager`, `macos-permissions`, `process`, `fs`, `dialog` (`Cargo.toml:39-47,79`).
- **Transcription engine** `transcribe-rs = "0.3.8"` with `whisper-cpp` + `onnx` features as the *default/baseline* (`Cargo.toml:72`). This is **overridden per-target** (see §2.5) to pull in GPU backends.
- **Audio stack**: `cpal`, `rubato`, `hound`, `rodio` (git fork `cjpais/rodio`), `vad-rs` (git fork `cjpais/vad-rs`), `rustfft` (`Cargo.toml:51-63`).
- **Input/keys**: `rdev` (git fork `rustdesk-org/rdev`), `enigo`, `handy-keys` (`Cargo.toml:50,59,73`).
- **Storage**: `rusqlite = "0.37"` with the **`bundled`** feature (compiles SQLite from source — no system dependency), `rusqlite_migration` (`Cargo.toml:46,68`).
- **Networking/download**: `reqwest` (json+stream), `futures-util`, `tar`, `flate2`, `sha2` (model download + checksum + tar.gz extraction) (`Cargo.toml:61-71`).
- **Type bindings**: `specta`, `specta-typescript`, `tauri-specta` (pinned rc versions) generate `src/bindings.ts` (`Cargo.toml:76-78`).
- **Misc text**: `strsim`, `natural`, `regex`, `ferrous-opencc` (Chinese conversion), `chrono`, `clap`, `anyhow`, `once_cell`, `log`, `env_filter`, `tokio` (`Cargo.toml:52-75`).

### 2.4 Unix-only deps (`Cargo.toml:81-82`)

`signal-hook = "0.3"` — used by `signal_handle::setup_signal_handler` for `SIGUSR1`/`SIGUSR2` remote toggling (referenced in `lib.rs:33-36,173-177`).

### 2.5 The desktop-only + per-OS backend matrix (the heart of GPU selection)

This is the single most important build-subsystem fact. The transcription backend is **chosen at compile time via Cargo target-cfg sections**, each re-specifying `transcribe-rs` with different features:

| cfg gate | `Cargo.toml` lines | `transcribe-rs` features | Effect |
| --- | --- | --- | --- |
| `cfg(not(any(android, ios)))` | `84-88` | — (adds desktop plugins: `autostart`, `global-shortcut`, `single-instance`, `updater`) | Desktop-only Tauri plugins; **explicitly excluded on iOS/Android**. |
| `cfg(windows)` | `90-99` | `whisper-vulkan`, `ort-directml` | Whisper-cpp via **Vulkan**; ONNX models via **DirectML**. Also pulls `windows` crate (audio endpoints, registry) + `winreg`. |
| `cfg(target_os = "macos")` | `101-104` | `whisper-metal` | Whisper-cpp via **Metal**. Also pulls `tauri-nspanel` (git fork) for the overlay NSPanel. |
| `cfg(target_os = "linux")` | `105-108` | `whisper-vulkan` | Whisper-cpp via **Vulkan**. Also pulls `gtk-layer-shell` + `gtk` for the Wayland/X11 overlay. |

Note the version skew: the baseline `transcribe-rs` is `0.3.8` (`Cargo.toml:72`) but the per-OS overrides pin `0.3.3` (`Cargo.toml:91,103,108`). Cargo unifies to a single resolved version across the feature union; the GPU/ORT feature flags are additive. macOS Apple-Silicon ONNX acceleration (CoreML) is not selected here — on macOS the ORT execution provider defaults to CPU/auto unless `transcribe-rs` enables CoreML internally.

### 2.6 Tauri fork patch (`Cargo.toml:110-113`)

```toml
[patch.crates-io]
tauri-runtime      = { git = "https://github.com/cjpais/tauri.git", branch = "handy-2.10.2" }
tauri-runtime-wry  = { git = "https://github.com/cjpais/tauri.git", branch = "handy-2.10.2" }
tauri-utils        = { git = "https://github.com/cjpais/tauri.git", branch = "handy-2.10.2" }
```

Handy depends on a **forked Tauri runtime** (likely overlay/NSPanel/window-behaviour patches). This is a supply-chain and maintenance pin worth flagging: any reproducible build must fetch this exact branch.

### 2.7 Release profile (`Cargo.toml:118-122`)

`lto = true`, `codegen-units = 1`, `strip = true`, `panic = "unwind"`. Aggressive size/speed optimization; `panic = "unwind"` (not `abort`) is required because the app catches panics in worker threads. `[profile.dev] incremental = true` (`Cargo.toml:10-11`).

---

## 3. `build.rs` — the build-time code generators (`build.rs:1-254`)

`build.rs` runs three things in `main()` (`build.rs:1-8`):

### 3.1 Apple-Intelligence Swift bridge (Apple Silicon only)

`#[cfg(all(target_os = "macos", target_arch = "aarch64"))] build_apple_intelligence_bridge()` (`build.rs:2-3`, body `114-254`):
- Picks `swift/apple_intelligence.swift` vs `swift/apple_intelligence_stub.swift` based on whether `FoundationModels.framework` exists in the SDK (`build.rs:147-158`).
- Compiles with `swiftc -parse-as-library -target arm64-apple-macosx11.0` (the `-parse-as-library` flag prevents a spurious `_main` symbol that could hijack Rust's `main` under nixpkgs' ld64 — detailed comment at `build.rs:190-198`).
- Honors `SDKROOT` / `SWIFTC` env overrides so non-Xcode toolchains (nixpkgs standalone swift) can build without `xcrun` (`build.rs:132-145,164-176`).
- `libtool -static` packs the `.o` into `libapple_intelligence.a` (`build.rs:220-236`).
- Emits link directives: `cargo:rustc-link-lib=static=apple_intelligence`, swift toolchain + SDK lib search paths, `framework=Foundation`, and **weak-links `FoundationModels`** so the binary still launches on pre-macOS-26 systems (`build.rs:238-253`).

This is the **on-device AI summarization compile path** — directly relevant to Plaud's "AI summary" feature.

### 3.2 Tray translations codegen (`build.rs:14-92`)

`generate_tray_translations()` reads every `src/i18n/locales/*/translation.json`, extracts the `"tray"` object, and writes `$OUT_DIR/tray_translations.rs` containing a `TrayStrings` struct (fields derived from the English keys, camelCase→snake_case via `camel_to_snake` at `build.rs:94-104`) and a `Lazy<HashMap<&str, TrayStrings>>` static. `cargo:rerun-if-changed` is set on the locales dir (`build.rs:22,36`). This is consumed by `tray_i18n` (declared `lib.rs:20`).

### 3.3 `tauri_build::build()` (`build.rs:7`)

Standard Tauri build step: validates capabilities, generates `gen/schemas`, embeds `tauri.conf.json`.

---

## 4. Tauri bundle & runtime configuration (`tauri.conf.json:1-75`)

- **Identity**: `productName "Handy"`, `identifier "com.pais.handy"`, `version 0.8.3` (`:3-5`).
- **Build hooks**: `beforeDevCommand "bun run dev"`, `devUrl localhost:1420`, `beforeBuildCommand "bun run build"`, `frontendDist "../dist"` (`:6-11`).
- **Security**: `macOSPrivateApi: true`, `csp: null`, asset protocol enabled with `allow: ["**"]` (`:12-25`) — broad asset scope so the frontend can load audio recordings and model files via `asset:`.
- **Bundle**: `active`, `createUpdaterArtifacts: true`, `targets: "all"`, **`resources: ["resources/**/*"]`** (bundles sounds, tray icons, `default_settings.json`, the Silero VAD model, GigaAM vocab) (`:26-31`).
- **macOS** (`:39-45`): `hardenedRuntime: true`, `minimumSystemVersion 10.15`, `signingIdentity "-"` (ad-hoc), `entitlements "Entitlements.plist"`.
- **Linux** (`:46-59`): deb depends on `libgtk-layer-shell0`; rpm compression `none`; appimage `bundleMediaFramework: true` (bundles GStreamer for WebKitGTK).
- **Windows** (`:60-65`): Azure Trusted Signing via `trusted-signing-cli`, custom `nsis/installer.nsi` template.
- **Updater** (`:67-74`): minisign `pubkey` + endpoint `https://github.com/cjpais/Handy/releases/latest/download/latest.json`.

---

## 5. Frontend build (`package.json`, `vite.config.ts`)

- **Scripts** (`package.json:6-21`): `dev` (vite), `build` (`tsc && vite build`), `tauri`, `lint`, `format` (prettier + `cargo fmt`), `test:playwright`, `check:translations`, and **`postinstall: "bun scripts/check-nix-deps.ts"`** — the Nix-sync hook fires on every `bun install`.
- **Vite** (`vite.config.ts`): React + Tailwind v4 plugins (`:10`); alias `@`→`./src` and `@/bindings` (`:13-18`); **two rollup entry points** `main: index.html` and `overlay: src/overlay/index.html` (`:21-28`) — the overlay window is a separate HTML/JS bundle; fixed dev `port 1420` / `strictPort` and HMR `1421` (`:35-45`); `watch.ignored: ["**/src-tauri/**"]` (`:46-49`).
- JS-side Tauri plugin bindings mirror the Rust plugins (`@tauri-apps/plugin-{autostart,clipboard-manager,dialog,fs,global-shortcut,opener,os,process,sql,store,updater}`) (`package.json:24-35`). Note `plugin-sql` is present on the JS side though the Rust side uses raw `rusqlite`.

---

## 6. Nix flake — reproducible Linux build (`flake.nix:1-249`)

### 6.1 Inputs & systems

- Inputs: `nixpkgs` (nixos-unstable) + **`bun2nix` 2.0.8** (`flake.nix:4-15`).
- `supportedSystems = ["x86_64-linux", "aarch64-linux"]` (`flake.nix:24-27`) — **Linux only; no Darwin output**. Version is read from `Cargo.toml` (`flake.nix:29-31`).

### 6.2 Shared native deps (`flake.nix:35-66`)

`commonNativeDeps` lists WebKitGTK 4.1, GTK3, glib, libsoup3, alsa-lib, **onnxruntime**, libayatana-appindicator, libevdev, libxtst, gtk-layer-shell, openssl, **vulkan-loader/headers, shaderc** (Vulkan shader compiler for the GPU backend). `gstPlugins` adds the GStreamer set for WebKitGTK media. `commonEnv` sets `ORT_LIB_LOCATION`, `ORT_PREFER_DYNAMIC_LINK=1` (dynamic-link ONNX Runtime), and the GStreamer plugin search path.

### 6.3 The package (`flake.nix:89-182`)

`pkgs.rustPlatform.buildRustPackage` with:
- `cargoRoot`/`buildAndTestSubdir = "src-tauri"`, `tauriBundleType = "deb"` (`flake.nix:94-96`).
- `cargoLock.allowBuiltinFetchGit = true` (`flake.nix:98-105`) — auto-fetches the git deps (rdev, rodio, vad-rs, the Tauri fork) without manual output hashes.
- **`bunDeps = bun2nix.fetchBunDeps { bunNix = ./.nix/bun.nix; }`** (`flake.nix:137-139`) — per-package JS fetch.
- `nativeBuildInputs`: `cargo-tauri.hook`, `pkg-config`, `wrapGAppsHook4`, `bun`, `bun2nix.hook`, `jq`, `cmake`, `rustPlatform.bindgenHook`, `shaderc` (`flake.nix:141-153`).
- **`doCheck = false`** (`flake.nix:155-157`) — tests need audio devices / GPU / model files unavailable in the sandbox.
- `postPatch` (`flake.nix:107-131`) does four sandbox fix-ups: disables updater artifacts in `tauri.conf.json`, strips the `postinstall` hook from `package.json`, rewrites `libappindicator-sys` to the Nix store `.so` path, and neuters `ferrous-opencc`'s cbindgen call.
- `preFixup` (`flake.nix:168-173`) injects `WEBKIT_DISABLE_DMABUF_RENDERER=1` and an `ALSA_PLUGIN_DIR` pointing at a merged pipewire+alsa-plugins symlink (`flake.nix:80-86`).
- `env` sets `OPENSSL_NO_VENDOR=1` (`flake.nix:164-166`).

### 6.4 Modules & dev shell

- `nixosModules.default` → `nix/module.nix`; `homeManagerModules.default` → `nix/hm-module.nix` (`flake.nix:189-202`).
- `devShells.default` (`flake.nix:205-247`): rust toolchain + node/bun + cargo-tauri, exports the ORT/GST env, sets `LD_LIBRARY_PATH` for appindicator/onnxruntime/vulkan, runs `bun install` in `shellHook`.
- `nix/module.nix:43-45` adds the `KERNEL=="uinput", GROUP="input", MODE="0660"` udev rule needed for `rdev grab()`. `nix/hm-module.nix:27-39` defines a systemd user service (`ExecStart = ${pkg}/bin/handy`, `Restart on-failure`).

### 6.5 bun2nix sync chain

`scripts/check-nix-deps.ts` (`:1-85`): hashes `bun.lock`, compares to `.nix/bun-lock-hash`, regenerates `.nix/bun.nix` via `bunx bun2nix` on mismatch. Skips on Windows (`:34`), never blocks `bun install` for non-Nix devs (exits 0 on failure, `:75-77`). `.nix/bun.nix` is the generated `fetchurl` set (`flake.nix:133-139` consumes it).

---

## 7. Important types & public functions (with citations)

| Symbol | Signature / kind | What it does | Location |
| --- | --- | --- | --- |
| `CliArgs` | `#[derive(Parser, Clone, Default)] struct` with 6 `bool` flags | The CLI surface (`--start-hidden`, `--no-tray`, `--toggle-transcription`, `--toggle-post-process`, `--cancel`, `--debug`) | `cli.rs:3-29` |
| `run` | `pub fn run(cli_args: CliArgs)`; `#[cfg_attr(mobile, tauri::mobile_entry_point)]` | The whole Tauri app: plugin registration, specta command collection, setup, window/tray bootstrap. **`mobile_entry_point` attr is the only mobile hook present.** | `lib.rs:316-615` |
| `main` | `fn main()` | Parses `CliArgs`, sets Linux WEBKIT env, calls `run`. `windows_subsystem="windows"` in release. | `main.rs:1-18` |
| `initialize_core_logic` | `fn(app_handle: &AppHandle)` | Constructs the four managers, applies accelerator settings, builds the tray, configures autostart, creates overlay. | `lib.rs:140-295` |
| `apply_accelerator_settings` | `pub fn(app: &tauri::AppHandle)` | Translates persisted `whisper_accelerator`/`ort_accelerator`/`whisper_gpu_device` into `transcribe_rs::accel` global atomics. | `transcription.rs:738-769` |
| `get_available_accelerators` | `pub fn() -> AvailableAccelerators` | Reports which whisper/ORT backends were compiled in + enumerated GPU devices (drives the Advanced settings UI). Pre-warmed on a background thread at startup (`lib.rs:552-554`). | `transcription.rs:813-828` |
| `cached_gpu_devices` | `fn() -> &'static [GpuDeviceOption]` | Calls `transcribe_rs::whisper_cpp::gpu::list_gpu_devices()`, cached in `OnceLock`; **guards x86_64 with `is_x86_feature_detected!("fma")`** to avoid SIGILL on FMA3-less CPUs. | `transcription.rs:780-803` |
| `GpuDeviceOption` | `struct { id: i32, name: String, total_vram_mb: usize }` (`Serialize`,`Type`) | One selectable GPU device. | `transcription.rs:771-776` |
| `AvailableAccelerators` | `struct { whisper: Vec<String>, ort: Vec<String>, gpu_devices: Vec<GpuDeviceOption> }` | The accelerator capability report. | `transcription.rs:805-810` |
| `WhisperAcceleratorSetting` | `enum { Auto, Cpu, Gpu }` (default `Auto`) | Persisted whisper backend preference. | `settings.rs:279-291` |
| `OrtAcceleratorSetting` | `enum { Auto, Cpu, Cuda, DirectMl, Rocm }` (default `Auto`) | Persisted ONNX-Runtime execution-provider preference. | `settings.rs:293-305` |
| `portable::init` | `pub fn()` | Detects the `portable` marker file next to the exe, creates `Data/`, caches the dir in a `OnceLock`. Called first thing in `run` (`lib.rs:319`). | `portable.rs:15-46` |
| `portable::data_dir` | `pub fn() -> Option<&'static PathBuf>` | The portable data dir if active. Used to redirect logs (`lib.rs:459-468`) and webview cache (`lib.rs:522-524`). | `portable.rs:55-57` |
| `generate_tray_translations` | `fn()` (build script) | Codegens `TrayStrings` + `TRANSLATIONS` map from locale JSON. | `build.rs:14-92` |
| `build_apple_intelligence_bridge` | `fn()` (build script, macOS aarch64) | Compiles + links the Swift FoundationModels bridge. | `build.rs:114-254` |
| `is_apple_intelligence_available` | `@_cdecl` Swift `-> Int32` | Whether on-device Apple Intelligence is usable. | `swift/apple_intelligence.swift:38-51` |
| `process_text_with_system_prompt_apple` | `@_cdecl` Swift `-> *mut AppleLLMResponse` | Runs the on-device LLM with a system prompt + user content (the summarization/cleanup primitive). | `swift/apple_intelligence.swift:53-129` |

---

## 8. Threading / concurrency in this subsystem

The build subsystem is mostly declarative, but the runtime bootstrap (`lib.rs`) and accelerator code spawn threads:

- **GPU pre-warm thread**: `std::thread::spawn(|| { let _ = get_available_accelerators(); })` (`lib.rs:552-554`) — loads the Metal/Vulkan backend and probes devices off the UI thread; result cached in `GPU_DEVICES: OnceLock` (`transcription.rs:778`).
- **Tray model-switch thread**: tray `model_select:` events spawn a thread to call `switch_active_model` then refresh the menu (`lib.rs:247-258`).
- **Unix signal thread**: `signal_handle::setup_signal_handler` consumes a `Signals` iterator for `SIGUSR1/2` (`lib.rs:173-177`).
- **`FILE_LOG_LEVEL: AtomicU8`** (`lib.rs:51`) — lock-free shared state read by the file-log target filter (`lib.rs:469-472`).
- **`accel` global atomics** in `transcribe-rs` — `apply_accelerator_settings` writes them via `set_whisper_accelerator` / `set_ort_accelerator` (`transcription.rs:748-767`); they are read when a model loads.
- **Swift bridge**: `process_text_with_system_prompt_apple` runs the model in `Task.detached` and blocks the calling (Rust) thread on a `DispatchSemaphore` (`swift/apple_intelligence.swift:80-118`) — i.e. the Rust caller must invoke it from a worker thread, not the main thread.
- **`bun install`** in the Nix `shellHook` and the `postinstall` hook spawn subprocesses (`Bun.spawnSync` in `check-nix-deps.ts:60`).

---

## 9. Data flow IN / OUT of the build subsystem

**IN (drives the build):**
- `Cargo.toml` target-cfg → compiler selects GPU backend features.
- `tauri.conf.json` → `tauri_build::build()` embeds config + capabilities.
- `src/i18n/locales/*/translation.json` → `build.rs` → `$OUT_DIR/tray_translations.rs`.
- `swift/*.swift` + SDK presence → `build.rs` → `libapple_intelligence.a` + link args.
- `bun.lock` → `check-nix-deps.ts` → `.nix/bun.nix` → `flake.nix` `fetchBunDeps`.
- `Cargo.toml package.version` → `flake.nix` (`fromTOML`) and `tauri.conf.json version`.

**OUT (build products & runtime config):**
- `src-tauri/target/release/handy` binary + `bundle/{dmg,deb,rpm,appimage,nsis}` installers (`BUILD.md:92`).
- `src/bindings.ts` — specta-generated TS, written only in debug builds (`lib.rs:432-438`).
- Updater artifacts + `latest.json` signed with the minisign key (`tauri.conf.json:67-74`).
- At runtime: persisted settings via `tauri-plugin-store` (portable-aware path, `portable.rs:86-92`), logs to LogDir or `Data/logs` (`lib.rs:459-468`), models under the app-data models dir.

**Message/event types crossing the boundary:** frontend ⇄ backend via the ~90 specta `collect_commands!` handlers (`lib.rs:325-429`) and the `HistoryUpdatePayload` event (`lib.rs:430`). Accelerator-relevant commands: `change_whisper_accelerator_setting`, `change_ort_accelerator_setting`, `change_whisper_gpu_device`, `get_available_accelerators` (`lib.rs:371-374`).

---

## 10. Error handling & edge cases

- **`build.rs` panics hard** on missing locale JSON, missing English `tray` section, missing Swift source, or `swiftc`/`libtool` failure (`build.rs:38,47,160,216,234`) — a broken locale file fails the whole build. Acceptable for a build script (fail fast).
- **FMA3 SIGILL guard** (`transcription.rs:788-792`): GPU enumeration is skipped on x86_64 CPUs without FMA3 because ggml's Vulkan backend would SIGILL uncatchably.
- **Weak-linking FoundationModels** (`build.rs:248-251`) so the binary launches on older macOS; the Swift stub path (`build.rs:154-158`) covers SDKs without the framework.
- **Portable marker validation** (`portable.rs:96-100`): only a marker containing the magic string `"Handy Portable Mode"` (with whitespace tolerance) counts; a legacy empty marker is upgraded **only if** a `Data/` dir already exists (`portable.rs:23-34`) — prevents Scoop's empty-file false positive (test at `portable.rs:145-154`). Six unit tests cover these branches (`portable.rs:107-165`).
- **Nix sandbox fix-ups** in `postPatch` (`flake.nix:107-131`) handle three third-party build failures (libappindicator path, ferrous-opencc cbindgen, updater artifacts) that would otherwise break the sandboxed build.
- **AppImage/strip toolchain bug** on rolling-release distros documented with a `--bundles deb` workaround (`BUILD.md:118-145`).
- **Intel-Mac ORT**: no prebuilt ONNX Runtime → must `brew install onnxruntime` and set `ORT_LIB_LOCATION`/`ORT_PREFER_DYNAMIC_LINK` (`BUILD.md:20-33`).
- `check-nix-deps.ts` deliberately exits 0 on `bun2nix` failure so non-Nix devs aren't blocked (`:75-77`).

---

## 11. State & persistence touched by the build subsystem

- **Bundled at build time** (`tauri.conf.json:30`, `resources/`): `default_settings.json`, feedback WAVs (`marimba_*`, `pop_*`), tray PNGs, `models/silero_vad_v4.onnx`, `models/gigaam_vocab.txt`, overlay PNGs. The Silero VAD model **must be present** for dev (`AGENTS.md` curls it from `blob.handy.computer`).
- **SQLite**: `rusqlite` `bundled` (compiled in) + `rusqlite_migration` — history DB lives under app-data (managed by `HistoryManager`, outside this doc's scope but built here).
- **Settings store**: `tauri-plugin-store`, path resolved through `portable::store_path` (`portable.rs:86-92`).
- **Models on disk**: downloaded at runtime from `https://blob.handy.computer/*` (`model.rs:132-593`) — **not bundled** except Silero VAD + GigaAM vocab. Whisper `.bin`, Parakeet/Moonshine/SenseVoice/GigaAM/Canary/Cohere `.tar.gz` (ONNX).
- **Logs**: rotated single-file (`max_file_size 500_000`, `KeepOne`) to LogDir or portable `Data/logs` (`lib.rs:447-473`).
- **`.nix/bun-lock-hash`** persists the last-synced lock hash.

---

## 12. Platform-specific branches (cfg-gate inventory)

| Concern | Gate | Location |
| --- | --- | --- |
| Apple-Intelligence Swift module | `cfg(all(macos, aarch64))` | `lib.rs:2-3`, `build.rs:2-3,114` |
| NSPanel overlay plugin | `cfg(target_os="macos")` | `lib.rs:477-480`; dep `Cargo.toml:101-102` |
| GTK layer-shell overlay | (Linux dep) | `Cargo.toml:105-107` |
| Vulkan whisper backend | `cfg(windows)` + `cfg(linux)` | `Cargo.toml:91,108` |
| Metal whisper backend | `cfg(macos)` | `Cargo.toml:103` |
| DirectML ORT backend | `cfg(windows)` | `Cargo.toml:91` |
| `windows` crate + `winreg` | `cfg(windows)` | `Cargo.toml:90-99` |
| Desktop plugins (autostart/global-shortcut/single-instance/updater) | `cfg(not(any(android, ios)))` | `Cargo.toml:84-88` |
| `signal-hook` | `cfg(unix)` | `Cargo.toml:81-82`; usage `lib.rs:33-36,173-177` |
| Linux DMABUF disable | `cfg(target_os="linux")` | `main.rs:10-15` (also Nix `preFixup`, `flake.nix:168-173`) |
| macOS activation-policy juggling | `cfg(target_os="macos")` | `lib.rs:98-103,181-187,581-596,608-612` |
| Windows mic-permission onboarding | `cfg(target_os="windows")` | `lib.rs:116-135` |
| FMA3 SIGILL guard | `cfg(target_arch="x86_64")` | `transcription.rs:788` |
| Release console suppression | `cfg_attr(not(debug_assertions), windows_subsystem="windows")` | `main.rs:2` |
| TS bindings export | `cfg(debug_assertions)` | `lib.rs:432-438` |
| `mobile_entry_point` | `cfg_attr(mobile, ...)` | `lib.rs:316` |

**iOS reality check:** the only iOS/Android presence is (1) the `cfg(not(any(android, ios)))` *exclusion* of desktop plugins (`Cargo.toml:84`), (2) the `mobile_entry_point` attribute (`lib.rs:316`), (3) `gen/apple/PrivacyInfo.xcprivacy`, and (4) icon sets in `icons/{android,ios}/`. **There is no `tauri ios`/`tauri android` config, no `gen/apple/*.xcodeproj`, no mobile capability, and no mobile audio/permission code.** Mobile is scaffolding-only.

---

## 13. PLAUD RELEVANCE — concrete extension points

A Plaud-style product = always-on capture (mic + system/call audio) → long-form recording → diarization → AI summary → sync across desktop + phone. Mapping that onto this build subsystem:

1. **On-device AI summaries (already half-built).** `swift/apple_intelligence.swift` + the `build.rs` bridge (`build.rs:114-254`) give you a local LLM on Apple Silicon. For Plaud summaries, **wrap `process_text_with_system_prompt_apple`** (`swift/apple_intelligence.swift:53-129`) with a "summarize meeting transcript" system prompt and a higher `maxTokens`. The C ABI (`swift/apple_intelligence_bridge.h`) and the `check_apple_intelligence_available` command (`lib.rs:389`) are the entry points. There is also a generic `llm_client` module (`lib.rs:11`) + post-process settings (`set_post_process_provider`, `fetch_post_process_models`, `lib.rs:354-355`) for cloud LLM summaries — extend `change_post_process_base_url_setting` to support your summary backend.

2. **GPU backend for long-form transcription.** Long recordings need the GPU path. The selection is the per-target `transcribe-rs` feature matrix (`Cargo.toml:90-108`) plus `apply_accelerator_settings` (`transcription.rs:738-769`). To add CoreML on Apple Silicon (currently absent), add a `coreml`/`ort-coreml` feature to the `cfg(target_os="macos")` block and a new `OrtAcceleratorSetting::CoreMl` variant (`settings.rs:293-305`).

3. **System/call audio capture.** This is *not* a build concern per se, but the build gates control it: on macOS you'd add a ScreenCaptureKit / CoreAudio tap, which requires **new entitlements** in `Entitlements.plist` (currently only `device.microphone` + `device.audio-input`, `Entitlements.plist:5-8`) — e.g. `com.apple.security.device.audio-input` is present but you'd add screen-recording entitlement + `NSScreenCaptureUsageDescription` to `Info.plist` (currently only `NSMicrophoneUsageDescription`, `Info.plist:5-6`). On Windows, the `Win32_Media_Audio_Endpoints` feature is already enabled (`Cargo.toml:92-98`) — extend the audio manager to open a loopback (WASAPI render) endpoint.

4. **Bundling diarization / larger models.** Diarization needs a speaker-embedding model (e.g. pyannote/ONNX). Add it the way Silero VAD is handled: either bundle it under `resources/models/` (`tauri.conf.json:30`) or add a download entry in `model.rs` (`model.rs:132-593`) pointing at your blob host. The ONNX execution provider is already wired (`OrtAcceleratorSetting`, `Cargo.toml onnx`/`ort-directml`/CoreML features).

5. **Auto-start / always-on.** Desktop autostart already exists via `tauri-plugin-autostart` (`lib.rs:281-291,503-506`) and the Linux systemd user service (`nix/hm-module.nix:27-39`). For a recorder that survives sleep/reboot this is the hook.

6. **Cloud/local sync.** No sync exists. The portable-mode `Data/` directory (`portable.rs`) and the SQLite history DB are the natural sync roots. You'd add a sync manager alongside the existing managers in `initialize_core_logic` (`lib.rs:147-166`) and a capability/permission entry. `reqwest` (json+stream) is already a dependency (`Cargo.toml:61`).

7. **Mobile (iPhone).** The crate is *prepared* for mobile (`crate-type` staticlib/cdylib at `Cargo.toml:20`, `mobile_entry_point` at `lib.rs:316`, iOS icons + xcprivacy), but **nothing is wired**. To start: run `tauri ios init`, add an `[target.'cfg(target_os = "ios")']` deps block in `Cargo.toml`, provide an iOS audio backend (cpal has limited iOS support — likely need an AVAudioEngine bridge à la the Swift bridge pattern in `build.rs`), and gate out every desktop-only plugin (already excluded at `Cargo.toml:84`). The transcription engine would need a CoreML/Metal iOS build of `transcribe-rs` — a major upstream dependency change.

---

## 14. GAPS vs a Plaud-style product

- **No iOS/Android build at all.** Only icons + a privacy manifest + the `mobile_entry_point` attribute exist. No `tauri.{ios,android}.conf`, no Xcode project, no mobile audio/permissions, and the Nix flake is Linux-only (`flake.nix:24-27`). The entire phone half of "macOS + iPhone" is greenfield.
- **No system/call-audio capture entitlements.** `Entitlements.plist` only grants microphone; no ScreenCaptureKit / process-audio-tap / Bluetooth-call capture. Plaud's core capture mode is absent at the packaging layer.
- **No speaker diarization model bundled or downloadable.** Only Silero VAD + ASR models in `model.rs`. No diarization/speaker-embedding pipeline.
- **No cloud sync / account / backup.** Everything is local-first to one machine; no sync manager, no server, no cross-device story. The portable `Data/` dir is single-machine.
- **Single private artifact host.** All models come from `https://blob.handy.computer` (`model.rs:132-593`) and the updater from `github.com/cjpais/Handy` (`tauri.conf.json:71`). A productized fork needs its own resilient, possibly mirrored, distribution.
- **macOS CoreML not selected.** macOS only compiles the Metal *whisper* backend (`Cargo.toml:103`); ONNX models on Mac fall back to CPU (no CoreML execution provider in the feature set), which hurts the ONNX model family (Parakeet/Canary/etc.) on long recordings.
- **Forked-Tauri supply-chain pin.** Three Tauri crates are pinned to `cjpais/tauri` `handy-2.10.2` (`Cargo.toml:110-113`); upstreaming or maintaining that fork is required for long-term reproducibility.
- **Ad-hoc macOS signing + no notarization config.** `signingIdentity: "-"` (`tauri.conf.json:43`) is ad-hoc; there's no notarization/Developer-ID/App-Store packaging — a shippable consumer product (especially on iPhone) needs a real signing + notarization pipeline.
- **No CI matrix in-repo** for the GPU backends (no `.github/workflows` referenced here); reproducibility for macOS/Windows builds relies on `BUILD.md` manual steps rather than codified pipelines.
