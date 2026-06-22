# 07 — Text Post-Processing, Custom Words, Filler Removal, Chinese Variant, Apple Intelligence + LLM Client

> **Abstract.** This subsystem is the "text-cleanup and rewriting" stage that sits *after* the ASR engine produces raw transcript text and *before* that text is pasted into the active application. It has two physically distinct layers. The **deterministic, local, synchronous layer** lives in `audio_toolkit/text.rs` and runs inside the transcription worker thread: fuzzy custom-word correction (Levenshtein + Soundex + n-gram), filler-word removal, and stutter collapse. The **probabilistic, optionally-remote, asynchronous layer** lives in `actions.rs` + `llm_client.rs` + `apple_intelligence.rs` and runs in a Tokio task on the stop path: OpenCC Simplified↔Traditional Chinese conversion, plus LLM "post-processing" through an OpenAI-compatible HTTP client (OpenAI, Z.AI, OpenRouter, Anthropic, Groq, Cerebras, AWS Bedrock-Mantle, a Custom/local endpoint) or via on-device **Apple Intelligence** through a Swift FFI bridge. The deterministic layer always runs; the LLM layer is gated behind a separate shortcut/flag (`transcribe_with_post_process`) and per-provider settings. This document maps every struct, function, thread boundary, settings key, and platform `cfg` gate, then identifies the concrete hooks a Plaud-style product would extend — speaker-aware summaries, long-form chunking, conversation persistence, and mobile.

---

## 1. File-by-file responsibility

| Path | Responsibility |
|---|---|
| `src-tauri/src/audio_toolkit/text.rs` | Pure, deterministic, **local** text cleanup: fuzzy custom-word correction (Levenshtein + Soundex + n-gram), language-aware filler-word stripping, stutter collapse, whitespace normalization. No I/O, no async, no network. Fully unit-tested in-file. |
| `src-tauri/src/actions.rs` | Shortcut-action orchestration. Owns the recording→transcription→**post-processing**→paste pipeline. Contains the orchestrator `process_transcription_output`, the LLM dispatch `post_process_transcription`, and the OpenCC `maybe_convert_chinese_variant`. Routes to either the OpenAI-compatible HTTP path or the Apple Intelligence native path. |
| `src-tauri/src/llm_client.rs` | Thin async HTTP client for **OpenAI-compatible** `/chat/completions` and `/models` endpoints. Builds provider-specific auth headers, supports JSON-schema structured output and two reasoning-control dialects (OpenAI top-level `reasoning_effort`, OpenRouter nested `reasoning`). |
| `src-tauri/src/apple_intelligence.rs` | Rust↔Swift FFI surface for on-device Apple Intelligence (FoundationModels). Declares the C ABI, wraps the unsafe pointer handling, and owns memory cleanup. |
| `src-tauri/swift/apple_intelligence.swift` | Real Swift implementation (compiled only when the SDK ships `FoundationModels.framework`). Calls `SystemLanguageModel.default` / `LanguageModelSession`. |
| `src-tauri/swift/apple_intelligence_stub.swift` | Stub compiled when `FoundationModels.framework` is absent; every entry point reports "not available". |
| `src-tauri/build.rs` (`build_apple_intelligence_bridge`, line 115) | Detects `FoundationModels.framework` in the SDK, compiles real-or-stub Swift to a static lib, weak-links the framework. Gated `#[cfg(all(target_os = "macos", target_arch = "aarch64"))]`. |
| `src-tauri/src/settings.rs` | Persisted configuration backing the whole subsystem: provider catalog (`PostProcessProvider`), prompt catalog (`LLMPrompt`), API-key secret map (`SecretMap`), custom-word/threshold/filler-word fields, language fields. |
| `src-tauri/src/shortcut/mod.rs` (≈ lines 797–1075) | Tauri command handlers the frontend calls to mutate the above settings and to `fetch_post_process_models`. |
| `src-tauri/src/managers/transcription.rs` (≈ lines 685–722) | The **call site** of the deterministic layer (`apply_custom_words`, `filter_transcription_output`) inside the transcription worker. |
| `src-tauri/src/audio_toolkit/mod.rs` (lines 3, 11) | Re-exports `apply_custom_words` and `filter_transcription_output`. |

---

## 2. Two pipelines, two threading models

There are **two separate clean-up stages** running on **two different threads**, and they must not be confused.

