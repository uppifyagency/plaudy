# riffado Teardown → Plaude Local: Keep / Spec / Drop

> The decision ledger that closes the [RIFFADO-CODEWIKI.md](RIFFADO-CODEWIKI.md) teardown. That
> document is a 13-subsystem architecture reference for [riffado](https://github.com/riffado/riffado)
> (AGPL-3.0). This file is the **verdict**: for every reusable idea, do we *keep* it (adopt /
> already have), *spec* it (worth doing — needs a design decision first), or *drop* it (cloud /
> web / multi-tenant machinery that does not apply to a local-first, single-user macOS app)?
>
> _Authored 2026-06-23, independently / clean-room. See the AGPL firewall below._

## AGPL firewall (read first)

riffado is **AGPL-3.0**. We may mirror **facts and interface shapes**; we may **not** copy or
paraphrase-translate its **source**.

| Safe to mirror (facts / specs) | Must NOT copy (copyleft source) |
| --- | --- |
| table & column names, the recording→transcript→enhancement split | `schema.ts`, the migration SQL, `queries/*` |
| `v1:iv:tag:ciphertext` envelope, AES-256-GCM layout | `encryption.ts`, `fields.ts` |
| `t=<unix>,v1=<hmac>` webhook signature, ±300s tolerance | `webhooks/*`, `signature.ts`, the CIDR/SSRF matcher |
| SRT/VTT/JSON export formats (open standards) | `export/route.ts`, `waveform.ts`, `decodePeaks` |
| `TranscriptionStyle` enum shape, prompt *structure* | `provider-presets.ts`, `transcribe-recording.ts`, the prompt **strings** |
| `op_`+CRC32 key format, keyset-cursor shape | `auth.ts`, `auth-request.ts`, `admin/*` |
| Worker message protocol `{type:"transcribe"}→complete` | `browser-transcriber.ts`, `worker.ts` |

`src/components/ui/*` is **shadcn-derived (MIT)** and the libs under it (Radix, Tailwind, cva, cmdk,
next-themes, sonner) are permissive — usable directly. Only riffado's **authored composition**
(Workstation, RecordingList, the Waveform logic, the settings system) is encumbered: reference, don't lift.

## Legend

- **Keep ✅** — adopt the pattern. Either already shipped in Plaude Local, or a one-step clean-room reimplementation with no open question.
- **Spec 📝** — worth adopting, but a design decision must be made before building (named in the row).
- **Drop ❌** — does not apply to a local-first, single-user, offline desktop app.

---

## Data model & persistence

| Idea (riffado) | Verdict | Plaude Local locus / decision |
| --- | --- | --- |
| `recordings → transcriptions → ai_enhancements` split | ✅ / 📝 | `transcription_history` row + inline transcript exist (`managers/history.rs`). A separate **`ai_enhancements`** table is **Spec** — gated on the AI-summary decision below. |
| First-class **per-segment** table (speaker, start_ms, end_ms, text) — the gap riffado *lacks* | ✅ done | `speakers` + `transcription_segments` (migration #5, Fase 2). This is where we diverge **upward**. |
| Explicit transcript **state column** (`pending\|transcribing\|done\|failed`) | ✅ done | `status` column, migration #6 (`TranscriptionStatus`), shipped 2026-06-23. We skip a separate `pending` — the row is born `transcribing` at Stop. |
| **Reset rows stuck in `transcribing` on startup** (riffado Lesson #7) | 📝 follow-up | **New gap created by the status column:** a finalize crash after `save_pending_entry` leaves a row pulsing forever. Fix: on boot, flip orphaned `transcribing` → `failed` (or re-queue) next to `recover_interrupted`. Small; named here so it isn't lost. |
| Idempotent, append-only on-boot migrations | ✅ have | `rusqlite_migration` + `user_version` (`managers/history.rs`). Drop riffado's advisory lock — single-process. |
| Keyset (cursor) pagination over a growing list | ✅ have | `get_history_entries` already pages `id < ?cursor ORDER BY id DESC`. |
| `waveform_peaks` write-once normalized array | 📝 | **Invert riffado:** compute peaks in **Rust at capture** (we hold the PCM) — no browser decode + POST-back. Gated on the player/scrubber UX. |
| Soft-delete tombstone (`deleted_at`) | ❌ skip (YAGNI) | We hard-delete (`delete_entry`). Tombstones exist to stop **cloud re-sync** resurrecting deletes — we have no cloud. Revisit only if a KB re-index loop lands. |
| `nanoid` text PKs | ❌ skip | SQLite `AUTOINCREMENT` integer PKs are fine for a single local DB. |

## Pipeline & correctness patterns

| Idea | Verdict | Decision |
| --- | --- | --- |
| Bounded concurrency, "one failure never aborts the batch" | ✅ principle | Session finalize already runs off-thread per session (`managers/session.rs`). If batch re-transcription lands, use `tokio::Semaphore(N)` + `Result` accumulation. |
| Single-flight coalescing (`inFlightSyncs` Map) | 📝 | Only when "transcribe now" can race auto-finalize. Rust `Mutex<HashSet<SessionId>>`. Not needed by the current single-active-session model. |
| `FOR UPDATE` re-check + tombstone guard after slow I/O | ❌ mostly | TOCTOU machinery for a multi-process Postgres writer. SQLite single-writer + our `Mutex<Option<ActiveSession>>` already serialize. |
| Idempotent processing keyed by stable id + version/mtime | ✅ have (lite) | `recover_interrupted` re-finalizes orphan `*.session.pcm`. Adequate for local capture (no version diffing needed — we *are* the recorder). |

## Transcription & AI

| Idea | Verdict | Decision |
| --- | --- | --- |
| `ProviderPreset` + `TranscriptionStyle` abstraction | 📝 | A Rust enum (`LocalOnnx \| Whisper{base_url} \| Chat{base_url} \| Ollama`) so one config can target bundled local ASR **and** an optional local "cloud-boost" (Ollama/LM Studio). **Decide:** do we offer non-local AI at all? Local-first stance says local-only by default, opt-in otherwise. |
| LLM summaries / key-points / action-items (`ai_enhancements`) | 📝 | Needs an on-device LLM decision (bundled small model vs user-supplied endpoint). Drives the `ai_enhancements` table above. |
| Worker message protocol → progress events | 📝 | We already emit `SessionStateChanged`. **Improve on riffado:** emit **real % progress** during long transcription, not just a boolean. Tauri `emit`/`listen`. |
| Browser-WASM Transformers.js + CDN model fetch | ❌ drop | We run native ONNX (`transcribe-rs` + sherpa) with **bundled** models — strictly better and offline. |
| 25 MiB ffmpeg compression dance | ❌ drop | On-device ASR has no upload cap. |

## Audio, export & storage

| Idea | Verdict | Decision |
| --- | --- | --- |
| Pure in-process metadata/duration (no ffprobe) | ✅ principle | Stay binary-free; `symphonia` if we need richer metadata than the WAV we write. |
| **Per-segment** SRT/VTT + Markdown speaker-labelled export | 📝 | Our `transcription_segments` already carry per-segment timings — so we can **fix riffado's single-cue-per-recording weakness**. High-value, low-risk export feature. |
| `StorageProvider` trait + path-traversal guard (`canonicalize` + `starts_with(base)`) | 📝 | Only when a user-chosen "KB folder" lands. Keep the **path-traversal guard** as the security-critical piece (re-derive in Rust). Drop the S3 half. |
| Compensating-delete orphan avoidance | 📝 | Adopt if/when writes span file + DB transactionally (KB import). |
| S3 / presigned URLs / 302 / HTTP Range serving | ❌ drop | Always local FS; Tauri serves files directly (`convertFileSrc`). |

## Security & encryption

| Idea | Verdict | Decision |
| --- | --- | --- |
| Versioned, pass-through field encryption (`v1:` prefix, legacy tolerance) | 📝 | Rust `aes-gcm`, `v1:` header. **Only** needed once we store a secret (e.g. a user API key). |
| **Key management** | 📝 **decision point** | riffado holds `ENCRYPTION_KEY` in a server env — antithetical to local. **Decide: macOS Keychain (Tauri `keyring`/stronghold) vs passphrase-derived (Argon2).** Keychain is the default recommendation. Encrypt/decrypt only in the Rust data layer so the frontend never sees ciphertext (avoids riffado's call-site-discipline leak). |
| Idempotent id-cursor backfill (with `--dry-run`) | 📝 | Pattern to retro-encrypt `history.db` *if* we ever ship at-rest encryption for content. Low priority — the DB never leaves the Mac. |

## Automation & frontend

| Idea | Verdict | Decision |
| --- | --- | --- |
| Outbound **signed webhooks** + SQLite-backed durable retry | 📝 | Genuinely useful locally: fire `session.ended` / `transcription.completed` to the user's own n8n/Obsidian. Port the signature spec verbatim (`hmac`+`sha2`), single Rust background task over a queue table, backoff `[30s,2m,10m,1h,6h]`. Opt-in. |
| Master-detail "Workstation" UX (date-grouped list, search, optimistic delete, infinite scroll) | 📝 | Sessions UI walking skeleton is shipped; this is the **richer UX buildout** (HANDOFF §3). `HistorySettings.tsx` already has infinite scroll + optimistic delete to grow from. |
| Canvas waveform scrubber + `usePlaybackEngine` (DPR-aware, `role="slider"`, full keyboard) | 📝 | Pairs with Rust-computed peaks above. Keep the **a11y contract** (keyboard + slider role) as non-negotiable. |
| `TranscriptionModelPicker` (curated list + "Custom…" escape hatch) | 📝 | Maps onto our bundled-models model manager. |
| Command palette, numbered onboarding, OKLCH token theming | 📝 | Nice-to-have UX; Handy already has onboarding to extend. |
| Native OS notification on "session finished" | 📝 | `tauri-plugin-notification` — the desktop analogue of riffado's browser `Notification`. Small, high-delight. |
| Unified error envelope `{error, code, details}` | ✅ principle | Keep Tauri command `Result<_, String>` consistent; add a code enum if surfaces grow. |

## Drop ❌ — entire subsystems that do not apply

Single-user local desktop ⇒ none of this exists for us:

- **Postgres + Better Auth + multi-tenancy** — users/sessions/accounts, `userId`-on-every-query, per-user cascade, `FOR UPDATE SKIP LOCKED` multi-process coordination.
- **Admin tier** — email/IP allowlists, HMAC reauth cookie, suspension, audit logs, install-script telemetry, `IS_HOSTED` dual-mode.
- **Plaud cloud integration** — OTP login, region redirects, UT→WT token escalation, workspace discovery, Webshare proxy bot-evasion, temp-url download, paginated sync, the browser-timer scheduler, sync rate limiting. *(We are the **anti-Plaud**: capture is local.)* Exception: an **opt-in one-shot "import old Plaud library" importer** is the one cloud-touching idea worth keeping — clean-room, clearly fenced, no UA-spoofing — filed under Spec, low priority.
- **Public API perimeter** — `op_` API keys, CRC32 checksums, two-bucket rate limiting, DNS-pinned SSRF guard, key scopes. (The webhook **signing** scheme survives; the *perimeter defense* does not.)
- **Cloud storage** — S3/R2/MinIO, AWS SDK, presigned URLs, 302 redirects, env-driven backend selection.
- **Cloud delivery / ops** — SMTP/Nodemailer/React-Email/Bark, Rybbit analytics, cross-device sync, Docker Compose, GHCR images, standalone Next output, `install.sh`, server-held `ENCRYPTION_KEY` + at-request-decrypt model.

---

## Roadmap mapping (where these land)

1. **Sessions UI** *(walking skeleton shipped 2026-06-23)* ← Workstation master-detail; next: richer list/search + per-row status (status column ✅ done).
2. **Diarization timeline & export** ← `transcription_segments` ✅ done → add **per-segment SRT/VTT + Markdown** export (Spec, high-value).
3. **Player UX** ← Rust-computed `waveform_peaks` + canvas scrubber (Spec).
4. **Optional AI layer** ← `TranscriptionStyle` provider abstraction + `ai_enhancements` (Spec; depends on the local-LLM decision).
5. **Automation** ← signed webhooks + SQLite durable retry (Spec; opt-in).
6. **KB folder** ← `StorageProvider` trait + path-traversal guard (Spec; drop the S3 half).

## Open decisions surfaced (need a human call)

- **AI provider stance** — local-only by default? allow opt-in Ollama/LM Studio? any non-local path at all?
- **Encryption key management** — macOS Keychain (recommended) vs passphrase-derived; or defer entirely until a secret exists to protect.
- **Webhooks** — ship the local automation surface, or YAGNI until asked.
- **Startup `transcribing`-reset** — small correctness follow-up to the status column (see Data-model table); do now or defer.