### 2.1 Deterministic local stage (synchronous, inside the transcription worker)

Called from `TranscriptionManager::transcribe` in `managers/transcription.rs`:

- `apply_custom_words(&result.text, &settings.custom_words, settings.word_correction_threshold)` — `transcription.rs:694`
- `filter_transcription_output(&corrected_result, &settings.app_language, &settings.custom_filler_words)` — `transcription.rs:704`

Important nuances at the call site:

- **Custom words are skipped for Whisper models** (`transcription.rs:687–701`): Whisper receives the custom words as an `initial_prompt` instead, so applying fuzzy correction afterward would double-correct. The branch only runs `apply_custom_words` when `!settings.custom_words.is_empty() && !is_whisper` (i.e., Parakeet and other non-Whisper engines).
- Filler filtering uses **`settings.app_language`** (the UI/app language) — *not* `settings.selected_language` (the ASR language). This is a meaningful distinction the Chinese stage below does *not* share.
- This stage is **blocking**, runs on whatever thread `TranscriptionManager::transcribe` is invoked on (the async transcription task spawned in `TranscribeAction::stop`), and has no network or `await`.

### 2.2 Probabilistic / network stage (async Tokio task, on the stop path)

`TranscribeAction::stop` (`actions.rs:492`) spawns `tauri::async_runtime::spawn(async move { … })` (`actions.rs:516`). Inside it, after `tm.transcribe(samples)` returns, it calls:

```
process_transcription_output(&ah, &transcription, post_process).await    // actions.rs:585
```

`process_transcription_output` (`actions.rs:349`) is the orchestrator of stage 2:

1. `maybe_convert_chinese_variant(&settings, transcription).await` (`actions.rs:359`) — OpenCC zh-Hans↔zh-Hant.
2. If `post_process` (the flag carried by the chosen shortcut action), `post_process_transcription(&settings, &final_text).await` (`actions.rs:364`) — the LLM rewrite.

This stage runs entirely on the Tokio runtime; network calls in `llm_client.rs` are `reqwest` async. The **Apple Intelligence path is the exception**: it is a *blocking* synchronous FFI call (`apple_intelligence::process_text_with_system_prompt`, `actions.rs:162`) executed directly inside the async task — there is **no `spawn_blocking`**, so a slow on-device model will block the Tokio worker (see Gaps §11).

A `FinishGuard` (`actions.rs:32`) drop-guard guarantees the `TranscriptionCoordinator` is notified even if the task panics anywhere in stage 2.

---

## 3. `audio_toolkit/text.rs` — deterministic cleanup in depth

### 3.1 Custom-word correction

| Fn | Signature | What it does | Citation |
|---|---|---|---|
| `build_ngram` | `fn build_ngram(words: &[&str]) -> String` | Strips non-alphanumeric chars per word, lowercases, concatenates **without spaces**. Lets `"Charge B"` collapse to `"chargeb"` to match `"ChargeBee"`. | `text.rs:10` |
| `find_best_match` | `fn find_best_match<'a>(candidate: &str, custom_words: &'a [String], custom_words_nospace: &[String], threshold: f64) -> Option<(&'a String, f64)>` | Core matcher. Rejects empty or >50-char candidates. For each custom word: skips if length differs by more than `max(25% of longer, 2)` chars; computes normalized Levenshtein (`dist / max_len`); computes Soundex phonetic equality; if phonetic match, multiplies Levenshtein score by `0.3` (favoring phonetics); accepts when `combined_score < threshold && < best_score`. Returns lowest-scoring (best) match. | `text.rs:34` |
| `apply_custom_words` | `pub fn apply_custom_words(text: &str, custom_words: &[String], threshold: f64) -> String` | Public entry. Pre-computes lowercase + space-stripped variants. Walks words left-to-right with **greedy n-gram matching from n=3 down to n=1**; on a hit, re-applies original prefix/suffix punctuation via `extract_punctuation`, re-applies the source word's case via `preserve_case_pattern`, advances by `n`. | `text.rs:102` |
| `preserve_case_pattern` | `fn preserve_case_pattern(original: &str, replacement: &str) -> String` | ALL-CAPS → uppercase replacement; Title-case → capitalize first char; else as-is. | `text.rs:159` |
| `extract_punctuation` | `fn extract_punctuation(word: &str) -> (&str, &str)` | Returns leading and trailing non-alphanumeric runs (so `"!hello?"` → `("!","?")`). | `text.rs:174` |

**Scoring semantics:** `threshold` is a *maximum* score where `0.0` = exact and higher = looser. The frontend exposes it as `word_correction_threshold` (default in `settings.rs:478`). A phonetic match gets a 70% score discount, so homophones match aggressively.

**Edge cases handled:** empty custom list returns input unchanged (`text.rs:103`); candidate length guard (`>50` rejected) prevents pathological cost; the 25%/2-char length gate prevents `"openaigpt"` from matching `"openai"`; the trailing-punctuation re-extraction is specifically tested to avoid double-counting (`test_apply_custom_words_trailing_number_not_doubled`, `text.rs:553`).

### 3.2 Filler removal & stutter collapse

| Fn | Signature | What it does | Citation |
|---|---|---|---|
| `get_filler_words_for_language` | `fn get_filler_words_for_language(lang: &str) -> &'static [&'static str]` | Maps a base language code (splits on `-`/`_`) to a curated filler list. **Language-aware**: English includes `um`, `eh`, `ha`; Portuguese omits `um` (= "a/an"); Spanish omits `ha` (= "has"). Unknown languages fall to a conservative list that excludes `um`/`eh`/`ha`. | `text.rs:202` |
| `collapse_stutters` | `fn collapse_stutters(text: &str) -> String` | Collapses **3+** consecutive case-insensitive repeats of the *same alphabetic* word to a single instance (`"wh wh wh"` → `"wh"`). 2 repeats are preserved. Non-alphabetic tokens never collapse. | `text.rs:236` |
| `filter_transcription_output` | `pub fn filter_transcription_output(text: &str, lang: &str, custom_filler_words: &Option<Vec<String>>) -> String` | Public entry. Builds case-insensitive `\b{word}\b[,.]?` regexes (with `regex::escape`) from either the custom list or the language defaults, strips them, then `collapse_stutters`, then collapses multiple spaces via the `MULTI_SPACE_PATTERN` (`text.rs:232`), then trims. | `text.rs:288` |

**Tri-state filler semantics** (`text.rs:296`):
- `None` → use language defaults for `lang`.
- `Some(non-empty)` → use exactly the user list (overrides defaults).
- `Some(empty)` → disable filtering entirely (tested at `text.rs:488`).

**Concurrency:** none. One `once_cell::sync::Lazy<Regex>` static (`MULTI_SPACE_PATTERN`) is the only shared state; everything else is stack-local. Pure functions, trivially `Send`.

**State/persistence touched:** none directly — it consumes `AppSettings` fields passed by the caller.

---

## 4. `actions.rs` — orchestration & LLM dispatch in depth

### 4.1 Helpers

| Item | Signature | Citation |
|---|---|---|
| `TRANSCRIPTION_FIELD` | `const TRANSCRIPTION_FIELD: &str = "transcription"` — JSON-schema field name for structured output | `actions.rs:53` |
| `strip_invisible_chars` | `fn strip_invisible_chars(s: &str) -> String` — removes `U+200B/C/D` and `U+FEFF` (zero-width + BOM) that LLMs sometimes inject | `actions.rs:56` |
| `build_system_prompt` | `fn build_system_prompt(prompt_template: &str) -> String` — strips the `${output}` placeholder and trims, since in structured mode the transcript is sent as the *user* message | `actions.rs:62` |

### 4.2 `post_process_transcription` (the LLM gateway) — `actions.rs:66`

```
async fn post_process_transcription(settings: &AppSettings, transcription: &str) -> Option<String>
```

Resolution order (each step returns `None` to "skip and keep prior text"):
1. Resolve active provider via `settings.active_post_process_provider()` (`settings.rs:821`).
2. Resolve model from `settings.post_process_models[provider.id]`; empty → skip (`actions.rs:81`).
3. Resolve selected prompt id from `settings.post_process_selected_prompt_id`; find it in `settings.post_process_prompts`; empty prompt → skip.
4. Resolve API key from `settings.post_process_api_keys[provider.id]` (the `SecretMap`).
5. **Reasoning suppression** (`actions.rs:132`): `custom` provider → top-level `reasoning_effort = "none"`; `openrouter` → nested `ReasoningConfig{ effort:"none", exclude:true }` (the `exclude` also keeps chain-of-thought out of the JSON so it can't corrupt structured-output parsing); all others → none.
6. **Branch A — structured output** (`provider.supports_structured_output`, `actions.rs:144`):
   - **Apple Intelligence sub-branch** (`provider.id == APPLE_INTELLIGENCE_PROVIDER_ID`, `actions.rs:151`): `cfg(macos+aarch64)` only. Checks `check_apple_intelligence_availability()`; parses the "model" string as an `i32` token limit (`actions.rs:161`); calls `process_text_with_system_prompt(system_prompt, user_content, token_limit)`. On non-macOS-aarch64 it returns `None`.
   - **HTTP sub-branch**: builds a strict JSON schema `{ transcription: string }` (`actions.rs:195`) and calls `send_chat_completion_with_schema(...)`. On `Ok(Some(content))` it parses JSON and extracts `transcription`; if the field is missing or JSON is malformed it falls back to returning the raw content (still stripped of invisibles). On `Err` it logs a warning and **falls through to legacy mode** (`actions.rs:251`).
7. **Branch B — legacy mode** (`actions.rs:261`): substitutes `${output}` with the transcript inside the prompt and calls `send_chat_completion(...)` (no schema). Returns stripped content or `None`.

**Error philosophy:** every failure path returns `None`, and the caller treats `None` as "use the pre-LLM text". The LLM stage is therefore **best-effort and non-fatal** — a dead API key or offline endpoint silently yields the deterministic-only transcript.

### 4.3 `maybe_convert_chinese_variant` — `actions.rs:299`

```
async fn maybe_convert_chinese_variant(settings: &AppSettings, transcription: &str) -> Option<String>
```

- Triggers only when `settings.selected_language` is `"zh-Hans"` or `"zh-Hant"` (note: **`selected_language`**, the ASR language — distinct from the `app_language` used by filler filtering).
- Uses `ferrous_opencc::OpenCC` with `BuiltinConfig::Tw2sp` (Traditional→Simplified) for `zh-Hans`, or `BuiltinConfig::S2tw` (Simplified→Traditional) for `zh-Hant` (`actions.rs:318`).
- `OpenCC::from_config` failure → logged, returns `None` (keep original). This is fully **local/offline**; OpenCC dictionaries are bundled by the `ferrous-opencc` crate.

### 4.4 `process_transcription_output` — `actions.rs:349`

```
pub(crate) async fn process_transcription_output(app: &AppHandle, transcription: &str, post_process: bool) -> ProcessedTranscription
```

Returns `ProcessedTranscription { final_text, post_processed_text: Option<String>, post_process_prompt: Option<String> }` (`actions.rs:343`). Logic:
- Start `final_text = transcription`.
- Apply Chinese conversion if any.
- If `post_process`, run the LLM gateway; on success, record both `post_processed_text` and the resolved `post_process_prompt` (for history).
- Else, if Chinese conversion changed the text, record that as `post_processed_text` too (`actions.rs:378`).

**Consumers of the result** (`actions.rs:585–628`): the final text is pasted via `utils::paste` on the **main thread** (`run_on_main_thread`), and a history row is written via `HistoryManager::save_entry(file_name, raw_transcription, post_process, post_processed_text, post_process_prompt)` (`actions.rs:591`). So the subsystem's output is persisted: raw transcript, the post-processed variant, and the prompt used.

### 4.5 `post_process` flag origin

The boolean is baked into the action at registration in `ACTION_MAP` (`actions.rs:700`): `"transcribe"` → `post_process:false` (`actions.rs:705`), `"transcribe_with_post_process"` → `post_process:true` (`actions.rs:710`). The CLI flag `--toggle-post-process` and a dedicated shortcut both route to the latter (`lib.rs:487`).

---

## 5. `llm_client.rs` — OpenAI-compatible HTTP client in depth

### 5.1 Wire types

| Type | Role | Citation |
|---|---|---|
| `ChatMessage { role, content }` | one chat turn (serialize) | `llm_client.rs:8` |
| `JsonSchema { name, strict, schema }` | structured-output schema wrapper | `llm_client.rs:14` |
| `ResponseFormat { type: "json_schema", json_schema }` | OpenAI structured-output envelope | `llm_client.rs:21` |
| `ReasoningConfig { effort: Option<String>, exclude: Option<bool> }` | OpenRouter-style nested reasoning control; `skip_serializing_if = Option::is_none` | `llm_client.rs:27` |
| `ChatCompletionRequest { model, messages, response_format?, reasoning_effort?, reasoning? }` | request body | `llm_client.rs:35` |
| `ChatCompletionResponse { choices: Vec<ChatChoice> }` → `ChatChoice { message }` → `ChatMessageResponse { content: Option<String> }` | response body (deserialize) | `llm_client.rs:47` |

### 5.2 Functions

| Fn | Signature | Notes | Citation |
|---|---|---|---|
| `build_headers` | `fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, String>` | Always sets `Content-Type`, `Referer`, `User-Agent: Handy/1.0`, `X-Title: Handy`. Auth is **provider-specific**: `anthropic` uses `x-api-key` + `anthropic-version: 2023-06-01`; everyone else uses `Authorization: Bearer …`. Empty key → no auth header (lets local `custom` servers work keyless). | `llm_client.rs:63` |
| `create_client` | `fn create_client(provider, api_key) -> Result<reqwest::Client, String>` | Builds a `reqwest::Client` with default headers. **No explicit timeout set** (see Gaps). | `llm_client.rs:100` |
| `send_chat_completion` | `pub async fn send_chat_completion(provider, api_key: String, model: &str, prompt: String, reasoning_effort, reasoning) -> Result<Option<String>, String>` | Convenience wrapper → `send_chat_completion_with_schema` with no system prompt / no schema. | `llm_client.rs:111` |
| `send_chat_completion_with_schema` | `pub async fn send_chat_completion_with_schema(provider, api_key: String, model: &str, user_content: String, system_prompt: Option<String>, json_schema: Option<Value>, reasoning_effort: Option<String>, reasoning: Option<ReasoningConfig>) -> Result<Option<String>, String>` | Builds `{base_url}/chat/completions`, assembles `[system?, user]` messages, wraps schema as `response_format` (`name:"transcription_output", strict:true`), POSTs JSON. Non-2xx → `Err(status+body)`. Returns first choice's `content`. | `llm_client.rs:137` |
| `fetch_models` | `pub async fn fetch_models(provider, api_key: String) -> Result<Vec<String>, String>` | GETs `{base_url}/models`; tolerant parser handling both `{data:[{id|name}]}` and bare `[ "model" ]` shapes. | `llm_client.rs:221` |

**Concurrency:** stateless async functions; each call builds a fresh `reqwest::Client`. No shared mutable state, no locks. Returns errors as `String` (stringly-typed, no typed error enum).

---

## 6. `apple_intelligence.rs` + Swift bridge — on-device LLM in depth

### 6.1 Rust FFI surface

| Item | Signature | Citation |
|---|---|---|
| `AppleLLMResponse` | `#[repr(C)] struct { response: *mut c_char, success: c_int, error_message: *mut c_char }` | `apple_intelligence.rs:6` |
| extern `is_apple_intelligence_available` | `fn() -> c_int` | `apple_intelligence.rs:14` |
| extern `free_apple_llm_response` | `fn(*mut AppleLLMResponse)` | `apple_intelligence.rs:15` |
| extern `process_text_with_system_prompt_apple` | `fn(*const c_char, *const c_char, i32) -> *mut AppleLLMResponse` | `apple_intelligence.rs:25` |
| `check_apple_intelligence_availability` | `pub fn () -> bool` (== 1) | `apple_intelligence.rs:19` |
| `process_text_with_system_prompt` | `pub fn (system_prompt: &str, user_content: &str, max_tokens: i32) -> Result<String, String>` | `apple_intelligence.rs:33` |

`process_text_with_system_prompt` converts the two `&str` to `CString` (NUL-error → `Err`), calls the Swift fn, null-checks the response pointer, reads `success`/`response`/`error_message` (lossy UTF-8), then **always** calls `free_apple_llm_response` before returning (`apple_intelligence.rs:70`). Memory ownership: Swift `strdup`s output and Rust hands the pointer back to Swift to `free` — no cross-allocator mismatch.

### 6.2 Swift implementation (`apple_intelligence.swift`)

- `@available(macOS 26.0, *)` everywhere; `@Generable struct CleanedTranscript { cleanedText }` is the structured-output target (`*.swift:5`).
- `isAppleIntelligenceAvailable` queries `SystemLanguageModel.default.availability` (`*.swift:38`).
- `processTextWithSystemPrompt` builds a `LanguageModelSession(model:instructions: systemPrompt)`, tries `session.respond(to: userContent, generating: CleanedTranscript.self)`; on failure **falls back** to an unstructured `session.respond(to:)` (`*.swift:98–107`). Optionally word-truncates to `maxTokens` via `truncatedText` (it truncates by **whitespace-split words**, not real tokens — `maxTokens` is a misnomer; `*.swift:25`).
- Bridges async→sync with a `DispatchSemaphore` and a `@unchecked Sendable ResultBox` (`*.swift:80–118`). This is what makes the Rust side blocking.

### 6.3 Platform gating

- **Build:** `build_apple_intelligence_bridge` is `#[cfg(all(target_os="macos", target_arch="aarch64"))]` (`build.rs:114`). It probes the SDK for `FoundationModels.framework` (`build.rs:147–149`): present → compile `apple_intelligence.swift`, absent → compile `apple_intelligence_stub.swift` (`build.rs:152–157`). Swift target is `arm64-apple-macosx11.0` (`build.rs:200`); the framework is **weak-linked** (`build.rs:248`) so binaries still launch on pre-macOS-26 systems.
- **Runtime registration:** the provider is added to the default catalog only under `cfg(macos+aarch64)` (`settings.rs:580`), and availability is **not** checked at startup (deliberately, to avoid a SIGABRT on macOS 26 beta — comment at `settings.rs:576`). The check is deferred to first use in `actions.rs:154`.
- The `"model"` value for Apple Intelligence is parsed as an integer token/word limit; default model id is the literal string `"Apple Intelligence"` (`settings.rs:11`, `APPLE_INTELLIGENCE_DEFAULT_MODEL_ID`).

---

## 7. Settings & persistence touched

Backing store is `tauri-plugin-store` (JSON on disk); access via `get_settings(app)`. Relevant fields (all in `settings.rs`):

| Field | Type | Default | Used by |
|---|---|---|---|
| `custom_words` | `Vec<String>` | `[]` | `apply_custom_words` (`settings.rs:373`) |
| `word_correction_threshold` | `f64` | `default_word_correction_threshold()` (`:478`) | `apply_custom_words` |
| `custom_filler_words` | `Option<Vec<String>>` | `None` | `filter_transcription_output` (`:424`) |
| `app_language` | `String` | `default_app_language()` (`:510`) | filler filtering |
| `selected_language` | `String` | `"auto"` (`:782`) | Chinese conversion (`:303`) |
| `post_process_enabled` | `bool` | `default_post_process_enabled()` (`:506`) | UI gating |
| `post_process_provider_id` | `String` | `default_post_process_provider_id()` (`:520`) | active provider |
| `post_process_providers` | `Vec<PostProcessProvider>` | `default_post_process_providers()` (`:524`) | catalog |
| `post_process_api_keys` | `SecretMap` | `default_post_process_api_keys()` (`:615`) | auth |
| `post_process_models` | `HashMap<String,String>` | per-provider (`:630`) | model selection |
| `post_process_prompts` | `Vec<LLMPrompt>` | one default "Improve Transcriptions" prompt (`:641`) | prompt catalog |
| `post_process_selected_prompt_id` | `Option<String>` | `None` (`:801`) | active prompt |

**`PostProcessProvider`** (`settings.rs:96`): `{ id, label, base_url, allow_base_url_edit, models_endpoint: Option<String>, supports_structured_output }`. **`LLMPrompt`** (`settings.rs:90`): `{ id, name, prompt }`.

**Secret handling:** `SecretMap(HashMap<String,String>)` (`settings.rs:310`) overrides `Debug` to print `[REDACTED]` for non-empty values (`settings.rs:312`) so keys never leak into logs. It still serializes in cleartext to the on-disk store (no OS-keychain integration — see Gaps).

**Default provider catalog** (`settings.rs:524`): OpenAI, Z.AI, OpenRouter, Anthropic, Groq, Cerebras, *(Apple Intelligence on macOS-aarch64, `:582`)*, AWS Bedrock-Mantle (`:593`), Custom (`:603`, `http://localhost:11434/v1`, the only one with `allow_base_url_edit:true`). `supports_structured_output` is `true` for OpenAI/Z.AI/OpenRouter/Cerebras/Bedrock/Apple, `false` for Anthropic/Groq/Custom — which forces those three down the legacy `${output}` path.

**Migration:** `ensure_post_process_defaults` (`settings.rs:657`) re-syncs `supports_structured_output` and back-fills new providers/keys/models for users upgrading from older versions.

**History persistence:** the LLM output and prompt are written to the history DB via `HistoryManager::save_entry` (`actions.rs:591`), so both raw and processed text are retained.

---

## 8. Frontend ↔ backend command surface

Tauri commands (registered in `lib.rs:349–359, 389`), implemented in `shortcut/mod.rs`:

- `change_post_process_enabled_setting(enabled)` — `:797`
- `change_post_process_base_url_setting(...)` — `:829`
- `change_post_process_api_key_setting(...)` — `:873`
- `change_post_process_model_setting(...)` — `:887`
- `set_post_process_provider(provider_id)` — `:901`
- `add_post_process_prompt(...)` / `update_post_process_prompt(...)` / `delete_post_process_prompt(id)` — `:911 / :935 / :959`
- `set_post_process_selected_prompt(id)` — `:1032`
- `fetch_post_process_models(app, provider_id)` — `:987` — async; short-circuits Apple Intelligence to return `["Apple Intelligence"]` on macOS-aarch64 or an error elsewhere; for everyone else requires a key (except `custom`) and delegates to `llm_client::fetch_models` (`:1027`).
- `check_apple_intelligence_available()` — `commands/mod.rs:119`, returns the FFI bool on macOS-aarch64, else `false`.

**Event/message types out:** `recording-error` (`RecordingErrorEvent`, `actions.rs:24`), `paste-error`, `model-state-changed` — none specific to post-processing other than the error surfaces.

---

## 9. Data flow summary (IN → OUT)

```
ASR engine (Whisper/Parakeet) ── raw text ──▶ TranscriptionManager::transcribe
   │  (synchronous, worker thread)
   ├─ apply_custom_words           (skipped for Whisper)           [text.rs]
   └─ filter_transcription_output  (app_language fillers+stutter)  [text.rs]
        │ returns cleaned text
        ▼
TranscribeAction::stop ── spawns Tokio task ──▶ process_transcription_output  [actions.rs]
   ├─ maybe_convert_chinese_variant  (OpenCC, local, selected_language)
   └─ post_process_transcription  (only if post_process flag)
         ├─ Apple Intelligence  ─FFI▶ Swift FoundationModels  (blocking, on-device)
         └─ HTTP  ─▶ llm_client::send_chat_completion[_with_schema]  ─▶ provider API
        │ returns final_text (+ post_processed_text, prompt)
        ▼
   HistoryManager::save_entry   (persist raw + processed + prompt)
   utils::paste  (main thread)  ─▶ active application
```

**Calls IN:** the ASR pipeline (`transcription.rs`) and shortcut actions (`actions.rs`). **Calls OUT:** `ferrous_opencc`, `reqwest` HTTP providers, Swift FoundationModels, the history DB, and the clipboard/paste path.

---

## 10. PLAUD relevance — concrete extension points

A Plaud-style product (long-form recordings, multi-speaker conversations, AI summaries, sync, mobile) maps onto this subsystem as follows. Wrap/extend, do not rewrite:

1. **Summaries / structured notes — extend the prompt + provider layer, not net-new code.** The entire LLM machinery already exists. To add "summary", "action items", "meeting minutes", add new `LLMPrompt` entries (`settings.rs:641` / command `add_post_process_prompt` at `shortcut/mod.rs:911`) and let the user pick a different prompt per action. To return *richer* output than a single string, widen the JSON schema at `actions.rs:195` (currently `{ transcription: string }`) to e.g. `{ summary, bullets[], action_items[], speakers[] }`, and parse it in the `Ok(Some(content))` arm (`actions.rs:219`). Mirror the same schema in the Swift `@Generable CleanedTranscript` struct (`apple_intelligence.swift:5`) for the on-device path.

2. **Speaker/diarization-aware post-processing — feed labeled segments through the same gateway.** `post_process_transcription` (`actions.rs:66`) takes a flat `&str`. For conversations, change the user-message construction (`actions.rs:148` / `llm_client.rs:166`) to pass a speaker-tagged transcript (`Speaker 1: …\nSpeaker 2: …`). Diarization itself belongs upstream (ASR/VAD subsystem), but **this is the natural place to consume diarized text** and ask the LLM to attribute and summarize per speaker. The filler/stutter cleaner (`text.rs`) is already token-stream friendly and can run per-segment.

3. **Long-form chunking — wrap `process_transcription_output`.** Today it assumes one short utterance and one LLM round-trip. For hour-long recordings, introduce a chunker that splits `final_text` into windows, calls `post_process_transcription` per chunk concurrently (the client is stateless and re-entrant — `llm_client.rs`), then a reduce step (map-reduce summary). Hook point: between `actions.rs:359` and `:364`.

4. **Apple Intelligence as the default on-device summarizer (mobile-relevant).** `process_text_with_system_prompt` (`apple_intelligence.rs:33`) is the on-device path. The same FoundationModels API exists on iOS 26+; the Swift bridge (`apple_intelligence.swift`) is largely portable to an iOS target. For an iPhone app, reuse the `@Generable` structured-output pattern and the FFI shape; the Rust `AppleLLMResponse` ABI is platform-neutral. **Wrap the blocking FFI in `spawn_blocking`** first (see Gaps) so a long summary doesn't stall the runtime.

5. **Cloud/local sync — tap the persistence boundary.** Post-processed text + prompt already flow into `HistoryManager::save_entry` (`actions.rs:591`). A sync engine should subscribe at that boundary (or extend the history schema) rather than touching this subsystem. The `SecretMap` redaction pattern (`settings.rs:310`) is the model to follow for sync credentials.

6. **Local/offline LLM by default.** The `custom` provider (`settings.rs:603`, base `http://localhost:11434/v1`, keyless, `allow_base_url_edit:true`) already targets Ollama/llama.cpp. For a local-first Plaud, make `custom` (or Apple Intelligence) the default `post_process_provider_id` (`settings.rs:520`) and ship a bundled summarization prompt. The reasoning-suppression branch for `custom` (`actions.rs:133`) is already correct for local servers.

7. **Multi-output history.** To show "raw vs. summary vs. transcript", the `ProcessedTranscription` struct (`actions.rs:343`) is the carrier — extend it with additional named outputs and thread them through `save_entry`.

---

## 11. Gaps vs. a Plaud-style product

1. **No diarization / speaker model.** The subsystem is single-speaker by design; `&str` in, `String` out. No speaker labels exist anywhere in `text.rs`/`actions.rs`.
2. **No long-form/chunking.** One transcript → one LLM call. No windowing, no map-reduce, no token-budgeting. A schema field literally named `transcription` (`actions.rs:53`) reveals the "rewrite one utterance" framing.
3. **No summaries/notes/action-items** out of the box — only a single "Improve Transcriptions" prompt (`settings.rs:645`) and a single-string schema. Everything needed exists, but nothing is wired.
4. **No HTTP timeout / retry / cancellation.** `create_client` (`llm_client.rs:100`) sets no `.timeout(...)`; a hung provider blocks the post-process task indefinitely. No retry/backoff, no streaming. For long-form this is a reliability gap.
5. **Apple Intelligence FFI blocks the Tokio runtime.** `process_text_with_system_prompt` is a synchronous semaphore-blocked call invoked directly in the async task (`actions.rs:162`) with no `spawn_blocking`. On long inputs this starves other async work.
6. **`maxTokens` is word-count truncation, not tokens** (`apple_intelligence.swift:25`), and it truncates output *after* generation rather than constraining it — wasteful for summaries.
7. **API keys stored in cleartext** in the Tauri store (`SecretMap` only redacts `Debug`/logs, `settings.rs:312`). No OS keychain. A security gap for a synced/mobile product.
8. **No conversation/session model.** History entries are per-utterance (`actions.rs:591`); there is no notion of a "recording session" spanning many utterances, which a Plaud device produces continuously.
9. **No streaming UI.** Output appears only after the full round-trip; no token streaming to an overlay/notes view.
10. **Language fields are inconsistent.** Filler filtering keys off `app_language` (`transcription.rs:706`) while Chinese conversion keys off `selected_language` (`actions.rs:303`); a multilingual conversation (common in meetings) has no per-segment language handling.
11. **No mobile target.** The HTTP client (`reqwest`) is portable, but the whole stage runs inside desktop Tauri actions (`actions.rs`); the Apple Intelligence bridge is `cfg`-gated to macOS-aarch64 and not yet wired for iOS. No background/continuous capture exists to feed this stage on a phone.
12. **LLM errors are silent.** Every failure returns `None` and falls back to raw text (`actions.rs:251`, `:288`), with no user-facing surface beyond a debug log — acceptable for dictation, poor for a "my summary failed" product moment.
