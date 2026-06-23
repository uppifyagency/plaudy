# riffado — Codewiki Teardown

> **What this is.** A subsystem-by-subsystem teardown of [riffado/riffado](https://github.com/riffado/riffado) (ex-OpenPlaud) — an AGPL-3.0, self-hosted Next.js transcription app in our product category — produced as an **architecture & data-model reference** for building **Plaude Local** (our fully-local, offline-first *anti-Plaud* on Handy/Tauri).
>
> **AGPL boundary.** Study the design; do **not** copy riffado source into Plaude Local. Reimplement clean-room.
>
> Generated from a 14-agent teardown workflow. Sections: 13.
>
> **The verdict lives in [DECISIONS.md](DECISIONS.md)** — the Keep/Spec/Drop ledger that turns this teardown into actionable choices for Plaude Local.

## Table of Contents

- [Overview](#overview)
- [End-to-End Data Flow](#end-to-end-data-flow)
- [Product & System Architecture](#product--system-architecture)
- [Data Model (Drizzle + Postgres)](#data-model-drizzle--postgres)
- [Auth, Accounts & Multi-tenancy](#auth-accounts--multitenancy)
- [Plaud Cloud Integration Layer](#plaud-cloud-integration-layer)
- [Sync Engine & Scheduling](#sync-engine--scheduling)
- [Transcription: OpenAI-compatible Providers & AI](#transcription-openaicompatible-providers--ai)
- [In-Browser / Local Transcription (Transformers.js Whisper WASM)](#inbrowser--local-transcription-transformersjs-whisper-wasm)
- [Audio Handling & Export Formats](#audio-handling--export-formats)
- [Storage Abstraction (Local FS + S3-compatible)](#storage-abstraction-local-fs--s3compatible)
- [Encryption at Rest](#encryption-at-rest)
- [Public/Automation API & Signed Webhooks](#publicautomation-api--signed-webhooks)
- [Frontend Architecture & UX](#frontend-architecture--ux)
- [Notifications, Admin & Testing](#notifications-admin--testing)
- [Lessons for Plaude Local](#lessons-for-plaude-local-anti-plaud-local-first)

---

# Overview

## riffado — Architecture in One Read

**What it is.** riffado is an open-source (**AGPL-3.0**) AI-transcription *companion* for cloud-connected voice recorders (the Plaud Note family). It is a single **Next.js 16 App Router** application backed by **Postgres**, that pulls a user's recordings from Plaud's undocumented private cloud, transcribes them via any **OpenAI-compatible** provider (or in-browser Whisper), enhances them with LLM summaries, stores audio in pluggable local-FS/S3 storage, and exposes a versioned read API + outbound signed webhooks. It ships from **one codebase in two modes**: single-tenant self-host (`docker compose up`) and a multi-tenant hosted SaaS (`IS_HOSTED=true`).

**The single biggest architectural bet: one process, no queue.** Web + API + all three background workers (sync, transcription, webhook delivery) run inside the *same* Next.js process. There is no Redis, no SQS, no separate worker container. Coordination happens entirely in Postgres: `SELECT … FOR UPDATE SKIP LOCKED` claims, `ON CONFLICT DO UPDATE` rate-limit buckets, advisory locks for migration singletons and per-user serialization, and a `row_number()`-windowed fair-share webhook claim. Horizontal scaling = run N copies of the image behind a load balancer; the DB row locks keep them safe. The webhook "worker" is literally a background task started by Next's `register()` boot hook in `src/instrumentation.ts` (gated on `NEXT_RUNTIME === "nodejs"`).

**Module map (subsystem → primary path):**
- **Boot/deploy** — `docker-entrypoint.sh` → `src/db/migrate-idempotent.ts` (advisory-locked, connect-retry) → `bun server.js`; `scripts/install.sh` (one-line installer), `scripts/release.ts`.
- **Data model** — `src/db/schema.ts` (16 tables, Drizzle over postgres-js), `src/db/migrations/0000…0024` (25 ordered SQL steps), `src/db/queries/*` (hand-written concurrency idioms).
- **Auth & multi-tenancy** — `src/lib/auth.ts` (Better Auth), `src/lib/auth-server.ts`/`auth-request.ts`, `src/lib/admin/*` (hosted-only operator tier).
- **Plaud cloud integration** — `src/lib/plaud/*` (OTP login, region redirects, UT→WT token escalation, Webshare proxy bot-evasion, recording list/download).
- **Sync engine** — `src/lib/sync/sync-recordings.ts` (paginated pull, dedup, in-process coalescing); scheduling is a *browser* timer (`use-auto-sync`), not server cron.
- **Transcription/AI** — `src/lib/transcription/transcribe-recording.ts`, `src/lib/ai/provider-presets.ts` (`TranscriptionStyle = whisper|chat|gemini`), `compress-audio.ts` (ffmpeg→Opus for Whisper's 25 MiB cap), plus an unwired browser-WASM path (`browser-transcriber.ts` + `worker.ts`, `@xenova/transformers`).
- **Audio & export** — `src/app/api/recordings/upload/route.ts` (music-metadata, no ffprobe), `src/lib/audio/waveform.ts` (client peak decode), `src/app/api/export/route.ts` (JSON/TXT/SRT/VTT).
- **Storage** — `src/lib/storage/{types,local-storage,s3-storage,factory}.ts` (5-method `StorageProvider`).
- **Encryption at rest** — `src/lib/encryption.ts` (AES-256-GCM), `src/lib/encryption/fields.ts` (`v1:` versioned, legacy pass-through).
- **Public API & webhooks** — `src/app/api/v1/*`, `src/lib/webhooks/*` (DB-as-queue worker, HMAC signing, DNS-pinned SSRF guard).
- **Frontend** — `src/components/dashboard/workstation.tsx` (master-detail composition root), `recording-list`, `recording-player` + `use-playback-engine`/`use-waveform`, the settings system, `command-palette`. shadcn-style: Radix + Tailwind v4 (OKLCH) + cva.
- **Notifications/admin/tests** — `src/lib/notifications/*` (SMTP/Bark/browser), `src/lib/admin/*`, Vitest `src/tests/**` (~55 files incl. a static PII-grep guard).

**Tech stack.** Bun runtime; Next.js 16 App Router (standalone output); Drizzle ORM over `postgres-js`; Better Auth; OpenAI SDK + `@xenova/transformers` + `@google/generative-ai`; AWS S3 SDK; Nodemailer + React Email; Fumadocs; Vitest + Biome; `ffmpeg` in the runtime image. Postgres 16 is the only stateful dependency.

**Design philosophy.** The *deploy surface* (DB schema, env vars, compose structure, installer URLs) is a versioned contract; internal code is freely refactorable. `IS_HOSTED` flips strict-by-default safety knobs while the default path is always self-host ("if it won't run in `docker compose up`, it doesn't ship"). Everything is fail-open where availability matters (rate limiting) and fail-closed where security matters (admin gate → 404, IP allowlist).

**License.** **AGPL-3.0** — we may study and re-derive its architecture but must not copy its source into Plaude Local. This caveat governs the entire wiki.

---

# End-to-End Data Flow

## End-to-End: the Life of a Recording

This traces one recording from the moment it exists in Plaud's cloud to the moment a user reads its transcript and exports it — naming the subsystem and key files at each hop.

### 1. Discovery & pull (Plaud cloud → Sync engine)
A browser timer (`use-auto-sync` hook, documented in `docs/AUTO_SYNC.md`) fires `POST /api/plaud/sync`. There is **no server cron**. The route (`src/app/api/plaud/sync/route.ts`) gates on `enforcePlaudSyncRateLimit` (a cross-process DB token bucket keyed `plaud-sync:user:${userId}`), then calls `syncRecordingsForUser` (`src/lib/sync/sync-recordings.ts`). An in-process `inFlightSyncs` Map coalesces concurrent syncs for the same user.

The sync engine builds a `PlaudClient` (`src/lib/plaud/client.ts`) from the decrypted `plaud_connections.bearerToken` (`client-factory.ts`). Auth is a **two-tier token model** (`src/lib/plaud/workspace.ts`): the user token (UT, from OTP login in `auth.ts`) mints a short-lived **workspace token (WT)** via `POST /user-app/auth/workspace/token/{id}`; data calls carry the WT, silently falling back to UT if minting fails. Every request goes through `plaudFetch` (`fetch.ts`), which spoofs a Chrome fingerprint and optionally routes through a rotating Webshare proxy pool (`proxy.ts`) to evade Plaud's bot filtering.

The engine paginates `getRecordings(skip, 50, …, "edit_time", true)` and stops early when **two consecutive pages yield zero new/changed rows** — an incremental sync via `edit_time DESC` ordering, no cursor. `safeParseJson` (`parse.ts`) reads body-as-text first to survive Plaud's `status:-302` region-redirect envelopes.

### 2. Dedup, download & storage (Sync engine → Storage)
`processRecording` is the per-item state machine. It looks up the row by `(plaudFileId, userId)`; **version dedup** compares `plaudVersion === version_ms.toString()` → skip if unchanged; a **tombstone guard** skips `deletedAt`-set rows so deletes don't resurrect (issue #56). New rows: `plaudClient.downloadRecording` chains `getTempUrl` → fetches the signed `temp_url_opus`. A unique storage key `${userId}/<name>.mp3` is built, then `StorageProvider.uploadFile` (`src/lib/storage/factory.ts` picks `LocalStorage` or `S3Storage`) writes the blob. Persistence uses a **compensating transaction**: upload first, then `INSERT … RETURNING`; if the DB insert throws, the just-uploaded blob is deleted to avoid orphans. The `recordings` row stores `storageType` + `storagePath` so reads later know how to fetch. On success it `emitEvent("recording.synced", …)`.

### 3. Transcription — two paths
**(a) Server provider path (the real one).** If `userSettings.autoTranscribe`, the sync loop fire-and-forgets `queueTranscriptions` (no `await`), which sequentially calls `transcribeRecording` (`src/lib/transcription/transcribe-recording.ts`). There is **no status column** — "transcribed" = a `transcriptions` row with non-empty `text` exists (idempotent short-circuit unless `force`). It selects credentials (`api_credentials`, default-flagged), `decrypt`s the API key, downloads + decrypts the audio, and routes on `getTranscriptionStyle(provider)` (`src/lib/ai/provider-presets.ts`): `"whisper"` → `openai.audio.transcriptions.create` (with `maybeCompressForWhisper` re-encoding to mono Opus via ffmpeg to beat the 25 MiB cap, `compress-audio.ts`); `"chat"` → `chatTranscribe` (base64 `input_audio` part); `"gemini"` → native `geminiTranscribe`. `parseTranscriptionResponse` (`format.ts`) handles `verbose_json`/`json`/`diarized_json` (the diarized case flattens `"speaker: text"` lines — losing per-segment timestamps). The write re-locks the row `FOR UPDATE`, re-checks the tombstone, and upserts the `transcriptions` row with `encryptText(text)`, then optionally generates a title and emits `transcription.completed`/`transcription.failed`.

**(b) Browser-WASM path (built but unwired).** `transcribeInBrowser` / `BrowserTranscriber` (`src/lib/transcription/browser-transcriber.ts`) spawns a module Web Worker (`worker.ts`) running Whisper via `@xenova/transformers` on WASM, with a `{type:"transcribe"} → {type:"complete"}` message protocol, 30s/5s chunking, and HuggingFace-CDN model fetch. A grep shows **no importers** — it is dead/standalone code despite prominent landing-page marketing, never invoked by `transcribeRecording`.

### 4. Enhancement & metadata
The summary route (`src/app/api/recordings/[id]/summary/route.ts`) calls an LLM through the same credential set, strips ```json fences, and stores `ai_enhancements.summary` (`encryptText`) + `keyPoints`/`actionItems` (`encryptJsonField`, `{c:…}` JSONB envelope). The client decodes a normalized waveform-peaks array (`decodePeaks`, `src/lib/audio/waveform.ts`) and best-effort `POST`s it to `/api/recordings/[id]/peaks` (write-once, `WHERE waveform_peaks IS NULL`).

### 5. UI presentation (Frontend)
`dashboard/page.tsx` (an RSC) runs parallel Drizzle queries, **decrypts content server-side**, and hands a single prop bag to the `Workstation` client island (`src/components/dashboard/workstation.tsx`). The master-detail layout pairs `RecordingList` (date-grouped, IntersectionObserver infinite scroll, client-side transcript search) with a detail pane: `RecordingPlayer` (`use-playback-engine` owns a hidden `<audio src="/api/recordings/[id]/audio">`), the canvas `Waveform` scrubber (renders the cached peaks), and `transcription-panel` (transcript + summary). Audio is served by `/api/v1/recordings/[id]/audio`: **302-redirect to an S3 presigned URL** for S3, or **RFC 7233 Range** streaming for local.

### 6. Export & fan-out
`GET /api/export?format=json|txt|srt|vtt` (`src/app/api/export/route.ts`) decrypts all of a user's transcripts and emits the chosen format — but note SRT/VTT collapse each recording into a **single whole-recording cue** (no per-segment timeline, even though diarized data may exist). Separately, the events emitted at steps 2–3 (`recording.synced`, `transcription.completed`, …) are picked up by `emitEvent` (`src/lib/webhooks/emit.ts`), enqueued into `webhook_deliveries`, and delivered by the in-process worker (`src/lib/webhooks/worker.ts`): two-phase `FOR UPDATE SKIP LOCKED` claim with a 15-min lease, exponential backoff `[30s,2m,10m,1h,6h]`, HMAC-SHA256 signature (`t=…,v1=…`), and a DNS-pinned SSRF guard (`url.ts`). Optional SMTP/Bark notifications (`src/lib/notifications/*`) fire only when `newRecordings > 0`.

**The throughline:** every hop is `userId`-scoped, every content field is encrypted at rest, and every slow-I/O write re-checks the soft-delete tombstone inside a `FOR UPDATE` transaction — the same TOCTOU discipline repeated across sync, transcription, and webhook delivery.

---

# Subsystems

## Product & System Architecture

**What it is.** riffado is an open-source (AGPL-3.0) AI-transcription *companion* for cloud-connected voice recorders (currently the Plaud Note family): a single Next.js 16 App Router application backed by Postgres that pulls recordings from the manufacturer's cloud, transcribes them via any OpenAI-compatible provider (or in-browser), and stores audio in pluggable local/S3 storage. It ships from one codebase in two production modes — single-tenant self-host (`docker compose up`) and a multi-tenant hosted SaaS (`IS_HOSTED=true`) — with no separate API service, worker, or message queue: web + API + background workers all run inside one process.

**Key files.**
- `README.md` — product pitch, feature list, the one-liner install (`curl … | sh`), Plaud-connect explanation, AGPL note.
- `content/docs/reference/architecture.mdx` — canonical architecture doc: single-process runtime, pull-based sync, pluggable storage/AI, route groups, multi-process safety.
- `content/docs/index.mdx` — docs landing; "How it ships" (hosted vs self-host both first-class).
- `AGENTS.md` — the real spec: deploy-surface contract, dual-mode invariants, code conventions, extension points, CRITICAL blocks (user-scoped queries, encryption, migrations).
- `next.config.ts` — `output: "standalone"`, custom image loader, `outputFileTracingIncludes` for the served install script, Fumadocs MDX wrapper.
- `Dockerfile` — multi-stage Bun build (deps → builder → runner), bundles migration + backfill scripts, installs `ffmpeg`.
- `docker-compose.yml` — the deploy topology: `db` (postgres:16-alpine) + `app` (GHCR image) + volumes, healthchecks.
- `docker-compose.dev.yml` — overlay that builds the image locally instead of pulling.
- `docker-entrypoint.sh` — runs idempotent migrations, then `exec`s the server.
- `scripts/install.sh` — one-line installer (prereqs → download release artifacts → generate secrets → `compose up` → poll `/api/health`).
- `scripts/release.ts` — two-phase release (open PR, then tag merged commit); maintainer-only.
- `src/instrumentation.ts` — Next.js boot hook that starts the in-process webhook worker.
- `src/proxy.ts` — request-edge middleware (`export function proxy`) for analytics-proxy header stripping and `/admin` pathname injection.
- `src/db/migrate-idempotent.ts` — boot-time migration with a Postgres advisory lock + connect-retry, bundled into the image.
- `src/lib/env.ts` — Zod-validated env object; the only sanctioned way to read config.
- `src/app/api/health/route.ts` — `GET /api/health` liveness probe used by compose + installer.

**How it works.**

*Boot sequence.* The container `ENTRYPOINT` is `docker-entrypoint.sh`, which runs `bun migrate-idempotent.js` then `exec "$@"` (default `bun server.js`, the Next.js standalone server). `migrate-idempotent.ts` is the interesting part: it connects with exponential backoff (`CONNECT_DELAYS = [1000,2000,4000,8000,16000]`, retrying only on connection-class errors), then takes a Postgres **advisory lock** (`ADVISORY_LOCK_ID = 0x4f504c41`, i.e. ASCII "OPLA" — a leftover from the OpenPlaud name) before running Drizzle's `migrate()`. The lock makes migrations safe when multiple app containers boot concurrently against one database (the hosted multi-process case). Once the server is up, Next's `register()` in `src/instrumentation.ts` fires — gated on `process.env.NEXT_RUNTIME === "nodejs"` so it only runs in the Node server runtime (not edge/browser) — and calls `startWebhookWorker()`. That is how a "worker" exists without a worker container: it's a background task started by the web process's instrumentation hook.

*Single-process runtime.* Per `architecture.mdx`, "Web + API + worker — all one Next.js process. The sync worker, the transcription worker, and the webhook delivery worker are background tasks inside the same process." Horizontal scaling = run more copies of the same image behind a load balancer. Postgres is the only stateful dependency: Better Auth uses it for sessions, Drizzle ORM for everything else. Storage holds audio files only; all metadata lives in Postgres.

*Request topology.* The App Router uses route groups under `src/app/`: `(app)` authenticated UI (dashboard, recordings, settings, workstation), `(auth)` login/register/OTP, `(docs)` Fumadocs site, `(admin)` and `(legal)` groups, `[version]` dynamic segment (serves version-pinned `/vX.Y.Z/install.sh`), and `api/`. The API splits into `src/app/api/*` (session-authenticated internal routes: `plaud`, `recordings`, `settings`, `backup`, `export`, `search`, `health`, `dev`, `int`) and `src/app/api/v1/*` (the public, API-key-authenticated, stable read surface). `src/proxy.ts` is the edge middleware (`matcher: ["/api/int/:path*", "/admin/:path*"]`): for `/api/int/*` (the Rybbit analytics proxy) it strips `cookie` and `authorization` headers before Next's rewrite forwards them upstream; for `/admin/*` it injects `x-pathname` so server components can audit-log the real path and build reauth `?next=` bounces.

*Pull-based sync.* Plaud offers no push/webhook API, so `src/lib/sync/sync-recordings.ts` is an idempotent, paginated pull loop: list recordings → diff `plaudFileId` + `version_ms` against the DB (insert new, refresh changed, skip unchanged) → download each new audio file once and hand the encoded storage key to the storage provider → emit `recording.synced` to subscribed webhooks. Interrupted runs resume on the next tick without duplicates.

*Multi-process safety.* All three background workers (sync, transcription, webhook delivery) claim work with `SELECT … FOR UPDATE SKIP LOCKED` at the DB level; in-memory `running` flags are explicitly advisory only. This is the core invariant that lets the same code run as a single self-host container or N hosted replicas.

*Pluggability.* Storage hides behind a `StorageProvider` interface (`src/lib/storage/types.ts`) chosen once by `createStorageProvider()` (`factory.ts`); feature code never branches on storage type. AI has *no* per-provider class — the OpenAI SDK is pointed at a per-user `baseURL`, and a `transcriptionStyle` field on each provider preset (`src/lib/ai/provider-presets.ts`) selects between Whisper-style multipart (`POST /v1/audio/transcriptions`) and chat-style audio-input (`POST /v1/chat/completions` with an `input_audio` part). Non-standard providers are fronted by an adapter, never by name-branching.

*Build & ship.* `Dockerfile` is three stages on `oven/bun:1`: `deps` (`bun install --frozen-lockfile --ignore-scripts`, skipping the fumadocs postinstall that needs the full tree), `builder` (installs `git` because Fumadocs' `lastModified` shells out to `git log`, regenerates MDX sources via `bunx fumadocs-mdx`, runs `bun run build`, then `bun build`s the migration and `encrypt-backfill` scripts into standalone JS), and `runner` (installs `ffmpeg` — needed because OpenAI Whisper rejects bodies >25 MiB so long recordings are re-encoded to mono Opus; copies `.next/standalone` + static + public + the bundled scripts + the migrations folder). `next.config.ts` sets `output: "standalone"` and declares `outputFileTracingIncludes` for `scripts/install.sh` so the standalone tracer ships the file that the `/install.sh` route reads from disk at request time.

*Install flow (the deploy surface).* `scripts/install.sh` is templated server-side (`VERSION="{{VERSION}}"` is substituted by the route). It detects OS (Linux/macOS only; Windows → WSL2), verifies Docker + compose v2, reopens stdin from `/dev/tty` (so `curl | sh` doesn't consume the script as input; falls back to `NON_INTERACTIVE=1` on CI runners), prompts for install dir + `APP_URL`, downloads `docker-compose.yml` + `env.example` from the GitHub release, generates `BETTER_AUTH_SECRET` and `ENCRYPTION_KEY` via `openssl rand -hex 32`, patches them into `.env` with a portable awk routine, `chmod 600 .env`, then `docker compose pull && up -d` and polls `/api/health` for up to 60s.

*Release.* `scripts/release.ts` (Bun) is two-phase because `main` is a protected rolling integration branch: phase 1 bumps `package.json`, rewrites the `[Unreleased]` changelog heading to a dated version, branches to `release/vX.Y.Z`, commits, pushes, and opens a PR via `gh`; phase 2 (`finalize`) locates the merged release commit by `git log --grep` and creates/pushes the tag, which triggers the `docker.yml` + `release.yml` GitHub workflows.

**Contracts & shapes.**

Deploy topology (`docker-compose.yml`) — `db` + `app`, the structure itself is a self-host contract:
```yaml
db:   image: postgres:16-alpine   # volume pgdata, healthcheck pg_isready
app:  image: ghcr.io/riffado/riffado:${RIFFADO_VERSION:-latest}
      ports: ["3000:3000"]
      volumes: [audio:/app/audio]
      depends_on: { db: { condition: service_healthy } }
      healthcheck: GET http://localhost:3000/api/health == 200
```

Health probe (`src/app/api/health/route.ts`):
```ts
export async function GET() {
  return NextResponse.json({ status: "ok", version: APP_VERSION });
}
```

Core env vars (Zod-validated in `src/lib/env.ts`; never read `process.env` directly in feature code):
```
DATABASE_URL                       postgresql://postgres:${POSTGRES_PASSWORD}@db:5432/riffado
BETTER_AUTH_SECRET                 auth/session secret (openssl rand -hex 32)
ENCRYPTION_KEY                     AES-256-GCM at-rest key — reserved, never reused for HMAC
API_TOKEN_HASH_SECRET             optional; >=32 chars; HMAC key for API tokens (rotation-independent)
APP_URL                            public URL (validated as URL)
IS_HOSTED                          "true" => hosted SaaS mode; default false (self-host)
DISABLE_REGISTRATION               "true" => disable email/password sign-up
DISABLE_UPDATE_CHECK               "true" => disable self-host update check
WEBHOOKS_REQUIRE_PUBLIC_TARGETS    strict bool; defaults to IS_HOSTED
RATE_LIMIT_TRUST_PROXY_HEADERS     strict bool; trust X-Forwarded-For only behind trusted proxy
DEFAULT_STORAGE_TYPE               enum("local","s3"), default "local"
LOCAL_STORAGE_PATH                 default "./storage" (compose: /app/audio)
S3_ENDPOINT / S3_BUCKET / S3_REGION / S3_ACCESS_KEY_ID / S3_SECRET_ACCESS_KEY
SMTP_HOST / SMTP_PORT / SMTP_SECURE / SMTP_USER / SMTP_PASSWORD / SMTP_FROM
```

Route-group / API surface (`src/app/`):
```
(app)  authenticated UI        (auth)  login/register/OTP    (docs) Fumadocs
api/*      session-authenticated internal routes
api/v1/*   public, API-key-authenticated, stable read surface
[version]/install.sh, /install.sh   templated installer (deploy-surface contract)
```

Migration boot lock (`src/db/migrate-idempotent.ts`):
```ts
const ADVISORY_LOCK_ID = 0x4f504c41;   // pg advisory lock; safe for concurrent boots
const CONNECT_DELAYS = [1000, 2000, 4000, 8000, 16000];
```

Webhook worker boot (`src/instrumentation.ts`):
```ts
export async function register() {
  if (process.env.NEXT_RUNTIME !== "nodejs") return;
  const { startWebhookWorker } = require("./lib/webhooks/worker");
  startWebhookWorker();
}
```

**Notable patterns & decisions.**
- *One process, no queue.* Workers are in-process background tasks coordinated purely through Postgres row locks (`FOR UPDATE SKIP LOCKED`). This is the single most deliberate architectural bet — it keeps the deploy a two-container compose file while still scaling horizontally.
- *Boot-time idempotent migrations with an advisory lock* solve the "N replicas all migrate at once" race without an external migration orchestrator. The migration script is `bun build`-bundled into a standalone JS so the runtime image needs no `node_modules`.
- *Dual-mode from one codebase.* `IS_HOSTED` flips strict-by-default safety knobs (public-only webhook egress, rate limiting, secret hashing) while the default code path is always the self-host path. Self-host is treated as a hard contract ("if it won't run in `docker compose up`, it doesn't ship").
- *The "deploy surface" is sacred, internal code is not.* DB schema, env vars, compose structure, and the installer URLs are versioned contracts with deprecation cycles; everything else can be refactored freely.
- *Runtime stack:* Bun as the server/script runtime, Drizzle ORM over `postgres-js`, Better Auth, OpenAI SDK + `@xenova/transformers` (browser Whisper) + `@google/generative-ai`, AWS S3 SDK, Fumadocs for docs, Vitest, Biome. Standalone Next output + custom image loader + `ffmpeg` for the >25 MiB transcode workaround.

**Relevance to Plaude Local (anti-Plaud, local-first).**

*Adapt the design ideas, not the code (AGPL-3.0 caveat first).* riffado is AGPL-3.0; we may study its architecture but must **not** copy its source into our app, since linking/derivation would force us to publish under AGPL. Everything below is conceptual adaptation only.

- **Worth adapting:**
  - The `StorageProvider` factory pattern and the OpenAI-compatible `baseURL` + `transcriptionStyle` AI abstraction are exactly the kind of seam we want in our Rust pipeline — our diarizer/ASR backends should sit behind one trait/factory so `session.rs` never branches on engine. The `transcriptionStyle` idea maps cleanly to "Whisper vs Parakeet" model selection in transcribe-rs.
  - **Idempotent, resumable processing keyed by a stable file id + version.** Our long-form sessions (`managers/session.rs`) should record a stable recording id and a version/mtime so re-processing or crash-recovery is a diff, not a duplicate — directly analogous to their `plaudFileId` + `version_ms` loop, minus the cloud pull.
  - **Boot-time idempotent migrations.** For our SQLite `history.db` we want the same "run migrations on launch, advisory-guard concurrent starts" discipline — trivially simpler since we're single-process single-user, but the *idempotent on every boot* habit is the takeaway.
  - **Export parity as a first-class invariant** ("the proof users can leave"): a one-archive export/restore of every recording + transcript + speaker timeline is a strong design north star for an anti-Plaud tool.
  - **A `/health`-style readiness signal and a templated installer** translate to a clean Tauri first-run/onboarding flow (model download, mic/system-audio permission, storage path) and a build-green check.
  - **Zod-validated single config object / "never read env directly"** maps to a single validated settings/config struct in Rust rather than scattered reads.

- **Does NOT apply (cloud / web / multi-tenant machinery to drop):**
  - The entire **pull-from-Plaud-cloud sync worker, OTP/bearer-token auth, region redirects** — we *are* the recorder; capture is local (CoreAudio tap + mic), so there is no cloud to sync from. This is the philosophical inverse of riffado (companion-to-Plaud vs anti-Plaud).
  - **Postgres + Better Auth sessions + multi-tenant `userId`-on-every-query** — single-user desktop app means SQLite and no auth/tenancy layer; the `FOR UPDATE SKIP LOCKED` multi-process machinery, per-user fairness queues, rate limiting, and `IS_HOSTED` hosted-mode branches are all irrelevant.
  - **S3 storage, SMTP/Bark/webhook notifications, signed-webhook automation API, Docker Compose topology, GHCR image releases, reverse-proxy/SSL guidance** — all cloud-deployment concerns replaced by local filesystem (`~/Library/Application Support/com.pais.handy/`) and native macOS/Tauri packaging.
  - **Server-side AI proxying / egress hardening** — our privacy stance is stronger: ASR + diarization run fully on-device via ONNX Runtime, so there's no outbound AI egress to harden in the first place (their "local-AI path must keep working" caveat is, for us, the *only* path).

## Data Model — Drizzle ORM over Postgres (schema, migrations, queries, field encryption)

**What it is.** riffado's entire persistence layer: a single Drizzle schema file declaring 16 Postgres tables (auth, Plaud connection/devices, recordings, transcriptions, AI enhancements, settings, API keys, webhooks, rate-limit buckets, admin audit logs), a 25-step ordered SQL migration history, a handful of hand-written query modules for the hot/concurrent paths, and an application-level AES-256-GCM field-encryption helper layer that encrypts secrets and user content *before* they reach the columns. It is multi-tenant: almost every table is keyed by `userId` with `onDelete: cascade`.

**Key files.**
- `src/db/schema.ts` — the single source of truth; all `pgTable` declarations, columns, enums, indexes, unique constraints, FKs. Drizzle infers `$inferSelect`/`$inferInsert` types from it.
- `drizzle.config.ts` — drizzle-kit config: `dialect: "postgresql"`, schema path, `out: "./src/db/migrations"`, credentials from `process.env.POSTGRES_URL`.
- `src/db/index.ts` — instantiates the `db` client via `drizzle(postgres(env.DATABASE_URL), { schema })` (postgres-js driver); exports `db` and `schema`.
- `src/db/migrate.ts` — simple migration runner (`drizzle-orm/postgres-js/migrator`), used in dev.
- `src/db/migrate-idempotent.ts` — production runner: connect-with-retry + a Postgres **advisory lock** (`ADVISORY_LOCK_ID = 0x4f504c41`) so concurrent app instances don't run migrations simultaneously.
- `src/db/migrations/0000_*.sql` … `0024_*.sql` — the ordered DDL history (plus `meta/_journal.json` + per-step snapshots).
- `src/db/queries/admin.ts`, `admin-pricing-snapshot-csv.ts` — read-only aggregate/analytics queries (hosted admin dashboard); explicitly forbidden from selecting PII/secret columns.
- `src/db/queries/plaud-locks.ts` — `acquirePlaudConnectLock(tx, userId)`: per-user `pg_advisory_xact_lock` to serialize connect/reconnect upserts.
- `src/db/queries/rate-limit.ts` — `upsertRateLimitBucket`: atomic fixed-window counter via `INSERT ... ON CONFLICT DO UPDATE`.
- `src/db/queries/webhook-deliveries.ts` — `claimDueWebhookDeliveries` etc.: two-phase atomic claim of a webhook delivery queue with per-user fair-share and a processing lease.
- `src/lib/encryption.ts` — raw `encrypt`/`decrypt` (AES-256-GCM, `iv:authTag:ciphertext` hex), keyed by `ENCRYPTION_KEY` (64 hex chars).
- `src/lib/encryption/fields.ts` — `encryptText`/`decryptText`/`encryptJsonField`/`decryptJsonField`: versioned (`v1:`) field wrappers with legacy-plaintext pass-through.

**How it works.**

*Connection.* `src/db/index.ts` builds one postgres-js connection and wraps it with Drizzle, passing the full `schema` so relational/query helpers and `$inferSelect` types are available. It tolerates build-time absence of `DATABASE_URL` (returns a `{}` stub when `isBuild`), throwing only in real runtime.

*Identity.* Every primary key is `text("id").primaryKey().$defaultFn(() => nanoid())` — string PKs generated app-side with `nanoid`, not DB sequences or uuids. Timestamps are TZ-naive `timestamp` columns (the query layer deliberately casts ISO strings `::timestamp`, never `::timestamptz`, to avoid timezone skew — see the long comment in `rate-limit.ts`).

*Tenancy & cascade.* Auth tables (`users`, `sessions`, `accounts`, `verifications`) are owned by Better Auth. Every domain table carries `userId` → `users.id` with `{ onDelete: "cascade" }`, so deleting a user purges their entire graph. The two admin log tables are the exception: `adminUserId` uses `onDelete: "set null"` plus a snapshotted `adminUserEmail` so the audit trail survives a user purge.

*Migration evolution* (read in order, the story the 25 files tell):
- `0000` creates the initial 12 tables. Note `transcriptions` originally had `language` and a `recordings_plaud_file_id_unique` (global) constraint.
- `0001` adds `sessions.token` (Better Auth upgrade). `0002` drops `transcriptions.language`, adds `detected_language` + `transcription_type` ('server'/'browser').
- `0003`–`0008` progressively fatten `user_settings` (sync flags, playback, transcription, display, notifications, export; default `sync_interval` flips `15` → `300000`; `bark_push_key` → `bark_push_url`).
- `0009` adds the performance indexes retroactively + the `(userId, serialNumber)` device unique.
- `0010`–`0015` evolve the **Plaud connection**: add `api_base`, add then **drop** `refresh_token` (`0012`→`0014` — they abandoned a stored refresh token), add `plaud_email`, add `workspace_id`.
- `0011` adds `user_settings.summary_prompt` and the `ai_enhancements (recordingId, userId)` unique.
- `0016` adds `recordings.deleted_at` (soft-delete tombstone, issue #56) and `0019` swaps `recordings`' global `plaud_file_id` unique for a per-user `(userId, plaudFileId)` unique — the multi-tenancy correctness fix.
- `0018` introduces the hosted-only admin layer (`admin_action_log`, `admin_audit_log`, `users.suspended_at/reason`).
- `0019` adds the public-API surface: `api_keys` (+ `api_key_source` enum), `api_rate_limit_buckets`, `webhook_endpoints`, `webhook_deliveries`.
- `0021` adds `recordings.waveform_peaks` (jsonb). `0023` adds the privacy-scrubbed `install_script_hits` counter. `0024` **drops `storage_config` CASCADE** — per-user S3 config was removed in favor of a single instance-level storage config.

*Field-level encryption.* The DB stores ciphertext for secrets and user content. `src/lib/encryption.ts::encrypt` produces `iv:authTag:ciphertext` (AES-256-GCM); `fields.ts` adds a `v1:` version prefix and shape-regexes so legacy raw ciphertext and legacy plaintext both decrypt/pass-through gracefully. Applied at the call sites (not in the schema):
- `plaud_connections.bearerToken` — `encrypt` in `src/lib/plaud/persist-connection.ts`, `decrypt` in `client-factory.ts`.
- `api_credentials.apiKey` — `encrypt` in `src/app/api/settings/ai/providers/route.ts`.
- `webhook_endpoints.url` + `secret` — `encrypt` in `src/lib/webhooks/secrets.ts`.
- `recordings.filename` — `encryptText` (in `sync-recordings.ts`, `upload/route.ts`, and generated titles).
- `transcriptions.text` — `encryptText` (`transcribe-recording.ts`).
- `ai_enhancements.summary` (`encryptText`), `keyPoints` + `actionItems` (`encryptJsonField`).
- `user_settings.titleGenerationPrompt` + `summaryPrompt` (`encryptJsonField`).

*Concurrency idioms* live in `queries/`: a per-user `pg_advisory_xact_lock(hashtextextended('plaud_connect:'+userId, 0))` to serialize Plaud upserts; an `ON CONFLICT DO UPDATE` window-rollover rate-limit counter; and a two-phase webhook claim (rank-and-pick candidate IDs with `row_number() OVER (PARTITION BY user_id)` capped at `PER_USER_DELIVERY_LIMIT=10`/`DELIVERY_LIMIT=50`, then conditionally `UPDATE ... SET status='processing'` with a 15-min lease and `RETURNING` to learn which rows you actually won).

**Contracts & shapes.**

Core entity tables and their key columns (verbatim names):
```
users(id pk, email unique, email_verified, name, suspended_at, suspended_reason, created_at, updated_at)
sessions(id, expires_at, token unique, user_id→users, ip_address, user_agent, ...)   -- Better Auth
accounts(id, user_id→users, account_id, provider_id, access_token, refresh_token, expires_at, password, ...)
verifications(id, identifier, value, expires_at, ...)

plaud_connections(id, user_id→users, bearer_token [ENCRYPTED], api_base default 'https://api.plaud.ai',
                  plaud_email, workspace_id, last_sync, ...)
plaud_devices(id, user_id→users, serial_number varchar(255), name, model varchar(50), version_number,
              UNIQUE(user_id, serial_number), INDEX(user_id))

recordings(id, user_id→users, device_sn, plaud_file_id varchar(255), filename [ENCRYPTED],
           duration int /*ms*/, start_time, end_time, filesize int /*bytes*/, file_md5 varchar(32),
           storage_type varchar(10) /*'local'|'s3'*/, storage_path, downloaded_at, plaud_version,
           timezone, zonemins, scene, is_trash, waveform_peaks jsonb, deleted_at /*soft-delete*/, ...,
           UNIQUE(user_id, plaud_file_id),
           INDEX(user_id), INDEX(plaud_file_id), INDEX(user_id, start_time))

transcriptions(id, recording_id→recordings, user_id→users, text [ENCRYPTED],
               detected_language varchar(10), transcription_type varchar(10) default 'server',
               provider varchar(100), model varchar(100), created_at,
               INDEX(recording_id), INDEX(user_id))

ai_enhancements(id, recording_id→recordings, user_id→users, summary [ENCRYPTED],
                action_items jsonb [ENCRYPTED], key_points jsonb [ENCRYPTED], provider, model,
                UNIQUE(recording_id, user_id))

api_credentials(id, user_id→users, provider varchar(100), api_key [ENCRYPTED], base_url,
                default_model, is_default_transcription, is_default_enhancement, ...)

user_settings(id, user_id→users UNIQUE, sync_interval default 300000, auto_transcribe, auto_sync_enabled,
              ... ~30 prefs ..., default_providers jsonb, title_generation_prompt jsonb [ENCRYPTED],
              summary_prompt jsonb [ENCRYPTED], ai_output_language, ...)

api_keys(id, user_id→users, name, key_hash text UNIQUE, key_prefix varchar(16),
         source api_key_source('manual'|'device-flow') default 'manual',
         scopes jsonb $type<string[]> default ['read'], last_used_at, expires_at, revoked_at, ...,
         INDEX(user_id))

webhook_endpoints(id, user_id→users, url [ENCRYPTED], secret [ENCRYPTED], events jsonb<string[]>,
                  description, enabled, last_delivery_at, last_delivery_status, ...)
webhook_deliveries(id, endpoint_id→webhook_endpoints, user_id→users, recording_id→recordings,
                   event varchar(64), payload jsonb, status varchar(16), attempts, last_attempt_at,
                   next_attempt_at default now(), last_response_status, last_response_body, last_error, ...,
                   INDEX(status, next_attempt_at), INDEX(endpoint_id), INDEX(recording_id))

api_rate_limit_buckets(key pk, count default 0, reset_at, ...)             -- key is HMAC'd by caller
admin_audit_log(id, admin_user_id→users[set null], admin_user_email, route, method, ip, user_agent, ...)
admin_action_log(id, admin_user_id→users[set null], admin_user_email, action varchar(64),
                 target_user_id, target_resource_id, reason, before jsonb, after jsonb, ip, ...)
install_script_hits(day date, version text, count, PK(day, version))      -- no PII, hosted-only
```
Encrypted ciphertext format and key:
```ts
// src/lib/encryption.ts  — `iv:authTag:ciphertext` (hex), AES-256-GCM
// src/lib/encryption/fields.ts — versioned wrapper, prefix:
const VERSION_PREFIX = "v1:";  // text → "v1:<iv>:<tag>:<ct>" ; jsonb → { c: "v1:..." }
// ENCRYPTION_KEY must be exactly 64 hex chars (32 bytes); else throws.
```
Env vars / config keys:
```
DATABASE_URL        // runtime postgres-js connection (index.ts)
POSTGRES_URL        // drizzle-kit generate/studio (drizzle.config.ts)
ENCRYPTION_KEY      // 64 hex chars, AES-256-GCM field key
IS_HOSTED           // gates admin tables, suspension, install_script_hits writes
```
Inferred row types are exported implicitly, e.g. `typeof webhookDeliveries.$inferSelect`, `typeof webhookEndpoints.$inferSelect` (used as `ClaimedDelivery`).

**Notable patterns & decisions.**
- **App-generated `nanoid` text PKs** everywhere — portable, no DB round-trip for IDs, opaque to clients (an explicit anti-enumeration choice noted in `admin-pricing-snapshot-csv.ts`).
- **Application-layer encryption, not pgcrypto/column transparent encryption** — secrets and *user content* (transcripts, filenames, summaries) are ciphertext at rest; a DB exfil yields nothing without `ENCRYPTION_KEY`. The `v1:`-prefixed, regex-detected, pass-through design lets them roll encryption out over a live plaintext DB without a backfill migration.
- **PII firewall in the query layer**: `admin.ts` and `admin-pricing-snapshot-csv.ts` carry explicit comments + a `queries-pii.test.ts` guard forbidding selection of `text/summary/filename/bearerToken/apiKey`. Aggregates only.
- **Postgres as a coordination primitive**: advisory locks (migration singleton + per-user connect lock), `ON CONFLICT` upserts for rate limiting, and a `row_number()`-windowed fair-share claim for the webhook queue — all done in SQL rather than an external queue/Redis.
- **TZ-naive timestamps with explicit `::timestamp` casts** to dodge a postgres-js/Bun `ERR_INVALID_ARG_TYPE` Date-binding crash inside raw `sql\`\`` and to avoid silent timezone conversion.
- **Soft delete via tombstone** (`recordings.deleted_at`) — audio is hard-deleted from storage but the row is kept keyed by `plaudFileId` so re-sync doesn't resurrect it. The migration history visibly records a *correctness fix* (global → per-user `plaud_file_id` unique) and *dead ends* (the added-then-dropped `refresh_token`).

**Relevance to Plaude Local (anti-Plaud, local-first).**

*Worth adapting (the entity model, recast for SQLite):*
- The **core relational spine maps almost 1:1 onto our `history.db`**: a `recordings`-like table (duration ms, start/end, filesize, file_md5, `storage_path`, `is_trash`, soft-delete `deleted_at`, `waveform_peaks`) → a `transcriptions` table (text, `detected_language`, `provider`, `model`) → an `ai_enhancements` table (summary, action_items, key_points). This is a clean, proven shape to extend Handy's existing `session.rs`/SQLite history with. We already have sessions; adopt their **`transcriptions` and `ai_enhancements` split** (one recording → one transcript → one enhancement, enforced by the `UNIQUE(recording_id, user_id)` idiom; for us, drop `user_id`).
- **A speaker/segment table is the gap we must add** — riffado has none (its transcript is a single `text` blob, since Plaud/Whisper hand it back flat). Our diarization output (sherpa-onnx speaker turns) needs a first-class `segments(recording_id, speaker_label, start_ms, end_ms, text)` table that riffado simply doesn't model. This is where we diverge upward, not copy.
- **`waveform_peaks` as a write-once normalized-float jsonb array** for the player scrubber — directly reusable; generate once on first decode, store in SQLite.
- **The `user_settings` single-row preferences pattern** (one wide row of typed prefs with defaults: playback speed/volume, transcription quality, sort order, theme, retention, summary/title prompt presets-as-jsonb) maps onto our app settings. For a desktop single-user app this could even be the existing Handy settings store rather than a DB table.
- **Versioned, pass-through field encryption** (`v1:` prefix + legacy detection) is a genuinely good idea worth re-implementing in Rust for the one thing we *will* have secrets for: a user-supplied **OpenAI-compatible API key** if they opt into a cloud LLM for summaries. Note: we are local/offline-first, so most of riffado's encrypted columns (bearer tokens, webhook secrets) have no analogue — encryption matters far less when the DB never leaves the user's Mac, but an API key still deserves OS keychain or AES-at-rest.
- **Migration discipline**: their ordered numbered SQL + idempotent advisory-locked runner is a model for how to evolve `history.db` safely; for SQLite single-process we don't need the advisory lock, but the ordered-file + journal pattern is sound.

*Does NOT apply (cloud/web/multi-tenant machinery to drop entirely):*
- Everything keyed on `userId` / multi-tenancy / `onDelete: cascade` per user — we are single-user; drop `user_id` from every table.
- `users`, `sessions`, `accounts`, `verifications` (Better Auth), and the entire **admin layer** (`admin_audit_log`, `admin_action_log`, `users.suspended_at`, `IS_HOSTED`, the PII-firewall queries, pricing CSV export) — pure hosted-SaaS concerns.
- **All Plaud-cloud machinery**: `plaud_connections` (bearer token, workspace_id, api_base), `plaud_devices`, `recordings.plaud_file_id/plaud_version/scene/zonemins`, `plaud-locks.ts`, sync intervals. We are the *anti-Plaud* — we capture locally (mic + CoreAudio tap), we do not pull from Plaud's cloud. `device_sn`/device modeling is irrelevant unless/until the iPhone-as-capture path lands (and then it's our own device identity, not Plaud's).
- `api_keys` (public-API surface for third parties), `webhook_endpoints`/`webhook_deliveries`/the claim queue, `api_rate_limit_buckets`, `install_script_hits` — all exist because riffado is a *server* others call. A local desktop app exposes no public API and needs no rate limiting or webhook fan-out.
- Postgres-specific coordination (advisory locks, `ON CONFLICT` fair-share workers) is overkill for SQLite single-writer; keep the *ideas* (atomic upsert, soft-delete) but not the multi-process plumbing.

*AGPL-3.0 caveat:* riffado is AGPL-3.0. We may **study and re-derive** this data model (table shapes, the encrypt-at-rest idea, the transcript/enhancement split, soft-delete + waveform-peaks patterns) — schema *ideas* are not protectable — but we must **not copy `schema.ts`, the migration SQL, `encryption.ts`/`fields.ts`, or any query file source into Plaude Local**. Re-implement from scratch in Rust/SQLite (Handy already uses `rusqlite`/SQL directly), document it as independently authored, and do not paste their TypeScript. Given our app is itself meant to be open-source, AGPL contamination is the specific risk to avoid here — keep a clean-room boundary.

## Auth, Accounts & Multi-tenancy

**What it is.** riffado is a multi-tenant, single-instance web app where every user's data is row-scoped by `userId`. Authentication is email+password via **better-auth** (backed by Drizzle/Postgres), with a parallel bearer-token API-key path for the public `/api/v1/*` API. On top of normal users sits a hosted-only, env-gated operator/admin tier (email allowlist + IP allowlist + a short-lived HMAC "elevated" re-auth cookie) that can suspend users, force-disconnect their Plaud link, soft-delete recordings, and export PII — all written to append-only audit/action logs.

**Key files.**
- `src/lib/auth.ts` — better-auth server instance config (Drizzle adapter, email/password, password-reset hook, signup lockdown). Exports `auth` and `type Session`.
- `src/lib/auth-client.ts` — `createAuthClient` (better-auth/react); re-exports `useSession`, `signIn/Out/Up`, `forgetPassword`, `resetPassword`.
- `src/lib/auth-server.ts` — server-side gates: `getSession`, `requireAuth` (server components, redirects), `requireApiSession` (API routes, throws `AppError`), `redirectIfAuthenticated`. Adds the suspension check.
- `src/lib/auth-request.ts` — dual-auth resolver `authenticateRequest` for `/api/v1/*`: bearer API-key (HMAC lookup) OR session; API-key minting/hashing/masking helpers.
- `src/app/api/auth/[...all]/route.ts` — mounts better-auth's handler via `toNextJsHandler(auth)`.
- `src/db/schema.ts` — Drizzle tables: `users`, `sessions`, `accounts`, `verifications`, `apiKeys`, `adminAuditLog`, `adminActionLog`, plus per-user data tables.
- `src/lib/admin/guard.ts` — admin gate `evaluateAdminGate` + `requireAdminPage`/`requireAdminApi`/`requireAdminMutation`/`isAdminEmail`.
- `src/lib/admin/elevated-cookie.ts` — HMAC sign/verify of the `riffado_admin_elev` re-auth cookie + TTL checks.
- `src/lib/admin/ip-allowlist.ts` — fail-closed CIDR (v4/v6) allowlist matcher + `clientIpFromHeaders`.
- `src/lib/admin/suspension.ts` — `isSuspended` predicate + the enforcement-points doc-comment.
- `src/lib/admin/actions.ts` — transactional mutation dispatcher: `suspendUser`, `unsuspendUser`, `forceDisconnectPlaud`, `softDeleteRecording`, `logCsvExport`.
- `src/app/api/admin/reauth/route.ts` — password reprompt → sets elevated cookie.
- `src/app/api/admin/actions/*/route.ts` — thin admin mutation endpoints (suspend/unsuspend/disconnect-plaud/soft-delete-recording).
- `src/app/(admin)/admin/(gated)/layout.tsx` — runs the page gate on every render.
- `src/lib/env.ts` — Zod-validated env incl. all `ADMIN_*`, `IS_HOSTED`, `DISABLE_REGISTRATION`, `BETTER_AUTH_SECRET`, `API_TOKEN_HASH_SECRET`.

**How it works.**

*Session auth.* `auth.ts` builds the better-auth instance with `drizzleAdapter(db, { provider: "pg", schema, usePlural: true })`. `emailAndPassword` is enabled with `requireEmailVerification: false`, `disableSignUp: env.DISABLE_REGISTRATION` (the real signup lockdown — the `/register` page only mirrors the flag for UX), `resetPasswordTokenExpiresIn: 3600`, `revokeSessionsOnPasswordReset: true`, and a `sendResetPassword` callback delegating to `sendPasswordResetEmail`. The Next.js catch-all route `api/auth/[...all]/route.ts` exposes better-auth's `GET/POST`. The client (`auth-client.ts`) points `baseURL` at `window.location.origin`.

*Server gates.* `getSession()` calls `auth.api.getSession({ headers })`. `requireAuth()` (for server components) redirects to `/login` if no user, then does a **second DB read** of `users.suspendedAt` and redirects to `/suspended` if set. `requireApiSession(request)` is the API equivalent but throws typed `AppError(ErrorCode.AUTH_SESSION_MISSING, 401)` / `AppError(ErrorCode.ACCOUNT_SUSPENDED, 403)`. Note the suspension check is a per-request extra query layered on top of better-auth, not part of the session itself.

*Multi-tenant isolation.* There is **no** tenant/org/workspace table — isolation is purely "every data row carries `userId` and every query filters on the authenticated session's `user.id`." Example (`api/recordings/route.ts`): `where(and(eq(recordings.userId, session.user.id), isNull(recordings.deletedAt)))`. Every per-user table (`recordings`, `transcriptions`, `aiEnhancements`, `apiCredentials`, `userSettings`, `apiKeys`, `webhookEndpoints`, `plaudConnections`, `plaudDevices`) has a `userId` FK with `onDelete: "cascade"`, so deleting a user purges their entire footprint.

*API-key path (`/api/v1/*`).* `authenticateRequest(request)` returns `AuthenticatedRequest | null`. It pulls a `Bearer` token; if it starts with `op_` it treats it as an API key: `hashApiKey` (HMAC-SHA256 over `API_TOKEN_HASH_SECRET ?? BETTER_AUTH_SECRET`) → DB lookup on `apiKeys.keyHash` filtered by `isNull(revokedAt)` and `(expiresAt IS NULL OR expiresAt > now)`. On hit it checks suspension (`assertUserNotSuspended`), fire-and-forgets a `lastUsedAt` update, and returns `{ user: { id }, via: "api-key", apiKeyId }`. Otherwise it falls back to a better-auth session (`via: "session"`). Keys are minted by `createApiKey()` as `op_{base62 payload}{crc32-base62 checksum}`; only the HMAC hash + a 12-char display prefix are stored (`maskApiKey` renders `head…tail`); legacy nanoid keys still authenticate because format validation isn't in the auth path. Scopes are effectively read-only (`normalizeApiKeyScopes` collapses to `["read"]`).

*Admin tier.* All of it is inert unless `IS_HOSTED=true` AND `ADMIN_EMAILS` is non-empty. `evaluateAdminGate(opts)` runs, in order: `IS_HOSTED` check → non-empty `ADMIN_EMAILS` → optional `ADMIN_IP_ALLOWLIST` CIDR match (`clientIpFromHeaders` reads XFF[0]/X-Real-IP) → better-auth session present → session email in `ADMIN_EMAILS` (lowercased) → verify the `riffado_admin_elev` cookie's HMAC and that `payload.userId === session.user.id` → TTL checks. Any failure returns `null`, and on null the page gate calls `notFound()` (a 404, so the route's existence isn't leaked) while API gates throw `AppError(ErrorCode.NOT_FOUND, 404)`. Reads need the cookie within `ADMIN_REAUTH_TTL_MINUTES` (else `mode: "reauth"`); mutations need it within the tighter `ADMIN_MUTATION_TTL_MINUTES` (else 404). On success the gate inserts an `adminAuditLog` row (route/method/IP/UA) and returns `{ mode: "ok", user, elevatedIssuedAt }`. `isAdminEmail` is a render-only predicate that explicitly "does NOT verify the elevated cookie — never use to authorise."

*Elevated cookie / re-auth.* `POST /api/admin/reauth` re-runs the hosted+email+IP+session checks, then verifies the submitted password **directly** against the `accounts` row where `providerId = "credential"` using better-auth's `verifyPassword` (deliberately not `signInEmail`, to avoid creating a session row or rotating cookies). On success it sets `riffado_admin_elev = {userId}.{issuedAt}.{HMAC-SHA256(userId.issuedAt, BETTER_AUTH_SECRET)}` as `httpOnly`, `secure`, `sameSite: "strict"`, `path: "/"`, `maxAge = ADMIN_REAUTH_TTL_MINUTES*60`. Verification uses `timingSafeEqual`.

*Admin mutations.* Endpoints like `api/admin/actions/suspend/route.ts` call `requireAdminMutation(...)` then a function in `admin/actions.ts`. Each action runs in a single `db.transaction`: it re-validates `reason` (min 4 chars), takes a `SELECT ... FOR UPDATE` row lock on the target to serialize concurrent toggles, mutates, and writes the `adminActionLog` row **in the same transaction** (so a failed audit insert rolls back the mutation). Idempotent cases (double-suspend, disconnect-with-no-connection) still write a `*_noop` audit row. `suspendUser` sets `users.suspendedAt`/`suspendedReason`; suspension is then enforced cooperatively at `requireAuth`/`requireApiSession`/the sync worker.

*Note on a stale abstraction:* comments in `suspension.ts` and the gated layout reference a `middleware.ts` (redirects suspended users to `/suspended`, sets an `x-pathname` header). No `middleware.ts` exists in the repo — web-route suspension enforcement actually lives in `requireAuth`, and the layout's `hdrs.get("x-pathname")` currently resolves to nothing (collapses to `/admin`).

**Contracts & shapes.**

```ts
// src/lib/auth.ts
export const auth = betterAuth({
  database: drizzleAdapter(db, { provider: "pg", schema, usePlural: true }),
  emailAndPassword: {
    enabled: true,
    requireEmailVerification: false,
    disableSignUp: env.DISABLE_REGISTRATION,
    sendResetPassword: async ({ user, url }) => { await sendPasswordResetEmail(user.email, url); },
    resetPasswordTokenExpiresIn: 60 * 60, // 1 hour
    revokeSessionsOnPasswordReset: true,
  },
  secret: env.BETTER_AUTH_SECRET,
  baseURL: env.APP_URL,
});
export type Session = typeof auth.$Infer.Session;
```

```ts
// src/lib/auth-request.ts
export type AuthenticatedRequest = {
  user: { id: string };
  via: "session" | "api-key";
  apiKeyId?: string;
};
const API_KEY_PREFIX = "op_";        // bearer keys start with this
// hashApiKey: HMAC-SHA256(apiKey, API_TOKEN_HASH_SECRET ?? BETTER_AUTH_SECRET)
```

Better-auth tables (Drizzle, `src/db/schema.ts`, `usePlural: true`):
```
users(id, email UNIQUE, email_verified, name, suspended_at, suspended_reason, created_at, updated_at)
sessions(id, expires_at, token UNIQUE, user_id → users.id ON DELETE CASCADE, ip_address, user_agent, ...)
accounts(id, user_id → users, account_id, provider_id, access_token, refresh_token, expires_at, password, ...)
verifications(id, identifier, value, expires_at, ...)
api_keys(id, user_id → users CASCADE, name, key_hash UNIQUE, key_prefix(16),
         source: 'manual'|'device-flow', scopes jsonb DEFAULT ['read'],
         last_used_at, expires_at, revoked_at, ...)
```
Admin audit tables (append-only; actor FK `ON DELETE set null` + email snapshot for post-deletion attribution):
```
admin_audit_log(id, admin_user_id → users SET NULL, admin_user_email, route, method, ip, user_agent, created_at)
admin_action_log(id, admin_user_id → users SET NULL, admin_user_email, action(64),
                 target_user_id, target_resource_id, reason, before jsonb, after jsonb, ip, created_at)
```
Action names written: `suspend_user` / `suspend_user_noop`, `unsuspend_user`, `force_disconnect_plaud` / `_noop`, `soft_delete_recording`, `csv_export_{kind}`.

```ts
// src/lib/admin/guard.ts — gate results
type AdminGuardOk     = { mode: "ok";     user: {id;email}; elevatedIssuedAt: number };
type AdminGuardReauth = { mode: "reauth"; user: {id;email}; returnTo: string };
```

```
// src/lib/admin/elevated-cookie.ts
ADMIN_ELEVATED_COOKIE = "riffado_admin_elev"
cookie value = `${userId}.${issuedAt}.${HMAC_SHA256(`${userId}.${issuedAt}`, BETTER_AUTH_SECRET)}`
```

Env vars (`src/lib/env.ts`): `IS_HOSTED`, `DISABLE_REGISTRATION`, `BETTER_AUTH_SECRET` (≥32 chars, required at runtime), `API_TOKEN_HASH_SECRET` (≥32 chars, optional), `APP_URL`, `ENCRYPTION_KEY` (64 hex), `ADMIN_EMAILS` (comma list, lowercased), `ADMIN_IP_ALLOWLIST` (comma CIDR list), `ADMIN_REAUTH_TTL_MINUTES` (default 30, ≤1440), `ADMIN_MUTATION_TTL_MINUTES` (default 10, ≤60).

Routes: `/api/auth/[...all]` (better-auth), `/api/admin/reauth`, `/api/admin/actions/{suspend,unsuspend,disconnect-plaud,soft-delete-recording}`, `/api/admin/pricing-snapshot/export.csv`, `(auth)/{login,register,forgot-password,reset-password}`, `(admin)/admin/(gated)/*`, `/suspended`.

**Notable patterns & decisions.**
- **Operator vs. user identity split.** Admin identity lives in `ADMIN_EMAILS` env, never in the DB — there is no `role` column. This keeps "who is an operator" out of the data plane entirely.
- **404-on-deny everywhere for admin** to avoid leaking the route's existence to non-operators.
- **Two-tier TTL** (looser for reads, tight for mutations) on a single re-auth cookie, with `timingSafeEqual` HMAC verification — a lightweight "sudo mode."
- **Audit-in-transaction**: an unaudited admin mutation is treated as worse than a refused one, so the audit insert shares the mutation's transaction and `*_noop` rows are written even for idempotent calls. `FOR UPDATE` row locks serialize concurrent toggles so `before` snapshots are accurate.
- **Fail-closed CIDR matcher** hand-rolled for v4+v6 (no dependency); empty allowlist = disabled, non-empty-but-unparseable = deny. Honest about trusting `x-forwarded-for` and warns at startup.
- **Cooperative suspension** (a tombstone column, not a hard kill) checked at every gate via an extra query rather than baked into the session.
- **API keys**: prefix + CRC32 checksum format, store only the HMAC + display prefix, fire-and-forget `lastUsedAt`, scopes pinned to read-only.
- Stale `middleware.ts` references in comments — the described middleware was never committed; enforcement is in the server gates.

**Relevance to Plaude Local (anti-Plaud, local-first).**

What does NOT apply (cloud/web/multi-tenant machinery to drop entirely): the whole admin/operator tier (`src/lib/admin/*`, `(admin)` routes, `admin_audit_log`/`admin_action_log`, IP allowlist, re-auth elevated cookie, suspension) — all gated on `IS_HOSTED` and meaningless for a single-user on-device Mac/iPhone app where the user owns the machine; the `users/sessions/accounts/verifications` better-auth tables and the email/password login flow; SMTP password-reset; `userId` row-scoping (you have exactly one implicit user); `DISABLE_REGISTRATION`; multi-tenant cascade-delete design. Plaude Local has no tenants and no remote attacker on the data plane — Handy already stores everything in `~/Library/Application Support/com.pais.handy/history.db` with OS-level file permissions as the trust boundary.

What is worth ADAPTING into the local Tauri/Rust app: (1) **The bearer-API-key design** if you ever expose a localhost HTTP control surface for the iPhone-as-capture / Mac-as-brain split — `op_`-prefix + checksum format, store only an HMAC (not the raw key), a 12-char display prefix for the UI, `revokedAt`/`expiresAt` columns, and `lastUsedAt`. This maps cleanly to a small SQLite table + a Rust `verify_token` Tauri command guarding the device-pairing endpoint, and is the right shape for pairing a phone to the desktop without a cloud account. (2) **The single-transaction "mutation + audit row" pattern with `before`/`after` JSON** is a clean model for a local edit/undo or change-history log on sessions/recordings (who-changed-nothing aside, it gives you a local activity trail). (3) **The `suspendedAt`-style cooperative tombstone + soft-delete `deletedAt`** is directly useful: Plaude's session/recording model should use soft-delete tombstones so a re-sync/re-index doesn't resurrect deleted recordings, exactly as riffado does for `recordings.deletedAt`. (4) **Zod-validated, fail-with-clear-messages env/config loading** (`env.ts`) is a good template for validating a local config file (model paths, storage dir) at startup. (5) **`timingSafeEqual` + HMAC over a stored secret** is the correct primitive if you add any local pairing-secret verification.

AGPL/license caveat: riffado is **AGPL-3.0**. Treat all of the above as design/architecture you may study and re-implement from scratch in Rust/React — do **not** copy any TypeScript source (the API-key CRC32 helpers, the CIDR matcher, the cookie HMAC code, schema definitions) into Plaude Local. Re-derive the idea, write your own implementation. The data-model shapes (column names, the `op_` key format) are facts/interfaces and safe to mirror; the concrete code is copyleft.

## Plaud Cloud Integration Layer

**What it is.** The `src/lib/plaud/` module is riffado's complete client for Plaud's undocumented private cloud API: it authenticates a Plaud account via email OTP, discovers the account's region/workspace, mints a workspace-scoped bearer token, and lists/downloads the user's cloud recordings (the audio files captured by Plaud hardware devices). It is the import-from-Plaud half of the app — the bridge that pulls a user's existing Plaud-cloud library into riffado.

**Key files.**
- `src/lib/plaud/auth.ts` — OTP send-code + verify (login), region `-302` redirect handling, JWT-expiry decode, `/user/me` email lookup.
- `src/lib/plaud/servers.ts` — region catalogue (global/EU/APAC/custom), the spoofed browser `User-Agent`, and HTTPS+`.plaud.ai` host validation.
- `src/lib/plaud/workspace.ts` — workspace list, personal-workspace selection, workspace-token minting, stale-cache retry, and an SSRF URL sanitiser.
- `src/lib/plaud/client.ts` — `PlaudClient` class: the authenticated request engine (UT→WT escalation, 429/5xx retry) and the recording endpoints (`listDevices`, `getRecordings`, `getTempUrl`, `downloadRecording`, `updateFilename`).
- `src/lib/plaud/fetch.ts` — `plaudFetch`, a `fetch`-shaped wrapper that injects browser-fingerprint headers and optionally routes through a rotating proxy pool.
- `src/lib/plaud/proxy.ts` — Webshare proxy-pool integration (cache, rotation, blacklist) used to evade Plaud's bot/IP filtering.
- `src/lib/plaud/parse.ts` — `safeParseJson`, status-aware JSON parser that maps HTTP status onto typed `AppError`s but tolerates 2xx business-status bodies.
- `src/lib/plaud/persist-connection.ts` — validates a token and writes the connection + device rows to Postgres (Drizzle).
- `src/lib/plaud/client-factory.ts` — builds a `PlaudClient` by decrypting a stored token.
- `src/lib/plaud/sync-rate-limit.ts` — per-user 429 limiter for the sync endpoint.
- `src/lib/plaud/types.ts` — deprecated re-export shim → `@/types/plaud`.
- `src/types/plaud.ts` — the canonical wire-shape interfaces (recording, workspace, temp-url, device).

**How it works.**

*1. Login (OTP, two legs).* `plaudSendCode(email, apiBase)` POSTs `{ username: email }` to `/auth/otp-send-code`. Plaud replies with a JSON envelope carrying a business-level `status` field (not just HTTP status). If `status === -302` and `data.domains.api` is present, the account lives in a different region: the function strips trailing slashes off the returned API host and **recurses into itself** with the regional base, bounded by `MAX_REGION_REDIRECTS = 3` (else `PLAUD_REGION_REDIRECT_LOOP`). On `status === 0` it returns `{ token, apiBase }` — `token` is a short-lived OTP-session token, and `apiBase` is the *resolved* regional base the caller must keep using. The second leg, `plaudVerifyOtp(code, otpToken, apiBase)`, POSTs `{ code, token: otpToken }` to `/auth/otp-login` and extracts the user access token from either `access_token` or `data.access_token`. There is no password path in this layer — login is OTP-only.

*2. Token decode (UX only).* `decodeAccessTokenExpiry` base64url-decodes the JWT payload's `exp` claim *without signature verification* — explicitly commented "never authorise from this," used only to show a "reconnect" hint.

*3. Region/workspace discovery.* `servers.ts` hardcodes three real Plaud hosts plus a `custom` slot. `persistPlaudConnection` (in `persist-connection.ts`) calls `listPlaudWorkspaces(userToken, apiBase)` → GET `/team-app/workspaces/list?need_personal_workspace=true` (auth: **user token / UT**). `pickPersonalWorkspaceId` selects the workspace whose `workspace_type === "0"` (personal), falling back to the first. It then constructs a `PlaudClient`, calls `listDevices()` to validate the token, encrypts the token (`encrypt()`), and upserts a `plaudConnections` row + one `plaudDevices` row per returned device, all inside a Drizzle transaction guarded by `acquirePlaudConnectLock` (advisory lock against concurrent connects).

*4. User-token → workspace-token escalation.* This is the central auth design. `PlaudClient.request()` lazily calls `ensureWorkspaceToken()` before every call. `resolveWorkspaceToken` (workspace.ts): if a `workspaceId` is cached, it POSTs (empty `{}` body) to `/user-app/auth/workspace/token/{workspaceId}` (auth: UT) to mint a **workspace token (WT)**; on a *stale* 4xx it re-lists workspaces and retries; a 401 is treated as non-stale (token genuinely dead → `PLAUD_INVALID_TOKEN`). The minted WT becomes the `Authorization: Bearer` for subsequent data calls. Crucially, if minting fails for any reason, `fetchWorkspaceToken` swallows the error, logs a warning, and sets `workspaceFallbackToUt = true` — the client then transparently uses the **user token** as the bearer instead. So `bearer = this.workspaceToken ?? this.userToken`.

*5. Recording endpoints.* On `PlaudClient`:
- `listDevices()` → GET `/device/list`.
- `getRecordings(skip=0, limit=99999, isTrash=0, sortBy="edit_time", isDesc=true)` → GET `/file/simple/web?skip=…&limit=…&is_trash=…&sort_by=…&is_desc=…`. The 99999 default fetches the whole library in one shot.
- `getTempUrl(fileId, isOpus=true)` → GET `/file/temp-url/{fileId}?is_opus=1|0`, returning short-lived signed download URLs (`temp_url`, optional `temp_url_opus`).
- `downloadRecording(fileId, preferOpus=true)` chains: it calls `getTempUrl`, prefers `temp_url_opus` when present, then `plaudFetch`es the signed URL and returns a `Buffer` (`arrayBuffer()` → `Buffer.from`).
- `updateFilename(fileId, filename)` → PATCH `/file/{fileId}` with `{ filename }` (writes the rename back to Plaud cloud).
- `testConnection()` just wraps `listDevices()` in try/catch.

*6. Transport hardening.* Every call goes through `plaudFetch`, which spoofs a full Chrome request fingerprint (`sec-ch-ua`, `origin: https://web.plaud.ai`, `referer`, `sec-fetch-*`, etc., caller headers winning) and — when `WEBSHARE_API_KEY` is set and the host is `*.plaud.ai` — routes through a Webshare HTTP proxy via Bun's non-standard `proxy` RequestInit option, rotating once on 403/407/network-error. `PlaudClient.request` adds exponential-backoff retry (`MAX_RETRIES=3`, `INITIAL_RETRY_DELAY=1000`, `2**retryCount`) on 429 (honouring `Retry-After`), 5xx, and `TypeError: fetch` network errors. `safeParseJson` reads the body as text first so it can parse 2xx envelopes that carry negative business statuses (the `-302` redirect), and maps HTTP 401/429/5xx/4xx onto typed `AppError` codes.

**Contracts & shapes.**

Endpoint paths (verbatim), all relative to a regional `apiBase` like `https://api.plaud.ai`:
```
POST /auth/otp-send-code                                   body: { username: email }
POST /auth/otp-login                                       body: { code, token }
GET  /user/me                                              auth: Bearer <accessToken>
GET  /team-app/workspaces/list?need_personal_workspace=true   auth: UT
POST /user-app/auth/workspace/token/{workspaceId}          auth: UT, body: {}
GET  /device/list
GET  /file/simple/web?skip=&limit=&is_trash=&sort_by=&is_desc=
GET  /file/temp-url/{fileId}?is_opus=1
PATCH /file/{fileId}                                       body: { filename }
```

The recording shape (`src/types/plaud.ts`, verbatim):
```ts
export interface PlaudRecording {
    id: string;
    filename: string;
    keywords: string[];
    filesize: number;
    filetype: string;
    fullname: string;
    file_md5: string;
    ori_ready: boolean;
    version: number;
    version_ms: number;
    edit_time: number;
    edit_from: string;
    is_trash: boolean;
    start_time: number; // Unix timestamp in milliseconds
    end_time: number; // Unix timestamp in milliseconds
    duration: number; // Duration in milliseconds
    timezone: number;
    zonemins: number;
    scene: number;
    filetag_id_list: string[];
    serial_number: string;
    is_trans: boolean;
    is_summary: boolean;
}

export interface PlaudRecordingsResponse {
    status: number;
    msg: string;
    data_file_total: number;
    data_file_list: PlaudRecording[];
}

export interface PlaudTempUrlResponse {
    status: number;
    temp_url: string;
    temp_url_opus?: string;
}
```

Workspace-token envelope (verbatim) — note WT carries its own expiry + refresh token:
```ts
export interface PlaudWorkspaceTokenResponse {
    status: number;
    msg?: string;
    data: {
        status: number;
        workspace_token: string;
        expires_in: number;
        wt_expires_at: number;
        refresh_token: string;
        refresh_expires_in: number;
        refresh_expires_at: number;
        workspace_id: string;
        member_id: string;
        role: string;
    };
}
```
Workspace selection rule: `workspaces.find((w) => w.workspace_type === "0")` = personal. Device shape: `{ sn, name, model, version_number }`.

Region catalogue + spoofed UA (`servers.ts`, verbatim):
```ts
export const PLAUD_USER_AGENT =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
// global  → https://api.plaud.ai
// eu      → https://api-euc1.plaud.ai      (Frankfurt)
// apse1   → https://api-apse1.plaud.ai     (Singapore)
// custom  → ""   (must be https + host === "plaud.ai" || endsWith(".plaud.ai"))
```

Env vars (`src/lib/env.ts`): `WEBSHARE_API_KEY`, `PLAUD_PROXY_SCOPE` (`"all" | "api-only"`, default `all`; `api-only` excludes `resource.plaud.ai` from proxying), `PLAUD_SYNC_RATE_LIMIT_PER_MINUTE`. Constants: `MAX_REGION_REDIRECTS=3`, `MAX_RETRIES=3`, `INITIAL_RETRY_DELAY=1000`, proxy `CACHE_TTL_MS=5*60_000`, `MAX_PROXY_ROTATIONS=1`. Persisted columns (`plaudConnections`): `userId, bearerToken (encrypted), apiBase, plaudEmail, workspaceId`.

**Notable patterns & decisions.**
- **Business-status-over-HTTP envelope.** Plaud returns HTTP 200 with a JSON `status` field where `0` = success and `-302` = region redirect. `safeParseJson` deliberately does *not* gate on `res.ok`, reading text first so the `-302` body survives. This is a hard-won detail about Plaud's API contract.
- **Two-tier token model.** UT (account-wide, from OTP) is used only to mint short-lived WTs (workspace-scoped, with refresh tokens); WTs authorise data calls, with silent UT fallback when minting fails. Cleanly modeled but entirely a function of Plaud's multi-workspace cloud.
- **Bot-evasion as a first-class concern.** Full Chrome header spoofing + a rotating residential-proxy pool (Webshare) with blacklist and 403/407 rotation. This signals Plaud actively fingerprints/blocks non-browser clients. Uses Bun's runtime-specific `proxy` RequestInit (`@ts-expect-error`).
- **Inlined SSRF guard.** `safePlaudUrl` is duplicated inline in `workspace.ts` rather than imported, with a comment "so CodeQL recognises it" — pragmatic over DRY for static-analysis appeasement.
- **Whole-library single fetch.** `limit=99999` pulls everything at once rather than paginating.
- **Recursion for redirects, lazy single-flight for WT** (`workspaceFetchInFlight` dedupes concurrent mints).

**Relevance to Plaude Local (anti-Plaud, local-first).**

This subsystem is the single most "not us" part of riffado, and it is worth being explicit about why. riffado *imports from Plaud's cloud*; Plaude Local is the **anti-Plaud** — it captures audio on-device (mic + macOS system audio) and never depends on Plaud's servers. So essentially the entire module is **cloud/multi-tenant machinery that does NOT apply**: OTP login to `plaud.ai`, region `-302` redirects, the UT→WT escalation, workspace lists, Webshare proxy bot-evasion, `temp-url` signed downloads, Postgres `plaudConnections`/`plaudDevices` tables, the per-user sync rate-limiter — all of it presupposes a remote Plaud account, a server with an egress IP to disguise, and a multi-user web backend. None map onto a single-user local Tauri app.

What is worth **adapting** is narrow and conceptual, not code:
1. **The recording metadata model.** `PlaudRecording` is a battle-tested schema for what a voice-memo record needs: `id, filename, file_md5, start_time/end_time/duration (ms), timezone/zonemins, scene, keywords, serial_number, is_trans, is_summary, filesize, filetype`. Our `handy/src-tauri/src/managers/session.rs` session/SQLite model can borrow these *fields* (especially `file_md5` for dedupe, ms-precision `start/end/duration`, `timezone/zonemins`, and the `is_trans`/`is_summary` "pipeline-stage" booleans) as a checklist — re-expressed as our own Rust structs.
2. **The optional, opt-in "import my old Plaud recordings" escape hatch.** If we ever want to let a switching user pull their existing Plaud library *once* into local storage, this file is the only known-working reference for the endpoint sequence (`otp-send-code` → `otp-login` → `workspaces/list` → `workspace/token/{id}` → `file/simple/web` → `file/temp-url/{id}`) and the `-302`/UT-vs-WT quirks. That would be a one-shot migration importer, not a runtime dependency, and must be clearly separated from the always-local core.

**AGPL caveat (hard rule):** riffado is AGPL-3.0. We may study this design and these endpoint/shape facts (the Plaud API surface itself is not riffado's IP), but we must **not copy `auth.ts`/`client.ts`/`workspace.ts`/etc. source into Plaude Local**. Any importer we build must be a clean-room reimplementation in Rust/TS from the documented endpoint behaviour above, not a paste of these files. Also note the bot-evasion layer (spoofed UA, residential proxies) is both legally/ToS-sensitive and entirely unnecessary for a local app — do not port it.

Key files (absolute): `/private/tmp/claude-501/-Users-vladvrinceanu-Desktop-PROGETTI-ANTYGRAVITY-Plaude-Local/5cf6ff9d-1432-4926-b37c-12af24c6d541/scratchpad/repos/riffado/src/lib/plaud/{auth,workspace,client,servers,fetch,proxy,parse,persist-connection,sync-rate-limit,client-factory,types}.ts` and `/private/tmp/.../riffado/src/types/plaud.ts`.

## Sync Engine & Scheduling

**What it is.** The pipeline that pulls a user's recordings from the Plaud cloud, deduplicates and stores them, and (optionally) chains into transcription. Despite the "Auto-Sync" docs implying a scheduler, there is **no server-side cron**: scheduling lives entirely in the browser (a React `useAutoSync` hook on a timer), and the server side is a single stateless function, `syncRecordingsForUser`, invoked per HTTP `POST /api/plaud/sync`.

**Key files.**
- `src/lib/sync/sync-recordings.ts` — the whole server sync engine: pagination, batching, per-recording dedup/state, in-process coalescing, notifications, transcription hand-off.
- `src/app/api/plaud/sync/route.ts` — the only entry point; auth + rate-limit gate, then calls `syncRecordingsForUser`.
- `src/lib/plaud/sync-rate-limit.ts` — `enforcePlaudSyncRateLimit`, the cross-process correctness gate (returns a 429 `NextResponse` or `null`).
- `src/lib/rate-limit.ts` — `consumeRateLimitBucket`, DB-backed token bucket (HMAC'd keys, fail-open).
- `src/lib/transcription/transcribe-recording.ts` — `transcribeRecording`, the downstream worker the sync engine fans new recordings into.
- `src/db/schema.ts` (`recordings`, `transcriptions` tables) — the persisted state.
- `docs/AUTO_SYNC.md` — describes the *client* scheduler (`useAutoSync` hook, intervals, throttling, visibility detection). Note: it documents `NEXT_PUBLIC_*` env vars and a `sync-config.ts` that are client-only and partly aspirational ("future UI controls").

**How it works.**

The flow is: `POST /api/plaud/sync` → `enforcePlaudSyncRateLimit` → `syncRecordingsForUser(userId)` → `runSyncRecordingsForUser` → paginate Plaud → batch → `processRecording` per item → optional notifications + `queueTranscriptions`.

*Scheduling.* There is no `cron`/`setInterval` server-side. `docs/AUTO_SYNC.md` is explicit that "each user's sync timer is independent and runs in their browser" — the `useAutoSync` hook (`src/hooks/use-auto-sync.ts`, not in scope here) fires `POST /api/plaud/sync` on a timer, on tab-visibility regain, and on manual button press. The server is deliberately stateless; the only persisted scheduling artifact is `plaudConnections.lastSync`, written at the end of every run.

*Two-layer dedup of concurrent syncs.* (1) **In-process coalescing**: `syncRecordingsForUser` keeps a module-level `inFlightSyncs = new Map<string, Promise<SyncResult>>()`. If a sync for that `userId` is already running in this process, the caller `await`s the same promise and gets `{ ...shared, inProgress: true }` rather than launching a second pull. The `finally` block deletes the map entry. (2) **Cross-process correctness**: the route calls `enforcePlaudSyncRateLimit` *before* the engine, which consumes a DB-backed token bucket keyed `plaud-sync:user:${userId}` over a 60s window — this is what actually prevents two server instances from syncing the same user simultaneously.

*Pagination.* `runSyncRecordingsForUser` loads the `plaudConnections` row (bearer token, apiBase, workspaceId), `userSettings`, and the `users` row (bails with "User is suspended" if `suspendedAt` is set). It builds a `SyncContext`, then constructs a Plaud client and a per-user storage provider. It loops pages via `plaudClient.getRecordings(skip, PAGE_SIZE, 0, "edit_time", true)` — fetching 50 at a time, sorted by edit time descending. It stops when: a page returns empty; a page is shorter than `PAGE_SIZE` (last page); `MAX_PAGES` (20 → 1000 recordings max) is hit; or — the clever bit — **two consecutive pages yield zero new/updated recordings** (`consecutiveEmptyPages >= 2`). Because Plaud is sorted newest-edit-first, this is an *incremental* early-exit: once you walk past the recently-changed head into already-synced history, you stop, instead of always paging to the end. A full sync (first run) naturally walks until a short page.

*Concurrency.* Within each page, recordings are processed in slices of `BATCH_CONCURRENCY = 5` via `processBatch`, which runs `Promise.allSettled` over `processRecording`. Pages are sequential; batches within a page are sequential; the 5 items in a batch run in parallel. Rejected promises are folded into `errors` as `"Batch processing error: ..."`.

*Per-recording state machine (`processRecording`).* This is where dedup and the recording lifecycle live. For each `PlaudRecording`:
1. Look up an existing row by `(plaudFileId, userId)`.
2. **Version dedup**: `versionKey = plaudRecording.version_ms.toString()`. If the existing row's `plaudVersion === versionKey`, return `{ status: "skipped" }` (unchanged). This is the incremental-update detector.
3. **Tombstone guard**: if `existingRecording.deletedAt` is set, skip — a user-deleted recording must not be resurrected on re-sync (issue #56).
4. **Download** the audio: `plaudClient.downloadRecording(id, false)` → buffer.
5. **Unique storage key**: `uniqueStorageKey` builds `${userId}/${safeName}.mp3`, probing for collisions (`(2)`, `(3)`, … up to 100) against *other* `plaudFileId`s, falling back to `${userId}/${plaudFileId}.mp3`. Filename is sanitized of `/\:*?"<>|`.
6. **Upload** to storage (`audio/mpeg`).
7. **Persist.** For a *new* recording: `INSERT ... RETURNING id`, then `emitEvent("recording.synced", …)`, return `{ status: "new", recordingId }`. For an *existing* one: a transaction re-selects the row `FOR UPDATE` and re-checks `deletedAt` — guarding against a concurrent DELETE during the slow download/upload. If it was tombstoned mid-flight, it skips and **best-effort deletes the orphaned blob** it just uploaded; otherwise it `UPDATE`s (including a fresh `plaudVersion`/`updatedAt`) and `emitEvent("recording.updated", …)`, returning `{ status: "updated" }`.
8. Any throw → `{ status: "error", error: "Failed to sync ${filename}: ..." }`. Errors never abort the run; they accumulate.

*The "transcription state machine."* There is **no** `pending/transcribing/done/failed` status column. State is implicit: a recording is "transcribed" iff a `transcriptions` row with non-empty `text` exists for it. After the sync loop, if `context.autoTranscribe` and there are `pendingTranscriptionIds` (the new recordings), it **fires `queueTranscriptions` without `await`** (`.catch` logs) so the HTTP response returns immediately. `queueTranscriptions` loops the IDs **sequentially** calling `transcribeRecording`, swallowing per-recording errors. `transcribeRecording` itself is idempotent: it short-circuits `{ success: true }` if a transcript already exists and `force` is false, and uses typed `TranscribeErrorCode` discriminators (`RECORDING_NOT_FOUND | NO_TRANSCRIPTION_PROVIDER | RECORDING_DELETED | TRANSCRIPTION_FAILED`) instead of persisted status. **There is no retry/backoff and no failure persistence** — a failed auto-transcribe is logged and dropped; the user re-triggers it manually.

*End of run.* Updates `plaudConnections.lastSync` (and `workspaceId` if the client resolved a new one), then sends email (`sendNewRecordingEmail`) and Bark push (`sendNewRecordingBarkNotification`) notifications only when `newRecordings > 0` and the respective channel is enabled. Notification failures append to `errors` but don't fail the sync.

**Contracts & shapes.**

```ts
// src/lib/sync/sync-recordings.ts — tuning constants
const SYNC_CONFIG = {
    PAGE_SIZE: 50,
    BATCH_CONCURRENCY: 5,
    MAX_PAGES: 20,
} as const;
```
```ts
interface SyncResult {
    newRecordings: number;
    updatedRecordings: number;
    errors: string[];
    pendingTranscriptionIds: string[];
    inProgress?: boolean; // true when coalesced into a running sync
}
interface SyncContext {
    userId: string;
    autoTranscribe: boolean;
    emailNotifications: boolean;
    barkNotifications: boolean;
    notificationEmail: string | null;
    barkPushUrl: string | null;
}
// per-recording outcome
{ status: "new" | "updated" | "skipped" | "error"; recordingId?; filename?; error? }
```
```ts
// In-process coalescing key store
const inFlightSyncs = new Map<string, Promise<SyncResult>>();
```
```ts
// Plaud page fetch (skip, limit, ?, sortField, descending)
plaudClient.getRecordings(skip, SYNC_CONFIG.PAGE_SIZE, 0, "edit_time", true);
// response.data_file_list: PlaudRecording[]
```
```ts
// recordings table — sync-relevant columns (src/db/schema.ts)
plaudFileId   : varchar(255).notNull()      // dedup key (with userId)
plaudVersion  : varchar(50).notNull()       // = version_ms; change detector
fileMd5       : varchar(32).notNull()
storageType   : varchar(10) // 'local' | 's3'
storagePath   : text.notNull()
downloadedAt  : timestamp
isTrash       : boolean.notNull().default(false)
deletedAt     : timestamp                   // soft-delete tombstone (#56)
// unique(userId, plaudFileId) = "recordings_user_id_plaud_file_id_unique"
// index "recordings_plaud_file_id_idx"
```
```ts
// transcriptions table — NOTE: no status/state column. "done" == row exists with text.
text, detectedLanguage, transcriptionType('server'|'browser'), provider, model
```
```ts
// Rate limit (src/lib/plaud/sync-rate-limit.ts) — cross-process gate
const WINDOW_MS = 60_000;
bucketKey = `plaud-sync:user:${userId}`; // HMAC'd before DB
limit = env.PLAUD_SYNC_RATE_LIMIT_PER_MINUTE;  // 429 + Retry-After when exceeded
```
```ts
// Webhook events emitted
emitEvent("recording.synced",  userId, recordingId);  // new
emitEvent("recording.updated", userId, recordingId);  // updated
```
Env keys: `DEFAULT_STORAGE_TYPE` (`local`|`s3`, default `local`), `PLAUD_SYNC_RATE_LIMIT_PER_MINUTE`. Client scheduler env (browser, from the doc): `NEXT_PUBLIC_SYNC_INTERVAL` (default 300000), `NEXT_PUBLIC_MIN_SYNC_INTERVAL` (60000), `NEXT_PUBLIC_SYNC_ON_MOUNT`, `NEXT_PUBLIC_SYNC_ON_VISIBILITY`.

**Notable patterns & decisions.**
- **No real cron.** Scheduling is browser-timer-driven; the server is a stateless POST handler. "Auto-sync" = client polling, not a backend job runner. The doc's "Future Enhancements" even lists Service-Worker background sync as not-yet-done.
- **Incremental via sort + early-exit, not cursors.** Relying on Plaud's `edit_time DESC` ordering plus a "2 consecutive zero-result pages → stop" heuristic is a cheap incremental sync without storing a high-water mark. `version_ms` → `plaudVersion` string compare is the change detector. Trade-off: a recording edited far back in history past the early-exit window can be missed until a fuller walk.
- **Two-tier concurrency control**: in-process `Map` coalescing (fast path, single instance) layered under a DB token-bucket rate limit (correctness across instances). The comment explicitly delegates cross-process correctness to the rate limiter.
- **`SELECT … FOR UPDATE` re-check after slow I/O.** The update path re-locks and re-checks `deletedAt` after the download/upload, with orphaned-blob cleanup if a delete raced in — careful TOCTOU handling.
- **Fire-and-forget transcription** with sequential processing and *no retry/backoff/persistence* of failures. Idempotency comes from "transcript row already exists" rather than a state machine. `Promise.allSettled` + accumulating `errors[]` means one bad recording never aborts the batch or run.
- **Fail-open rate limiter**: if the bucket store (DB) is down, `consumeRateLimitBucket` allows the request and logs, rather than taking the API down.

**Relevance to Plaude Local (anti-Plaud, local-first).**

*What does NOT apply (cloud/web/multi-tenant machinery):* The entire premise — "discover in Plaud cloud, download, store" — is exactly what we are the *anti*-thesis of. We capture audio on-device (mic + system-audio tap into `session.rs`), so there is no remote source to poll, no `plaudConnections`/bearer token/workspace, no `downloadRecording`, no S3/storage provider abstraction, no `plaudFileId`/`plaudVersion`/`fileMd5` dedup, no email/Bark notifications, no per-user rate limiting or multi-tenant HMAC'd buckets, no browser `useAutoSync` polling. The `userId` scoping throughout collapses to nothing in a single-user desktop app.

*What design to ADAPT into our Tauri/Rust+React app:*
1. **The implicit-state idempotency idea is worth stealing, but invert it.** riffado has *no* status column and relies on "row exists with text"; that costs them retry/backoff and observability. For our recorder, a session's transcription is long-running and crash-prone, so we *should* add an explicit state column on our SQLite session/segment rows — `pending | transcribing | done | failed` — exactly the state machine riffado lacks. That lets a Rust worker resume after a crash (anything stuck in `transcribing` on startup → reset to `pending`) and gives the Sessions UI a live indicator.
2. **A bounded worker with `BATCH_CONCURRENCY`-style limiting.** Their slice-of-5 `Promise.allSettled` maps cleanly to a Rust bounded queue / `tokio::Semaphore`(N) feeding the diarizer + ASR ONNX runtimes, so a backlog of recorded sessions transcribes without oversubscribing CPU. Keep their "one failure never aborts the batch" discipline (`Result` accumulation instead of early return).
3. **In-process coalescing (`inFlightSyncs` map)** translates to a single-flight guard so the UI's "transcribe now" button and any auto-transcribe-on-stop don't double-run the same session — a `Mutex<HashSet<SessionId>>` or an actor.
4. **The `FOR UPDATE` re-check after slow I/O / tombstone guard** is a good reminder: when our transcription job finishes a long ASR pass, re-check the session wasn't deleted mid-flight before writing results (and clean up partial artifacts) — same TOCTOU lesson, applied to local file + SQLite row.
5. **Skip the rate limiter entirely**; a single local user has no thundering-herd to throttle. Replace it with a simple "is a job already running" check.

*AGPL caveat:* riffado is AGPL-3.0. We may study this architecture (state machine, coalescing, bounded concurrency, early-exit, TOCTOU handling) and re-implement it idiomatically in Rust, but must **not** copy `sync-recordings.ts` source, its type definitions, or doc text verbatim into Plaude Local. The Plaud-coupled identifiers (`PlaudRecording`, `getRecordings`, `version_ms`) are irrelevant to us anyway, which makes a clean-room re-implementation natural.

## Transcription: OpenAI-compatible Providers & AI

**What it is.** Riffado's server-side transcription + AI layer: a provider-agnostic abstraction that routes a stored audio file to one of three "transcription styles" (OpenAI-compatible Whisper `/v1/audio/transcriptions`, OpenAI-compatible chat-completions `input_audio`, or native Google Gemini `generateContent`) against **any** OpenAI-compatible `baseURL`, then runs follow-on LLM calls (title + summary/key-points/action-items) through the same credential set. A separate in-browser Whisper path (Transformers.js in a Web Worker) exists for fully-client transcription.

**Key files.**
- `src/lib/transcription/transcribe-recording.ts` — orchestrator `transcribeRecording(userId, recordingId, opts)`: provider selection, audio download/decrypt, style routing, DB upsert, auto-title, webhook emission.
- `src/lib/ai/provider-presets.ts` — the provider catalog (`PROVIDER_PRESETS`), `TranscriptionStyle` union, `getTranscriptionStyle(provider)`, hosted/local visibility filtering.
- `src/lib/transcription/format.ts` — Whisper response-format selection (`getResponseFormat`), request param builder (`buildTranscriptionParams`), and `parseTranscriptionResponse` (handles `verbose_json` / `json` / `diarized_json`).
- `src/lib/transcription/chat-transcribe.ts` — `chatTranscribe(...)`: sends base64 audio as a chat `input_audio` content part (OpenRouter etc.).
- `src/lib/transcription/gemini-transcribe.ts` — `geminiTranscribe(...)`: native `@google/generative-ai` `inlineData` path, MIME mapping, 20 MB inline cap.
- `src/lib/transcription/compress-audio.ts` — `maybeCompressForWhisper(...)`: ffmpeg child-process re-encode to mono Opus to beat Whisper's 25 MiB limit.
- `src/lib/transcription/audio-file.ts` — `buildAudioFile(...)`: turns a `Buffer` into a `File` with correct extension/MIME (OGG magic-byte sniff).
- `src/lib/transcription/browser-transcriber.ts` + `worker.ts` — client-side Whisper via `@xenova/transformers` in a Web Worker.
- `src/lib/ai/chat-completion-params.ts` — `buildChatCompletionParams(...)`: chooses `max_tokens` vs `max_completion_tokens` by model-name prefix.
- `src/lib/ai/generate-title.ts` — `generateTitleFromTranscription(...)`: LLM title generation with prompt presets.
- `src/lib/ai/prompt-presets.ts` / `src/lib/ai/summary-presets.ts` — prompt template catalogs (title presets; summary presets + AI-output-language directive).
- `src/lib/ai/validate-base-url.ts` — `validateAiBaseUrl(...)`: rejects loopback base URLs in hosted mode (SSRF guard).
- `src/app/api/recordings/[id]/summary/route.ts` — the actual summary-generation call site (`POST`/`GET`/`DELETE`), JSON parsing of the LLM response.

**How it works.**

*Transcription flow* (`transcribeRecording`): (1) Loads the recording scoped by `userId` and `isNull(deletedAt)` — tombstoned recordings are skipped to avoid re-creating rows for deleted audio. (2) Idempotent short-circuit: if a transcription row already has `text` and `opts.force` is false, it returns the decrypted existing text immediately (the sync worker relies on this; the manual "Re-transcribe" route passes `force: true`). (3) Provider selection: an explicit `opts.providerId` (user-scoped lookup in `apiCredentials`) wins, otherwise the row with `isDefaultTranscription = true`. (4) Loads `userSettings` for `defaultTranscriptionLanguage`, `autoGenerateTitle`, `syncTitleToPlaud` (note: `transcriptionQuality` is read but immediately discarded — `void quality`). (5) `decrypt(credentials.apiKey)`, downloads the audio via the storage provider, decrypts the filename, and builds a `File` via `buildAudioFile`. (6) `model = opts.model || credentials.defaultModel || "whisper-1"`. (7) **Style routing** via `getTranscriptionStyle(credentials.provider)`:
- `"gemini"` → `geminiTranscribe` (native `GoogleGenerativeAI`, `inlineData` base64, MIME map, 20 MB cap).
- `"chat"` → constructs `new OpenAI({ apiKey, baseURL })` and calls `chatTranscribe`, which base64-encodes audio into a chat message `input_audio` part. The `input_audio` part is cast `as unknown as { type: "text"; text: string }` to bypass the SDK's type that doesn't yet model audio input. Only `mp3`/`wav` accepted (`ChatTranscribeFormatError` otherwise).
- `"whisper"` (default) → `getResponseFormat(model)` picks the format, `maybeCompressForWhisper` re-encodes if over ~24 MiB, then `openai.audio.transcriptions.create(buildTranscriptionParams(...), { timeout })` with a per-request timeout (default 1 h) so long Whisper jobs don't hit the SDK's 10-min default. `parseTranscriptionResponse` extracts text + language.

(8) Persistence is wrapped in a transaction that re-selects the recording `FOR UPDATE` and aborts (throws a `RECORDING_TOMBSTONED` symbol) if it was deleted mid-flight; otherwise upserts the `transcriptions` row (text encrypted via `encryptText`, plus `detectedLanguage`, `transcriptionType: "server"`, `provider`, the actual `model`). (9) If `autoGenerateTitle`, calls `generateTitleFromTranscription` and stores the encrypted title as the recording filename (optionally pushed back to Plaud). (10) Emits `transcription.completed` / `transcription.failed` webhooks. All failures funnel into a typed `TranscribeErrorCode`.

*`getResponseFormat`* maps model name → format: `diarize` substring → `diarized_json`, `gpt-4o*` → `json`, else `verbose_json`. `parseTranscriptionResponse` flattens diarized segments into `"${speaker}: ${text}"` lines joined by `\n` (no per-segment timestamps preserved); `verbose_json` yields `{ text, language }`; plain `json` yields `{ text, null }`.

*Compression* (`maybeCompressForWhisper`): passes through under the byte threshold; otherwise spawns `ffmpeg` (`-ac 1 -c:a libopus -b:a Nk -application voip -f ogg`) starting at 12 kbit/s, halving down to a 6 kbit/s floor until it fits the 25 MiB hard cap, else throws (recommending chunking).

*AI generation*: both title and summary load credentials preferring `isDefaultEnhancement` then falling back to `isDefaultTranscription`, build an `OpenAI` client, swap a Whisper `defaultModel` for a chat model (`gpt-4o-mini`, or provider-specific `llama-3.1-8b-instant`/`meta-llama/...`/`openai/gpt-4o-mini` chosen by sniffing `baseUrl` substrings), truncate the transcript (2000 chars for titles, 8000 for summaries), inject it into the `{transcription}` placeholder, and call `chat.completions.create(buildChatCompletionParams(...))`. The summary route then strips ```` ```json ```` fences and `JSON.parse`s into `{ summary, keyPoints, actionItems }`, falling back to treating the whole response as `summary` on parse failure. Output language is steered via a **system-message** directive (`getAiOutputLanguageDirective`) kept separate from the JSON-shape rules in the user prompt.

**Contracts & shapes.**

Provider config (the abstraction's core type):
```ts
// src/lib/ai/provider-presets.ts
export type TranscriptionStyle = "whisper" | "chat" | "gemini";

export interface ProviderPreset {
    name: string;
    baseUrl: string;
    placeholder: string;
    defaultModel: string;
    transcriptionStyle: TranscriptionStyle;
    fetchAudioModels?: boolean;
    knownTranscriptionModels?: readonly string[];
}
// presets: OpenAI(""), Groq(api.groq.com/openai/v1), Together AI(api.together.xyz/v1),
// OpenRouter(openrouter.ai/api/v1, style "chat"), LM Studio(http://localhost:1234/v1),
// Ollama(http://localhost:11434/v1), Google Gemini(style "gemini"), Custom("")
export const LOCAL_PRESET_NAMES = new Set(["LM Studio", "Ollama"]);
```

Transcript/segment parsing types & result format:
```ts
// src/lib/transcription/format.ts
export type ResponseFormat = "diarized_json" | "json" | "verbose_json";
// diarized → segments.map(seg => `${seg.speaker}: ${seg.text}`).join("\n")
// (uses openai/resources TranscriptionDiarized & TranscriptionVerbose)

// src/lib/transcription/transcribe-recording.ts
export type TranscribeErrorCode =
    | "RECORDING_NOT_FOUND" | "NO_TRANSCRIPTION_PROVIDER"
    | "RECORDING_DELETED"   | "TRANSCRIPTION_FAILED";
export interface TranscribeResult {
    success: boolean; error?: string; errorCode?: TranscribeErrorCode;
    text?: string; detectedLanguage?: string | null;
}
```

DB shapes (Postgres / Drizzle):
```ts
// transcriptions: text (encrypted), detected_language(varchar 10),
//   transcription_type(varchar 10, 'server'|'browser'), provider, model
// ai_enhancements: summary(text, enc), action_items(jsonb, {c:ciphertext}),
//   key_points(jsonb), provider, model; UNIQUE(recording_id, user_id)
// api_credentials: provider, api_key(text, encrypted), base_url(text),
//   default_model(varchar 100), is_default_transcription(bool),
//   is_default_enhancement(bool)
```

Token-param prefixes & env vars:
```ts
const MAX_COMPLETION_TOKENS_PREFIXES = ["gpt-5", "o1", "o3", "o4"]; // → max_completion_tokens
// env: WHISPER_REQUEST_TIMEOUT_MS (default 3_600_000),
//      WHISPER_MAX_BYTES (default 24 MiB), WHISPER_COMPRESS_BITRATE_KBPS (default 12)
```

Verbatim transcribe instruction reused by chat + gemini paths:
```
Transcribe the attached audio verbatim. Output only the transcript text — no preamble, no summary, no timestamps, no speaker labels, no markdown.
```

**Notable patterns & decisions.** One `apiCredentials` row models any OpenAI-compatible endpoint; the only branch is the three-way `TranscriptionStyle`, keyed off the provider *name* preset, not capability detection. The SDK type-hole cast for `input_audio` shows the chat-audio path predates SDK support. Token-limit-param selection is opt-in by model-name prefix so non-OpenAI providers keep `max_tokens`. Both AI prompts force JSON-without-fences but defensively strip fences and `JSON.parse` with a graceful fallback. Whisper's 25 MiB ceiling is treated as the common case (meetings), so ffmpeg-to-Opus down-bitrate is built in rather than an edge path. Heavy concurrency-safety: every write re-checks the soft-delete tombstone inside a `FOR UPDATE` transaction to serialize against the delete handler. Credentials are encrypted at rest (`decrypt`/`encryptText`/`encryptJsonField`), transcript ciphertext never leaves the DB un-decrypted. Hosted-mode SSRF guard (`validateAiBaseUrl`) blocks loopback/`0.0.0.0`/`::1`/`127.x` and IPv4-mapped loopback.

**Relevance to Plaude Local (anti-Plaud, local-first).**

*Adapt (design, re-implement clean — do not copy):*
- The **`ProviderPreset` + `TranscriptionStyle` abstraction** is the single best takeaway: a Rust enum (`Whisper { base_url } | Chat { base_url } | GeminiNative`) lets one config screen target OpenAI/Groq/Together/**Ollama/LM Studio** *and* your bundled local Whisper/Parakeet — useful as an optional "cloud boost" alongside the on-device `transcribe-rs` path. The LM Studio / Ollama localhost presets are exactly your local story.
- `getResponseFormat` → `diarized_json` parsing (`"speaker: text"` flattening) maps directly onto your sherpa-onnx diarization output; consider keeping segment+timestamp structure rather than flattening to a string, since your timeline UI needs it.
- The **prompt presets** (title + summary/key-points/action-items, the JSON envelope `{summary, keyPoints, actionItems}`, fence-stripping, output-language directive in a system message) are a ready-made local "AI notes" feature — these are *templates/strings*, conceptually reusable but see license caveat.
- `buildChatCompletionParams` model-prefix logic and the per-request long-timeout idea are pragmatic gotchas worth replicating when you call any OpenAI-compatible endpoint from Rust (`reqwest`).

*Does NOT apply (cloud/web/multi-tenant machinery):* per-row `userId` scoping, Postgres/Drizzle, S3 storage provider, at-rest field encryption (your data is single-user on-device — local DB encryption is optional, not multi-tenant secrecy), the hosted-mode loopback SSRF guard (you *want* localhost), Plaud title sync, webhook emission, and the 25 MiB Whisper compression dance (irrelevant for on-device inference; only matters if you bolt on a cloud provider). The Transformers.js/WASM browser path is a browser concern — your equivalent is native ONNX in Rust.

*AGPL-3.0 caveat:* riffado is AGPL-3.0. Study the architecture freely, but **do not copy source verbatim** into Plaude Local — including the prompt-template strings and type definitions, which are copyrightable expression. Re-implement the provider abstraction and write your own prompts; cite this as a design reference only.

## In-Browser / Local Transcription (Transformers.js Whisper WASM)

**What it is.** A self-contained, zero-API-key transcription path that runs Whisper entirely in the user's browser via `@xenova/transformers` (Transformers.js) on WebAssembly, off the main thread in a Web Worker. It is riffado's only genuinely on-device, no-cloud transcription route — heavily marketed ("transcribe free in your browser") but, notably, **not wired into the server-side recording pipeline**; it exists as a standalone client module plus a worker.

**Key files.**
- `src/lib/transcription/browser-transcriber.ts` — public API: `BrowserTranscriber` class + `transcribeInBrowser()` convenience fn. Spawns/owns the worker, reads the audio `File`, marshals messages, resolves a `TranscriptionResult`.
- `src/lib/transcription/worker.ts` — the Web Worker. Imports `pipeline` from `@xenova/transformers`, lazily builds an `automatic-speech-recognition` pipeline, runs inference, posts results back.
- `src/types/transcription.ts` — shared `TranscriptionResult` interface and `TranscriptionModel` union.
- `package.json` — declares `"@xenova/transformers": "^2.17.2"`.
- `src/components/landing/the-math.tsx`, `src/components/landing/pricing.tsx`, `src/components/landing/hero.tsx` — marketing copy referencing "Whisper via Transformers.js, no key required."
- `src/lib/transcription/transcribe-recording.ts` — the *real* server pipeline (Gemini / OpenAI-compatible chat). Shown here to prove the browser path is **not** referenced by it.

**How it works.** Data flow, tracing the actual code:

1. **Worker boot.** `BrowserTranscriber.initialize()` (browser-transcriber.ts:26) constructs `new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })` — an ES-module worker resolved by the bundler. It returns a `Promise` that resolves when the worker posts `{ type: "ready" }`. The worker posts `ready` *immediately on load* (worker.ts:67), before any model is fetched — so "ready" means "worker booted," not "model loaded."

2. **Model selection.** `transcribe(audioFile, model, onProgress)` maps a friendly model id to a HuggingFace repo via the module-level `MODEL_MAP` (browser-transcriber.ts:13): `whisper-tiny → "Xenova/whisper-tiny"`, etc. Default is `whisper-base`.

3. **Audio marshalling.** A `FileReader` reads the `File` with `readAsArrayBuffer` (browser-transcriber.ts:123). The resulting `ArrayBuffer` is `postMessage`'d to the worker as `{ type: "transcribe", audioData, model: modelPath }`. (Note: it's passed by structured-clone copy, **not** transferred — no `[buffer]` transfer list.)

4. **Lazy pipeline init in the worker.** `initTranscriber(model)` (worker.ts:10) memoizes a single `transcriber` and builds it with `pipeline("automatic-speech-recognition", model, { revision: "main" })`. Transformers.js fetches the ONNX model weights + tokenizer from the HuggingFace CDN on first use and caches them (browser cache / IndexedDB). `self.ONNX_CACHE = false` (worker.ts:6) is set with a `@ts-expect-error` to disable the local ONNX cache.

5. **Inference + chunking.** The worker posts `{ type: "progress", status: "transcribing" }`, then calls the pipeline with `{ return_timestamps: false, chunk_length_s: 30, stride_length_s: 5 }` (worker.ts:44-48). This is Whisper's standard 30-second windowing with a 5-second overlap stride to stitch long audio — Transformers.js handles the windowing/overlap-merge internally; the app does not chunk audio itself.

6. **Result & language.** On success it posts `{ type: "complete", text: result.text, detectedLanguage: result.chunks?.[0]?.language || "en" }` (worker.ts:50-54). Language detection is best-effort: it reads the first chunk's `language` field and falls back to `"en"`. The class's per-call `messageHandler` (browser-transcriber.ts:89) resolves `{ text, detectedLanguage }`; `progress` messages drive the optional `onProgress(status)` callback; `error` rejects.

7. **Teardown.** `terminate()` kills the worker and resets `isReady`. The one-shot `transcribeInBrowser()` wrapper (browser-transcriber.ts:142) does `initialize → transcribe → terminate` in a `try/finally`, throwing away the loaded model after a single file — convenient but it forfeits the memoized pipeline, forcing a re-download/re-init on the next call.

**Disconnect from production.** A grep shows `transcribeInBrowser` / `BrowserTranscriber` have **no importers** anywhere in `src/` (only the type union members appear elsewhere). The actual server pipeline `transcribeRecording()` routes only to `geminiTranscribe` or `chatTranscribe` (cloud APIs) based on `getTranscriptionStyle(credentials.provider)`. So the in-browser path is currently dead/standalone code despite prominent landing-page promotion.

**Contracts & shapes.**
```ts
// src/types/transcription.ts
export interface TranscriptionResult {
    text: string;
    detectedLanguage: string;
}
export type TranscriptionModel =
    | "whisper-tiny"
    | "whisper-base"
    | "whisper-small";
```
```ts
// src/lib/transcription/browser-transcriber.ts — model id → HF repo
const MODEL_MAP: Record<TranscriptionModel, string> = {
    "whisper-tiny": "Xenova/whisper-tiny",
    "whisper-base": "Xenova/whisper-base",
    "whisper-small": "Xenova/whisper-small",
};
```
```ts
// Worker message protocol (postMessage payloads)
// main → worker:
{ type: "transcribe", audioData: ArrayBuffer, model: string /* HF repo */ }
// worker → main:
{ type: "ready" }                                   // posted on worker load
{ type: "progress", status: "transcribing" }
{ type: "complete", text: string, detectedLanguage: string }
{ type: "error", error: string }
```
```ts
// worker.ts — pipeline construction + inference options
pipeline("automatic-speech-recognition" as PipelineType, model, { revision: "main" });
// @ts-expect-error -- disable local model cache in browser
self.ONNX_CACHE = false;
pipe(audioData, {
    return_timestamps: false,
    chunk_length_s: 30,
    stride_length_s: 5,
});
```
```jsonc
// package.json
"@xenova/transformers": "^2.17.2"
```

**Notable patterns & decisions.**
- **Off-main-thread inference** via a `type: "module"` Web Worker resolved with `new URL("./worker.ts", import.meta.url)` — the canonical Next.js/Vite-friendly worker pattern that keeps the heavy WASM/ONNX work from freezing the UI.
- **Per-call `messageHandler` that self-removes** (`removeEventListener` on complete/error) — avoids leaking listeners across multiple transcriptions on a shared worker. Clean.
- **Lazy, memoized model load** inside the worker (`if (!transcriber)`), so repeated `transcribe()` calls on the *same* `BrowserTranscriber` instance reuse the loaded weights — but `transcribeInBrowser()` defeats this by terminating after one file.
- **Weak progress story:** `onProgress` only ever receives the single string `"transcribing"`. Transformers.js emits rich `progress_callback` events (per-file download %, model load); none are surfaced. For a multi-hundred-MB first download (`whisper-small`), the user sees no percentage.
- **`whisper-base` default** is a deliberate quality/size trade-off; `whisper-small` is offered but no `medium`/`large` (browser memory limits).
- **No transfer of the ArrayBuffer** (structured clone copy) and **`return_timestamps: false`** (so no segment timeline) are simplicity choices that diverge from the cloud path, which produces speaker/segment data.
- **Surprising:** the heavily-marketed "free in your browser" feature is not actually invoked anywhere in the app's recording flow — it's a built-but-unwired module.

**Relevance to Plaude Local (anti-Plaud, local-first).**
- **Closest ethos match, but architecturally inverted from our stack.** This is the one riffado subsystem that is genuinely on-device, no-key, no-cloud — exactly our goal. But our local advantage is *native*: we run Whisper/Parakeet through `transcribe-rs` + ONNX Runtime in Rust (`handy/src-tauri/...`), which is dramatically faster and memory-safer than WASM. We should **not** adopt Transformers.js/WASM; it would be a downgrade for a desktop app that already has native ORT.
- **ADAPT — the worker/message-protocol shape.** The clean `{ type, ... }` message protocol (`transcribe` → `progress`/`complete`/`error`) and the self-removing per-job handler is a good model for our **frontend↔backend** boundary: in Tauri this maps to a `command` invocation plus `emit`/`listen` events (e.g. `transcribe_session` command emitting `transcription-progress` / `transcription-complete`). The contract `{ text, detectedLanguage }` and an explicit progress channel are worth mirroring — and we should do progress *better* than riffado (emit real percentages, since our native pipeline can report them).
- **ADAPT — model-id → asset mapping & lazy/memoized load.** `MODEL_MAP` and the "load once, reuse" memoization mirror what our Rust model manager should do: a small enum of model sizes mapped to bundled local files, loaded lazily and cached for the session. Crucially, **invert their `transcribeInBrowser()` mistake** — keep the model resident across recordings, don't tear it down per file.
- **ADAPT — the 30s/5s chunking idea, but it's already native to us.** Their reliance on `chunk_length_s: 30, stride_length_s: 5` is just Whisper's standard windowing; our `transcribe-rs` does this internally too. Worth noting only as confirmation of sane defaults.
- **DOES NOT APPLY — CDN model fetch.** Their models stream from the HuggingFace CDN on first run (cloud dependency, offline-fragile). That is antithetical to our offline-ready clone; we **bundle** models in `handy/src-tauri/resources/models/...` and auto-install on first run. Keep doing that.
- **DOES NOT APPLY — the marketing/landing machinery and the unused-feature situation.** Pure web product surface; ignore.
- **AGPL-3.0 caveat.** riffado is AGPL-3.0 (confirmed `LICENSE` header). The worker/transcriber source is short and tempting to lift, but copying it into Plaude Local would impose AGPL copyleft on our app. We may **study the message protocol, the model-map idea, and the chunking parameters as design**, then re-implement independently in Rust/TS — do not copy `worker.ts` / `browser-transcriber.ts` verbatim or as light edits.

## Audio Handling & Export Formats

**What it is.** riffado's audio subsystem is split into two halves: (1) an *ingestion* path that validates uploaded audio files and extracts their duration via a pure-JS metadata parser (no `ffprobe`/`ffmpeg` binary), and (2) a *client-side* waveform-peaks pipeline that decodes audio in the browser and caches the result. Separately, a single export route renders all of a user's transcripts into JSON / TXT / SRT / VTT.

**Key files.**
- `src/app/api/recordings/upload/route.ts` — upload endpoint; extension allowlist, size cap, MD5, duration parse via `music-metadata`, storage write + DB insert with rollback.
- `src/lib/audio/waveform.ts` — `decodePeaks()`: Web Audio decode → normalized amplitude envelope. Pure browser code.
- `src/hooks/use-waveform.ts` — `useWaveform()` React hook: orchestrates fetch → decode → render → best-effort persist, with abort/stale-result guards.
- `src/app/api/recordings/[id]/peaks/route.ts` — write-once persistence of the peaks array (validation + idempotent conditional UPDATE).
- `src/app/api/recordings/[id]/audio/route.ts` — serves the stored audio bytes with HTTP Range support; supplies the buffer that `decodePeaks` consumes.
- `src/app/api/export/route.ts` — the entire export-format engine (JSON/TXT/SRT/VTT) in one `switch`.
- `src/components/settings-sections/export-section.tsx` — settings UI: format dropdown + "Export All" / "Create Backup" buttons (blob-download pattern).
- `src/lib/utils.ts` — `AUDIO_MIME_TYPES` map, `getAudioMimeType()`, `audioFilenameWithExt()`.
- `src/tests/regressions/58-upload-no-ffprobe.test.ts` — regression proof that duration parsing needs no system binary.
- `src/db/schema.ts` — `recordings.duration / filesize / fileMd5 / waveformPeaks` columns; `userSettings.defaultExportFormat`.

**How it works.**

*Ingestion (`upload/route.ts`).* `POST` is wrapped in `apiHandler` and gated by `requireApiSession`. It reads `multipart/form-data`, pulls `file`, and validates in order: (1) the entry is a `File`; (2) `file.size <= MAX_FILE_SIZE` (500 MB) else `413 FILE_TOO_LARGE`; (3) the lowercased extension is in `ACCEPTED_EXTENSIONS` (`.mp3 .mp4 .m4a .wav .ogg .opus .webm .aac .flac`) else `400 INVALID_FILE_FORMAT`. The file is read once into a `Buffer`. A storage key is built as `${userId}/uploaded-${nanoid()}${ext}`, and **content type is derived from the validated extension via `getAudioMimeType()`, never from `file.type`** — an explicit stored-XSS guard. `createHash("md5")` produces `fileMd5`. Duration comes from `getAudioDurationMs(buffer, contentType)`, which calls `music-metadata`'s `parseBuffer(buffer, { mimeType, size }, { duration: true })`; the `duration: true` flag forces a full scan for containers that don't expose duration in headers (Chrome-recorded WebM/Opus, raw ADTS AAC). On parse failure it logs and returns `0`; **duration 0 ⇒ `422 INVALID_FILE_FORMAT` ("does not contain a valid audio stream")** — this doubles as audio-stream validation. Only then does it upload to the storage provider and `db.insert(recordings)`, encrypting `filename` with `encryptText()`. If the DB insert throws, it deletes the just-uploaded storage object to avoid orphans, then rethrows.

*Waveform pipeline.* `useWaveform()` decides whether to auto-decode: if `initialPeaks` exist (server already has them) it starts in `ready`; otherwise, for `durationMs <= AUTO_DECODE_MAX_MS` (30 min) it auto-runs, and for longer recordings it sets status `skipped` and waits for a manual `decode()` gesture. `runDecode()` fetches `/api/recordings/[id]/audio` (Range-capable), gets an `ArrayBuffer`, and calls `decodePeaks(buf, DEFAULT_BUCKETS)`. `decodePeaks` lazily creates a shared `AudioContext` (with `webkitAudioContext` fallback), `ctx.decodeAudioData()`, then for each of `buckets` (default 500, clamped 32–2048) walks `samplesPerBucket` frames, averages across channels, takes the absolute max → per-bucket peak, then normalizes the whole array to `[0,1]` by dividing by the global max. The hook then POSTs the array to `/api/recordings/[id]/peaks` as best-effort fire-and-forget. Concurrency is handled with an `AbortController` per fetch plus a `currentIdRef` stale-guard (decode results for a switched-away recording are dropped); the CPU-bound decode itself is not interruptible.

*Peaks persistence (`peaks/route.ts`).* Validates `{ peaks: number[] }`: array, length `MIN_PEAKS(32)..MAX_PEAKS(2048)`, every value a finite number in `[0,1]`; rounds each to 3 decimals. Then a **write-once** pattern: select the recording (scoped to user, not soft-deleted); if `waveformPeaks` already set, return `{ stored: false }`; else `UPDATE ... SET waveform_peaks = ... WHERE id=? AND user_id=? AND deleted_at IS NULL AND waveform_peaks IS NULL` — the `IS NULL` predicate makes concurrent POSTs idempotent (first writer wins) and avoids resurrecting tombstoned rows.

*Export (`export/route.ts`).* `GET` resolves format precedence as `?format=` query param → `userSettings.defaultExportFormat` → `"json"`. It loads all the user's non-deleted recordings and their transcriptions, **decrypts** `transcription.text` and `recording.filename` up front into a `transcriptionMap` and `decryptedRecordings`, then a `switch (exportFormat)` emits the body + `Content-Type` + `Content-Disposition: attachment; filename="recordings-YYYY-MM-DD.<ext>"`:
- **json** — pretty-printed array of `{ id, filename, duration, startTime, filesize, transcription }`.
- **txt** — `filename \n ISO-startTime \n text \n\n---\n\n` per recording.
- **srt** — numbered cues; timecode `HH:MM:SS,mmm` built from `getUTC*` of `startTime` and `startTime + duration`. Recordings with no transcript are dropped via `flatMap(... return [])`.
- **vtt** — `WEBVTT\n\n` header then cues with `HH:MM:SS.mmm` (dot, not comma) timecodes.
- default ⇒ `400 INVALID_INPUT`.

Note the SRT/VTT timing is a **whole-recording single cue** keyed off absolute clock time formatted as if it were an elapsed offset — there is no per-segment/per-speaker timeline in the export despite riffado having diarized transcripts elsewhere.

**Contracts & shapes.**

```ts
// upload/route.ts
const ACCEPTED_EXTENSIONS = new Set([
  ".mp3", ".mp4", ".m4a", ".wav", ".ogg", ".opus", ".webm", ".aac", ".flac",
]);
const MAX_FILE_SIZE = 500 * 1024 * 1024; // 500 MB
// duration via: parseBuffer(buffer, { mimeType, size }, { duration: true }) → format.duration (s) → round(*1000) ms
```

```ts
// src/lib/audio/waveform.ts
export const DEFAULT_BUCKETS = 500;
export const AUTO_DECODE_MAX_MS = 30 * 60 * 1000; // 30 min auto-decode ceiling
export interface PeaksResult { peaks: number[]; sampleCount: number; durationSeconds: number; }
export async function decodePeaks(arrayBuffer: ArrayBuffer, buckets?: number): Promise<PeaksResult>;
```

```ts
// peaks/route.ts — validation bounds
const MAX_PEAKS = 2048;
const MIN_PEAKS = 32;
// body: { peaks: number[] }  // each finite, in [0,1], rounded to 3 dp
```

```ts
// src/lib/utils.ts
const AUDIO_MIME_TYPES: Record<string,string> = {
  ".mp3":"audio/mpeg", ".mp4":"audio/mp4", ".m4a":"audio/mp4", ".wav":"audio/wav",
  ".ogg":"audio/ogg", ".opus":"audio/ogg", ".webm":"audio/webm", ".aac":"audio/aac", ".flac":"audio/flac",
}; // fallback "audio/mpeg"
```

```ts
// db/schema.ts — recordings
duration:  integer("duration").notNull(),       // milliseconds
filesize:  integer("filesize").notNull(),        // bytes
fileMd5:   varchar("file_md5", { length: 32 }).notNull(),
storageType: varchar("storage_type",{length:10}).notNull(), // 'local' | 's3'
storagePath: text("storage_path").notNull(),
waveformPeaks: jsonb("waveform_peaks"),          // JSON number[] in [0,1], ~N=500, write-once
// db/schema.ts — userSettings
defaultExportFormat: varchar("default_export_format",{length:10}).notNull().default("json"), // 'json'|'txt'|'srt'|'vtt'
```

```
Routes:
POST /api/recordings/upload            (multipart "file")
GET  /api/recordings/[id]/audio        (Range-capable byte serving)
POST /api/recordings/[id]/peaks        ({ peaks })
GET  /api/export?format=json|txt|srt|vtt
```

**Notable patterns & decisions.**
- **No ffmpeg/ffprobe** — the headline decision (issue #58). Duration extraction is pure-JS via `music-metadata.parseBuffer(..., { duration: true })`, so the app runs in a slim Docker image with zero native audio deps. The MIME hint short-circuits format sniffing; `duration === 0` is overloaded as the "not real audio" signal.
- **Waveform is decoded on the client, persisted write-once on the server.** The server never decodes audio; it just stores a tiny (~3–6 KB) normalized `number[]`. The schema comment explicitly notes "no audio reconstruction is possible from these values" — peaks are visualization-only.
- **Security hygiene in ingestion**: content type derived from validated extension (never `file.type`), filename encrypted at rest, single-read buffering to avoid double memory, storage-rollback on DB failure.
- **Robust client orchestration**: shared `AudioContext`, `AbortController` + `currentIdRef` stale guards, graceful degradation (decode failure is `console.warn`, never a toast), manual-trigger escape hatch for long files.
- **Export is deliberately minimal**: one route, one `switch`, plaintext decrypted up front, browser blob-download via synthetic `<a download>`. Auto-export and scheduled backup are visibly stubbed ("Coming soon", disabled switches).

**Relevance to Plaude Local (anti-Plaud, local-first).**

*Adapt (design only — see license caveat):*
- **The peaks model is the single most reusable idea.** Generate a small normalized `f32`/float amplitude-envelope array once and persist it next to the recording (in our SQLite `history.db`), keep it write-once, and render a canvas waveform from it. For us this is even cleaner: the Rust side already has decoded PCM during capture/diarization, so we can compute peaks *server-side at capture time* (e.g. in `session.rs`/`recorder.rs`) and never need the browser-decode + POST-back dance at all. Borrow the `DEFAULT_BUCKETS=500`, `[0,1]` normalization, and the 32–2048 clamp as sane defaults.
- **Pure-in-process duration/metadata extraction** maps directly onto our no-cloud ethos: instead of `music-metadata` (a JS lib), use a Rust crate (e.g. `symphonia`) to read duration/codec from the captured buffer — same principle, zero external binary, no `ffprobe`.
- **The export format strings (SRT `HH:MM:SS,mmm`, VTT `HH:MM:SS.mmm`, the `WEBVTT` header, the TXT separator block, the JSON record shape, `recordings-YYYY-MM-DD.<ext>` filenames) are a clean spec to reimplement** in Rust as a Tauri command that returns a string + suggested filename. **Crucially, improve on it for our use case:** because Plaude Local has a real speaker-labelled, time-stamped timeline, our SRT/VTT should emit **one cue per diarized segment** with true per-segment start/end times and an optional `Speaker N:` prefix — riffado collapses the whole recording into a single cue, which would throw away our diarization. Add a Markdown/Plaud-style "speaker-labelled transcript" export too.
- **Settings precedence (query param → stored default → hardcoded fallback)** and the blob-download UX translate to a Tauri "save file" dialog with a default-format preference persisted locally.

*Does NOT apply (cloud/web/multi-tenant machinery):* `requireApiSession` + `userId`-scoping on every query (we're single-user), `createUserStorageProvider` / `storageType: 'local'|'s3'` abstraction (we always write to `~/Library/Application Support/com.pais.handy/recordings/`), `encryptText`/`decryptText` field encryption (Postgres-at-rest concern; macOS local disk + OS keychain is a different threat model), the `plaudFileId`/`plaudVersion`/`deviceSn` Plaud-cloud-sync columns (we are the *anti-Plaud* — there is no Plaud cloud to pull from), the HTTP `Range` audio-serving route (a desktop app reads the file directly, no streaming endpoint), and the POST-peaks-back-to-server round trip (eliminated entirely if we compute peaks in Rust).

*AGPL caveat:* riffado is **AGPL-3.0**. We may study and reimplement these designs (format specs, the peaks concept, the no-ffprobe approach) from scratch, but must **not copy any of these source files or their code verbatim** into Plaude Local. The SRT/VTT/WebVTT formats themselves are open standards and free to implement; only riffado's specific source text is encumbered.

## Storage Abstraction (Local FS + S3-compatible)

**What it is.** A tiny provider-pattern abstraction (`StorageProvider` interface + two implementations) that lets riffado store/retrieve/serve audio blobs identically whether they live on the local filesystem or in any S3-compatible object store (AWS S3, Cloudflare R2, MinIO, Backblaze B2, DigitalOcean Spaces, Wasabi). A factory picks the implementation from env at runtime; callers (upload, sync, audio-serve, delete routes) only ever touch the interface.

**Key files.**
- `src/lib/storage/types.ts` — the `StorageProvider` interface, `S3Config` shape, and `StorageType` union. The contract.
- `src/lib/storage/local-storage.ts` — `LocalStorage` class: writes files under a base dir, with path-traversal hardening.
- `src/lib/storage/s3-storage.ts` — `S3Storage` class: wraps `@aws-sdk/client-s3` + `@aws-sdk/s3-request-presigner`.
- `src/lib/storage/factory.ts` — `createStorageProvider()` / `createUserStorageProvider(userId)`; reads env, validates S3 config, returns the right provider; re-exports everything.
- `src/lib/env.ts` (lines 57–63, 196–202) — zod-validated env schema for the storage knobs.
- `src/app/api/recordings/upload/route.ts` — primary writer: computes the storage key, uploads, inserts DB row, compensating-deletes on DB failure.
- `src/app/api/v1/recordings/[id]/audio/route.ts` — primary reader/server: S3 → 302 redirect to presigned URL; local → stream buffer with HTTP Range support.
- `src/app/api/recordings/[id]/audio/route.ts` — legacy reader (local-only, always downloads buffer).
- `src/app/api/recordings/[id]/route.ts` (lines 89–100, 156–162) — delete path; `isStorageNotFoundError()` normalizes provider-specific not-found errors.
- `src/lib/sync/sync-recordings.ts` — Plaud-cloud sync writer; same upload/compensation pattern via `uniqueStorageKey(...)`.

**How it works.**
The whole abstraction is the 5-method `StorageProvider` interface. Both implementations are stateless wrappers around a backend; neither knows about the DB or HTTP — they move `Buffer`s keyed by string.

*Factory & selection.* `createStorageProvider()` reads `env.DEFAULT_STORAGE_TYPE`. For `"local"` it returns `new LocalStorage()` (no config). For `"s3"` it assembles an `S3Config` from `S3_ENDPOINT/BUCKET/REGION/ACCESS_KEY_ID/SECRET_ACCESS_KEY`, then hard-fails (`throw new Error`) if any of bucket/region/accessKeyId/secretAccessKey is missing, and returns `new S3Storage(s3Config)`. Anything else throws `Unsupported storage type`. `createUserStorageProvider(_userId)` is an `async` wrapper that currently **ignores its `userId` arg** and just delegates to `createStorageProvider()` — a deliberate seam for future per-user/per-tenant storage routing that isn't wired yet (note the leading-underscore unused param). Every consumer calls the per-user variant, so adding tenant routing later is a one-function change.

*Keying.* Keys are constructed by callers, not the provider. On upload (`upload/route.ts`): `fileId = "uploaded-" + nanoid()`, then `storageKey = `${session.user.id}/${fileId}${ext}``. So the layout is `<userId>/<fileId>.<ext>` — user-id-prefixed namespacing that doubles as an isolation boundary and (for local) a subdirectory. The DB row stores both `storageType` (which backend) and `storagePath` (the key) so reads later know how to fetch. Sync uses `uniqueStorageKey(...)` to avoid collisions with existing `recordings.storagePath` values.

*LocalStorage.* Constructor resolves `baseDir = resolve(baseDir || env.LOCAL_STORAGE_PATH)` (default `./storage`). The crux is `getFilePath(key)`, a two-layer path-traversal guard: (1) normalize `\`→`/`, reject keys containing `..`, starting with `/`, or containing a NUL byte; (2) `join` onto baseDir, `resolve`, then verify `relative(baseDir, resolvedPath)` doesn't start with `..` (i.e. the resolved path stays inside baseDir). `uploadFile` lazily `mkdir -p`s both the base dir and the key's parent dir, then `writeFile`; it **ignores `contentType`** (`void contentType`) since the FS has no metadata slot. `downloadFile` is `readFile`. `getSignedUrl` ignores `expiresIn` and returns a relative app URL `/api/recordings/audio/${encodeURIComponent(key)}` — i.e. "signing" locally just means "route it through our own authenticated audio endpoint." `deleteFile` is `unlink`. `testConnection` round-trips a `test-<ts>.txt` write+delete.

*S3Storage.* Constructor builds an `S3Client` with region + static credentials, and — only if `config.endpoint` is set — adds `endpoint` plus `forcePathStyle: true` (required for MinIO/R2/etc. that don't do virtual-hosted-style buckets). `uploadFile` → `PutObjectCommand` (passes real `ContentType`). `downloadFile` → `GetObjectCommand`, then manually drains the streaming `response.Body` as `AsyncIterable<Uint8Array>` into chunks and `Buffer.concat`s them. `getSignedUrl` → `getSignedUrl(client, GetObjectCommand, { expiresIn })` (presigned GET, seconds). `deleteFile` → `DeleteObjectCommand`. `testConnection` → `HeadBucketCommand`. Every method wraps errors in a `Failed to ... : <msg>` `Error`.

*Read path (the polymorphism payoff).* `v1/.../audio/route.ts` loads the recording, then branches on `recording.storageType`: if `"s3"`, it calls `getSignedUrl(storagePath, 300)` and returns `NextResponse.redirect(signedUrl, 302)` — the browser fetches the blob directly from object storage, never proxying bytes through the app. Otherwise it `downloadFile`s into a buffer and serves it with full RFC 7233 **Range** support (206 Partial Content, `Content-Range`, clamps oversized end, 416 only on unsatisfiable start, `Cache-Control: private, max-age=300`). The local provider's `getSignedUrl` returning a relative `/api/recordings/audio/...` URL closes the loop: even the "presigned" local case funnels back through this authenticated, range-capable endpoint.

*Write integrity.* Both writers (`upload`, `sync`) use a **compensating transaction**: upload to storage first, then DB insert; if the insert throws, `deleteFile(storageKey)` to avoid orphaned blobs (and log if cleanup also fails). The delete route does the inverse ordering (storage delete first, then DB tombstone) and uses `isStorageNotFoundError()` to treat already-gone objects as success so a half-failed delete can be retried idempotently — it prefers typed signals (`error.code === "ENOENT"`, AWS `error.name` `"NoSuchKey"`/`"NotFound"`, `error.$metadata.httpStatusCode === 404`) over fragile message matching.

**Contracts & shapes.**

```ts
// src/lib/storage/types.ts
export interface StorageProvider {
    uploadFile(key: string, buffer: Buffer, contentType: string): Promise<string>;
    downloadFile(key: string): Promise<Buffer>;
    getSignedUrl(key: string, expiresIn: number): Promise<string>;
    deleteFile(key: string): Promise<void>;
    testConnection(): Promise<boolean>;
}

export interface S3Config {
    endpoint?: string;          // optional; only for non-AWS S3-compatible
    bucket: string;
    region: string;
    accessKeyId: string;
    secretAccessKey: string;
}

export type StorageType = "local" | "s3";
```

```ts
// src/lib/env.ts (lines 57–63) — zod schema
DEFAULT_STORAGE_TYPE: z.enum(["local", "s3"]).optional().default("local"),
LOCAL_STORAGE_PATH:  z.string().optional().default("./storage"),
S3_ENDPOINT:         z.string().optional(),
S3_BUCKET:           z.string().optional(),
S3_REGION:           z.string().optional(),
S3_ACCESS_KEY_ID:    z.string().optional(),
S3_SECRET_ACCESS_KEY:z.string().optional(),
```

```
Storage key layout:  <userId>/uploaded-<nanoid>.<ext>
Local signed URL:    /api/recordings/audio/<urlencoded-key>
S3 presigned GET:    expiresIn = 300 (seconds) in the read route
forcePathStyle:      true  (only when S3_ENDPOINT is set)
DB columns carried:  recordings.storageType, recordings.storagePath
```

```ts
// @aws-sdk/client-s3 commands used
PutObjectCommand, GetObjectCommand, DeleteObjectCommand, HeadBucketCommand, S3Client
// @aws-sdk/s3-request-presigner
getSignedUrl
```

**Notable patterns & decisions.**
- **Bytes-in/bytes-out interface, keys constructed by callers.** The provider is dumb about meaning; the route layer owns key naming and the `storageType`+`storagePath` columns. Clean separation.
- **`getSignedUrl` unified across backends by polymorphic meaning, not signature.** S3 returns a real presigned URL (offload bytes to the CDN/object store, 302 redirect); local returns a relative app route (re-enter the authenticated server). Same method, two strategies, read route doesn't care.
- **Defense-in-depth path-traversal guard** in `LocalStorage.getFilePath` (string checks *and* resolved-path containment check) — the one piece of real security logic, since attacker-influenced keys hit the FS.
- **`forcePathStyle` gated on `endpoint`** — the single line that makes "S3" actually mean "any S3-compatible store."
- **Compensating delete on DB-insert failure** both on upload and sync — orphan avoidance without a real distributed transaction.
- **Typed-error normalization** (`isStorageNotFoundError`) to make delete idempotent across two very different backends.
- **`createUserStorageProvider` is a no-op seam** — `userId` ignored today, ready for per-tenant routing.
- Leans on `@aws-sdk/client-s3` v3 modular commands and the separate `s3-request-presigner` package; manual stream-draining of `GetObject` `Body` (no helper).

**Relevance to Plaude Local (anti-Plaud, local-first).**
- **Adopt the interface shape, drop S3 entirely.** The 5-method `StorageProvider` contract (`upload/download/getSignedUrl/delete/testConnection`) is a clean seam, but for a single-user offline Tauri/Rust app the S3 implementation, presigned URLs, the 302-redirect read path, and the `<userId>/` key prefix are **cloud/multi-tenant machinery that does not apply**. We have no tenants and no remote bucket. Our backend is already SQLite + `~/Library/Application Support/com.pais.handy/recordings/` — that *is* `LocalStorage`.
- **What to ADAPT for the "knowledge-base folder" idea:** (1) the **path-traversal hardening** in `getFilePath` — if we ever let users name/import files into a KB folder, port that exact two-layer guard (reject `..`/leading-`/`/NUL, then verify the canonicalized path stays under the root) into Rust (`std::path::Path::canonicalize` + `starts_with(base)`). (2) The **DB-stores-the-key, provider-resolves-it** split: keep a `storage_path` column relative to a single configurable root (analogous to `LOCAL_STORAGE_PATH`) rather than absolute paths, so the KB folder is relocatable/portable. (3) The **compensating-delete / orphan-avoidance ordering** (write blob → insert row → on failure delete blob; delete row + delete file as a pair) maps directly onto our `session.rs` recording lifecycle. (4) **HTTP Range / partial-content** serving is relevant only insofar as our React frontend streams audio from the Rust side — Tauri can serve via a custom protocol or `convertFileSrc`, so we likely don't reimplement byte-range parsing, but the seek-support requirement is the same UX.
- **What does NOT apply:** S3/R2/MinIO config, `@aws-sdk/*`, presigned-URL expiry, env-driven backend selection, `createUserStorageProvider(userId)` tenancy seam, `getAudioMimeType`-via-HTTP serving. Our equivalent of "signed URL" is just a local file path / Tauri asset URL.
- **AGPL-3.0 caveat:** this code is AGPL-3.0 — we may study the design (interface decomposition, the traversal guard logic, the compensating-delete pattern) and re-implement it cleanly in Rust, but must **not copy these TypeScript source files** (or close paraphrases) into Plaude Local. Re-derive from the documented behavior, not by porting line-for-line.

## Encryption at Rest (AES-256-GCM field encryption)

**What it is.** Riffado encrypts a fixed set of sensitive database columns with AES-256-GCM, keyed off a single server-held `ENCRYPTION_KEY` env var. It is explicitly **server-held-key envelope encryption, not zero-knowledge** — the app server decrypts at request time so it can run AI on the content. The scheme protects against stolen DB backups, read replicas, SQL injection that reads but doesn't execute, and DB-only operators; it does **not** protect against a compromised app server, a leaked key, or the configured AI provider.

**Key files.**
- `src/lib/encryption.ts` — the AES-256-GCM primitive. Exports `encrypt`, `decrypt`, `encryptJSON`, `decryptJSON`, `generateEncryptionKey`; reads/validates the key via `getEncryptionKey()`.
- `src/lib/encryption/fields.ts` — content-field wrappers layered on the primitive: `encryptText`/`decryptText`, `encryptJsonField`/`decryptJsonField`, predicates `isEncryptedText`/`isEncryptedJsonField`. Adds the `v1:` version prefix and legacy-plaintext tolerance.
- `scripts/encrypt-backfill.ts` — one-shot, idempotent, id-cursor-paginated backfill that eagerly encrypts pre-rollout rows (`BATCH_SIZE = 500`).
- `src/db/schema.ts` — the columns themselves (declared as plain `text()`/`jsonb()`; no custom Drizzle type).
- `src/tests/encryption-fields.test.ts` — round-trip, legacy passthrough, tamper-rejection, predicate tests.
- `docs/encryption-at-rest.md`, `content/docs/reference/encryption-at-rest.mdx`, `content/docs/reference/security-model.mdx`, `SECURITY.md` — the prose spec, the wider trust-boundary model, and deployment guidance.

**How it works.**

*Primitive (`src/lib/encryption.ts`).* `getEncryptionKey()` reads `env.ENCRYPTION_KEY`, requires `length === 64`, and `Buffer.from(keyHex, "hex")` → a 32-byte key (throws otherwise). `encrypt(plaintext)` generates a fresh 16-byte IV via `randomBytes(IV_LENGTH)`, runs `createCipheriv("aes-256-gcm", key, iv)`, updates with `utf8`→`hex`, finalizes, pulls the GCM auth tag with `cipher.getAuthTag()`, and returns the colon-joined string `iv:authTag:ciphertext` (all hex). `decrypt(ciphertext)` splits on `:`, **rejects anything not exactly 3 parts**, rebuilds the three buffers from hex, calls `decipher.setAuthTag(authTag)`, and decrypts — so a tampered ciphertext or wrong key throws inside GCM verification (wrapped as `Decryption failed: …`). `encryptJSON`/`decryptJSON` are thin `JSON.stringify`/`JSON.parse` wrappers.

*Field layer (`src/lib/encryption/fields.ts`).* This is where versioning and forward/backward compatibility live. It defines a `VERSION_PREFIX = "v1:"` and two regexes that recognize ciphertext shape: `V1_CIPHERTEXT_SHAPE` (`v1:` + 32-hex IV + `:` + 32-hex tag + `:` + even-length hex body) and `RAW_CIPHERTEXT_SHAPE` (same without the prefix — the pre-existing token/key format).
- `encryptText(plaintext)` passes `null`/`undefined` through untouched, otherwise returns `` `v1:${encrypt(plaintext)}` ``. Overloads preserve nullability in the type signature.
- `decryptText(value)` passes null/undefined through; if it matches `V1_CIPHERTEXT_SHAPE` it strips the prefix and decrypts; if it matches the legacy `RAW_CIPHERTEXT_SHAPE` it decrypts as-is; **anything else is returned verbatim as legacy plaintext.** This is the deploy→backfill compatibility window.
- `encryptJsonField(value)` JSON-stringifies, encrypts, and wraps in an envelope object `{ c: "v1:..." }` so encrypted JSONB stays valid JSONB. `decryptJsonField<T>(value)` detects the envelope via `isEnvelope` (object with a string `c` key), strips `v1:` if present, decrypts, and `JSON.parse`s; non-envelope (legacy plaintext JSON) values pass through cast to `T`.
- `isEncryptedText`/`isEncryptedJsonField` are read-only shape predicates used by the backfill to skip already-encrypted rows.

*Drizzle integration.* There is **no custom Drizzle column type or transparent codec.** Columns are ordinary `text("...")` / `jsonb("...")` in `src/db/schema.ts`. Encryption is applied **manually at every read/write boundary** — i.e. ciphertext is what actually sits in Postgres, and route handlers call the wrappers explicitly. Examples: writes — `recordings.upload/route.ts` stores `filename: encryptText(basename)`; `summary/route.ts` stores `encryptText(summary)`, `encryptJsonField(keyPoints)`, `encryptJsonField(actionItems)`; `settings/user/route.ts` stores `encryptJsonField(body.summaryPrompt)`. Reads — `dashboard/page.tsx`, `recordings/[id]/page.tsx`, and the recordings/export/backup API routes call `decryptText(...)` / `decryptJsonField<T>(...)` when projecting rows out. The provider-key and bearer-token paths use the lower-level `encrypt` directly (legacy raw format). Because the discipline is manual, a forgotten decrypt at a new call site silently leaks `v1:...` ciphertext to the client — there is no compiler enforcement.

*Backfill (`scripts/encrypt-backfill.ts`).* Pages each table by `id > lastId ORDER BY id ASC LIMIT 500` (bounded memory), skips rows where `isEncryptedText`/`isEncryptedJsonField` is already true, encrypts the rest, and tracks `TableStats` (`inspected/alreadyEncrypted/encrypted/nullSkipped`). `--dry-run` reports without writing. Idempotent and resumable.

**Contracts & shapes.**

```ts
// src/lib/encryption.ts
const ALGORITHM = "aes-256-gcm";
const IV_LENGTH = 16;   // bytes
const KEY_LENGTH = 32;  // bytes (AES-256)
// ENCRYPTION_KEY must be exactly 64 hex characters (32 bytes)

/** AES-256-GCM encrypt. Returns `iv:authTag:ciphertext` (hex). */
export function encrypt(plaintext: string): string;
export function decrypt(ciphertext: string): string; // requires exactly 3 colon-parts
```

```ts
// src/lib/encryption/fields.ts
const VERSION_PREFIX = "v1:";
const RAW_CIPHERTEXT_SHAPE = /^[0-9a-f]{32}:[0-9a-f]{32}:(?:[0-9a-f]{2})*$/i;
const V1_CIPHERTEXT_SHAPE  = /^v1:[0-9a-f]{32}:[0-9a-f]{32}:(?:[0-9a-f]{2})*$/i;

export interface EncryptedJsonEnvelope { c: string; }   // jsonb stored as { "c": "v1:..." }
```

Ciphertext format ladder (verbatim from `docs/encryption-at-rest.md`):
```
v1:iv:tag:hex   — current; identifies key/version
iv:tag:hex      — legacy (Plaud bearer tokens, AI keys); read path tolerates it
<anything else> — legacy plaintext; returned verbatim
```

Encrypted columns (from `security-model.mdx`, superset):
```
recordings.filename                     (text)   AI-generated titles
transcriptions.text                     (text)   full transcript
ai_enhancements.summary                 (text)   LLM summary
ai_enhancements.key_points              (jsonb)  { "c": "v1:..." }
ai_enhancements.action_items            (jsonb)  { "c": "v1:..." }
user_settings.summary_prompt            (jsonb)  envelope
user_settings.title_generation_prompt   (jsonb)  envelope
plaud_connections.bearer_token          (text)   legacy raw format
api_credentials.api_key                 (text)   legacy raw format
webhook_endpoints.url                   (text)
webhook_endpoints.secret                (text)   whsec_… signing secret
```

Key generation and env:
```bash
node -e "console.log(require('node:crypto').randomBytes(32).toString('hex'))"
# ENCRYPTION_KEY (64 hex). Distinct keys: BETTER_AUTH_SECRET (sessions),
# API_TOKEN_HASH_SECRET (HMAC-SHA256 of personal API keys). ENCRYPTION_KEY is
# NEVER reused as an HMAC key.
```

**Notable patterns & decisions.**
- **Self-describing version prefix as a no-op migration mechanism.** `v1:` carries nothing functional today; its whole purpose is to let a future key-rotation pass find rows to re-encrypt. Key rotation is *not* implemented.
- **Decrypt is permissive, encrypt is strict.** `decryptText` returning unknown shapes verbatim is deliberate — it makes "deploy code, then backfill later" safe and lets old plaintext rows coexist. The cost: it can mask a bug where ciphertext was never written.
- **GCM gives integrity for free.** Tamper-rejection is just the auth-tag check; the test suite asserts a mutated ciphertext throws.
- **JSONB envelope `{ c }`** keeps the column a valid JSON document while opaque — avoids changing the column type to `text`.
- **Per-secret-use key separation** is called out explicitly as a deliberate "no key reuse" rule, with API tokens HMAC-hashed (not encrypted, not plain SHA-256).
- **Backup files are plaintext by design** (`/api/backup`) — treated as the user's own export.
- Stdlib-only crypto (`node:crypto`), no third-party crypto dependency.

**Relevance to Plaude Local (anti-Plaud, local-first).**
- **ADAPT — the field-encryption abstraction layer.** The `encryptText`/`decryptText` + `{ c }` JSON envelope + version-prefix + legacy-passthrough design is the genuinely reusable idea and maps cleanly onto our SQLite history (`~/Library/Application Support/com.pais.handy/history.db`). In Rust we'd implement the same with the `aes-gcm` crate (or `ring`): random 12-byte nonce (note: GCM's standard nonce is 96-bit; riffado uses 16 bytes, which is non-standard but legal) + tag + ciphertext, a `v1:` prefix for future rotation, and tolerant decrypt for migration. Encrypt the speaker-labelled transcript text, summaries, and any AI key fields.
- **ADAPT — the idempotent, id-cursor backfill pattern** for retro-encrypting an existing `history.db` after we ship encryption, plus a `--dry-run`.
- **DECISION POINT — key management is the hard part for us, and riffado's answer doesn't transfer.** Their `ENCRYPTION_KEY` env var assumes an operator-managed server process. A local single-user desktop app has no such operator and storing a key in plaintext config defeats the purpose. The right local analog is the **macOS Keychain** (Tauri has `keyring`/`tauri-plugin-stronghold` options) to hold the AES key, or deriving it from a user passphrase via Argon2/PBKDF2. This is the one subsystem where we should *not* mirror riffado.
- **Manual call-site discipline does NOT scale safely** — riffado leaks ciphertext if a dev forgets a `decryptText`. For a local app, prefer encrypting/decrypting inside the single Rust data-access layer (the session/history manager) so the React frontend never sees ciphertext and there's exactly one boundary, not dozens.
- **DOES NOT APPLY (cloud/web/multi-tenant):** the entire "server holds the key and decrypts at request time so it can run AI" trust model — that's the exact thing we are the *opposite* of. Our threat model is the reverse: data never leaves the device, ASR/diarization/LLM run locally, so "compromised AI provider" and "read-replica/DB-operator" boundaries are moot. Also N/A: `webhook_endpoints.*`, `plaud_connections.bearer_token` (we don't pull from Plaud cloud), `api_credentials.api_key` for hosted providers, S3 bucket SSE, Postgres-on-disk advice, per-user `userId` query isolation, account suspension, rate-limit proxy headers.
- **AGPL-3.0 caveat:** AES-256-GCM, the `iv:tag:ciphertext` layout, and the `v1:` envelope are standard cryptographic constructions and ideas — free to reimplement. But do **not** copy `encryption.ts`/`fields.ts` source (or paste-translate it) into our app; the copyleft applies to their *code*, not to the design pattern. Reimplement from the spec in Rust.

Key files (absolute): `/private/tmp/claude-501/-Users-vladvrinceanu-Desktop-PROGETTI-ANTYGRAVITY-Plaude-Local/5cf6ff9d-1432-4926-b37c-12af24c6d541/scratchpad/repos/riffado/src/lib/encryption.ts`, `.../src/lib/encryption/fields.ts`, `.../scripts/encrypt-backfill.ts`, `.../src/db/schema.ts`.

## Public/Automation API & Signed Webhooks

**What it is.** A versioned, read-only public HTTP surface (`/api/v1/*`) authenticated by personal API keys (Bearer tokens), paired with an outbound signed-webhook system (HMAC over `timestamp.body`) backed by a Postgres-queued, leased, exponential-backoff delivery worker. This is riffado's "automation surface" — the integration point for Zapier/n8n/scripts and for event-driven downstream automation. There is no agent/LLM tool surface; the automation primitives are a stable read API plus event webhooks.

**Key files.**
- `src/app/api/v1/recordings/route.ts` — `GET /api/v1/recordings`: cursor-paginated list with filters.
- `src/app/api/v1/recordings/[id]/route.ts` — `GET` single recording detail (transcript + summary inline).
- `src/app/api/v1/recordings/[id]/transcript/route.ts` — `GET` transcript only.
- `src/app/api/v1/recordings/[id]/audio/route.ts` — `GET` audio bytes / `302` to S3 signed URL, with RFC 7233 `Range` support.
- `src/lib/v1/serialize.ts` — `V1Recording`/`V1RecordingDetail`/`V1Transcript`/`V1Summary` shapes, snake_case serializers, opaque base64url cursor encode/decode, the joined `getV1RecordingDetailForUser` query.
- `src/lib/v1/rate-limit.ts` — two-bucket limiter (`enforceV1IpRateLimit`, `enforceV1AuthenticatedRateLimit`) + 429 envelope.
- `src/lib/rate-limit.ts` — DB-backed bucket primitive `consumeRateLimitBucket`, HMAC'd bucket keys, fail-open, `getClientIp` proxy-header trust.
- `src/lib/auth-request.ts` — API-key generation/format/mask/hash + `authenticateRequest` (Bearer key OR session), suspension check.
- `src/lib/errors.ts` — `ErrorCode` enum, `AppError`, `apiHandler` wrapper, `mapErrorToAppError`, `err_*` error-id tagging.
- `src/app/api/settings/api-keys/route.ts` (+ `[id]/route.ts`) — session-auth'd key management (create returns raw key once).
- `src/app/api/settings/webhooks/route.ts` (+ `[id]/`, `[id]/deliveries/`, `.../redeliver/route.ts`) — endpoint CRUD + delivery log + manual redeliver.
- `src/lib/webhooks/signature.ts` — HMAC-SHA256 sign/verify with timestamp tolerance and `timingSafeEqual`.
- `src/lib/webhooks/emit.ts` — `WEBHOOK_EVENTS`, `emitEvent` (fan-out to matching endpoints, enqueue deliveries).
- `src/lib/webhooks/worker.ts` — tick loop, DNS-pinned outbound POST, backoff, attempt bookkeeping.
- `src/lib/webhooks/url.ts` — SSRF guard: scheme/credential checks, private-range IPv4/IPv6 detection, DNS resolve + pin.
- `src/lib/webhooks/secrets.ts` / `payload.ts` / `recording.ts` — encrypted secret/URL storage, payload shapes, recording hydration (incl. soft-delete tombstones).
- `src/db/queries/webhook-deliveries.ts` — two-phase fair-share atomic claim, lease reclaim, release.
- `src/db/schema.ts` (lines 449–558) — `api_keys`, `webhook_endpoints`, `webhook_deliveries`, `api_rate_limit_buckets` tables.
- `src/instrumentation.ts` — boots `startWebhookWorker()` on the Node runtime.
- `content/docs/reference/public-api.mdx` — the documented public contract.

**How it works.**

*Request lifecycle (v1).* Every v1 route is wrapped in `apiHandler` (from `errors.ts`), which try/catches the handler and converts any throw into the unified JSON error envelope via `mapErrorToAppError`; 5xx errors get an `err_xxxxxxxx` id attached and logged. Inside each handler the order is fixed: (1) `enforceV1IpRateLimit(request)` → if non-null return its 429; (2) `authenticateRequest(request)` → throw `UNAUTHORIZED` if null; (3) `enforceV1AuthenticatedRateLimit(authn)` → return its 429; (4) do the work. This exact sequence is duplicated in all four v1 routes (no shared middleware — Next.js App Router route handlers, each is a standalone `export const GET`).

*Authentication.* `authenticateRequest` reads `Authorization: Bearer <token>`. If the token starts with `op_`, it HMAC-SHA256-hashes it (`hashApiKey`, keyed off `API_TOKEN_HASH_SECRET` ?? `BETTER_AUTH_SECRET`) and looks up `api_keys` by `keyHash` with `revokedAt IS NULL AND (expiresAt IS NULL OR expiresAt > now)`. On hit it asserts the user isn't suspended (`assertUserNotSuspended` → `ACCOUNT_SUSPENDED` 403), fire-and-forgets a `lastUsedAt` update (`void db.update(...).catch(...)` — never blocks the request), and returns `{ user, via: "api-key", apiKeyId }`. Otherwise it falls through to a Better Auth session cookie (`auth.api.getSession`), returning `via: "session"`. So v1 routes accept *both* API keys and logged-in sessions; `/api/settings/*` routes instead use `requireApiSession` (session-only).

*API key format.* `createApiKey()` produces `op_` + base62 payload (default 30 chars) + a 4-char base62 CRC32 checksum (`op_{payload}{crc}`). Only the HMAC hash and a 12-char `keyPrefix` are persisted; the raw key is returned exactly once from `POST /api/settings/api-keys`. `validateApiKeyFormat` does a checksum self-check but is deliberately *not* on the auth path (legacy nanoid keys still authenticate by hash lookup). Scopes are normalized to `["read"]` — read is the only scope.

*Listing & pagination.* `GET /api/v1/recordings` left-joins `recordings` with `plaud_devices`, `transcriptions`, `ai_enhancements`, all scoped to `userId` and `deletedAt IS NULL`. Filters: `limit` (1–100, default 50), `created_since`/`updated_since` (ISO), `has_transcription` (true/false → `isNotNull/isNull(transcriptions.id)`). Pagination is keyset/cursor: orders by `updated_at DESC, id DESC`, fetches `limit+1`, and if overfull encodes a cursor with `encodeRecordingCursor({updatedAt,id})` (base64url JSON). The cursor predicate is the canonical compound keyset comparison `updatedAt < cursor.updatedAt OR (updatedAt = cursor.updatedAt AND id < cursor.id)`.

*Audio streaming.* For `storageType === "s3"`, returns `302` to a 5-minute pre-signed URL (client fetches direct from S3). For local storage it downloads the buffer, derives MIME from extension, and honors `Range`: parses `bytes=start-end`, clamps oversized `end` to `fileSize-1` (RFC 7233), returns `206` + `Content-Range`, and only emits `416` (`Content-Range: bytes */<size>`) for unsatisfiable starts.

*Rate limiting.* `consumeRateLimitBucket(rawKey, {limit, windowMs})` HMACs the raw key (so the multi-tenant DB never stores plaintext user-ids/IPs as bucket keys) and calls `upsertRateLimitBucket` against the `api_rate_limit_buckets` table. It is **fail-open**: if the bucket store throws, it logs and returns `allowed:true` rather than taking the API down. v1 enforces two buckets per request: IP (`v1:ip:<ip>`, 1200/min) and authenticated identity (`v1:auth:api-key:<id>` or `v1:auth:user:<id>`, 600/min). `getClientIp` only trusts `cf-connecting-ip`/`x-real-ip`/`x-forwarded-for` when `RATE_LIMIT_TRUST_PROXY_HEADERS` is set, else returns `"unknown"`.

*Webhook config.* `POST /api/settings/webhooks` (session-auth) validates the URL through `parseWebhookUrl` + `assertWebhookUrlAllowed`, validates events through `parseWebhookEvents` (rejects unknown/typo'd events rather than silently dropping), mints a secret `whsec_${nanoid(32)}`, encrypts both URL and secret at rest, and returns the plaintext secret once. `serializeWebhookEndpoint` decrypts the URL for display but masks the secret as `whsec_****<last4>`.

*Event emission & queueing.* `emitEvent(event, userId, recordingId, {error?})` selects enabled endpoints subscribed to the event (`events @> '["<event>"]'::jsonb` JSONB containment), inserts one `webhook_deliveries` row per endpoint with `status:"pending"`, `nextAttemptAt: now`, and a *stored* (compact) payload from `createStoredWebhookPayload`, then calls `signalWebhookWorker()`. The whole thing is wrapped so a webhook failure never breaks the business action that triggered it.

*Delivery worker.* Booted in `instrumentation.ts` (Node runtime only). `startWebhookWorker` runs `deliverDueWebhooks` on a 30s `setInterval` (`.unref()`'d) plus immediately; `signalWebhookWorker` kicks it on demand after emit/redeliver. A module-level `running` guard makes it single-flight. Each tick: `claimDueWebhookDeliveries()` does a **two-phase atomic claim** — phase 1 selects up to 50 due IDs with a per-user fairness cap of 10 via `row_number() OVER (PARTITION BY userId ...)`; phase 2 conditionally `UPDATE ... SET status='processing', nextAttemptAt = now+15min` (a lease) guarded by the still-due predicate + endpoint-still-enabled `EXISTS`, returning only rows actually won (concurrent-worker-safe). For each claimed row, `reloadClaimedDeliveryForSend` re-asserts ownership/enabled/still-processing (else `releaseClaimedDelivery` back to pending). `postDelivery` resolves the URL, hydrates the recording (`getWebhookRecordingDetailForUser`), builds the outbound payload, signs it, and POSTs. `markDeliveryAttempt` writes success (status `success`) or, on failure, computes backoff `[30s,2m,10m,1h,6h]`, sets status `pending` (or `dead` on permanent failure / exhausted attempts > 5), and updates the endpoint's `lastDeliveryStatus`. All writes are guarded by `status='processing' AND userId=...` so a stale worker can't clobber a reclaimed row.

*Signing.* `formatWebhookSignatureHeader` emits `t=<unix>,v1=<hex hmac-sha256(secret, "<t>.<body>")>`. `verifyWebhookSignature` parses the header, enforces a ±300s timestamp tolerance, recomputes, and compares with `crypto.timingSafeEqual` (length-checked first). Outbound headers: `X-Riffado-Signature`, `X-Riffado-Timestamp`, `X-Riffado-Event`, `X-Riffado-Delivery`, `User-Agent: Riffado-Webhooks/1`.

*SSRF defense (`url.ts`).* When `webhookTargetsRequirePublic()` (true when hosted), targets must be HTTPS, must not carry credentials, must not be `localhost`/`.local`/`.internal`/`.home.arpa`/`.lan`, and must not resolve to private/reserved ranges. It hand-rolls private-range detection for IPv4 (0/8, 10/8, 127/8, CGNAT 100.64/10, link-local 169.254, 172.16/12, 192.168, TEST-NETs, multicast ≥224) and IPv6 (loopback, ULA `fc00::/7`, link-local `fe80::/10`, multicast `ff00::/8`, `2001:db8::/32`, plus `::ffff:` IPv4-mapped). Crucially it does DNS resolution itself (`dns.lookup`, `all:true`), rejects if any resolved address is private, then **pins** those resolved IPs into the actual HTTP request via a custom `lookup` (`createPinnedLookup`) — closing the TOCTOU/DNS-rebinding hole between validation and connection. Outbound response bodies are truncated to 4096 bytes; request timeout is 10s.

**Contracts & shapes.**

```ts
// src/lib/webhooks/emit.ts — the entire event vocabulary
export const WEBHOOK_EVENTS = [
    "recording.synced",
    "recording.updated",
    "recording.deleted",
    "transcription.completed",
    "transcription.failed",
] as const;
```

```ts
// src/lib/v1/serialize.ts — public read shapes (snake_case on the wire)
export type V1Recording = {
    id: string; title: string;
    created_at: string; updated_at: string; recorded_at: string;
    duration_ms: number; filesize_bytes: number;
    device: { serial_number: string; name: string | null; model: string | null } | null;
    has_transcription: boolean; has_summary: boolean;
    links: { self: string; transcript: string; audio: string };
};
export type V1RecordingDetail = V1Recording & {
    transcript: V1Transcript | null; summary: V1Summary | null;
};
export type V1Transcript = {
    language: string | null; text: string;
    provider: string; model: string; created_at: string;
};
```

```ts
// src/lib/errors.ts — unified error envelope
export interface AppErrorJSON {
    error: string;            // human-readable; never parse it
    code: ErrorCode;          // machine-readable; branch on this
    details?: Record<string, unknown>;
}
```

```
# Outbound webhook signature header (signature.ts)
X-Riffado-Signature: t=<unix-seconds>,v1=<hmac-sha256-hex of "<t>.<rawbody>">
# tolerance = 300s, timingSafeEqual compare
```

```
# Routes
GET  /api/v1/recordings                       (?limit=1..100&cursor&created_since&updated_since&has_transcription)
GET  /api/v1/recordings/{id}
GET  /api/v1/recordings/{id}/transcript
GET  /api/v1/recordings/{id}/audio            (Range / 302→S3 signed URL)
GET|POST /api/settings/api-keys               (session-auth; POST returns raw key once)
GET|POST /api/settings/webhooks
...      /api/settings/webhooks/{id}/deliveries[/{deliveryId}/redeliver]
```

```
# Tables (src/db/schema.ts)
api_keys(id, user_id, name, key_hash UNIQUE, key_prefix(16), source, scopes jsonb=['read'],
         last_used_at, expires_at, revoked_at, created_at, updated_at)
webhook_endpoints(id, user_id, url[encrypted], secret[encrypted], events jsonb,
                  description, enabled, last_delivery_at, last_delivery_status(16), ...)
webhook_deliveries(id, endpoint_id, user_id, recording_id, event(64), payload jsonb,
                   status(16), attempts, last_attempt_at, next_attempt_at,
                   last_response_status, last_response_body, last_error, ...)
api_rate_limit_buckets(key PK[HMAC'd], count, reset_at, ...)
```

```
# Env / constants
API_TOKEN_HASH_SECRET (?? BETTER_AUTH_SECRET)   # HMAC key for keys + rate-limit buckets
APP_URL                                          # base for absolute webhook links
WEBHOOKS_REQUIRE_PUBLIC_TARGETS (?? IS_HOSTED)   # toggles SSRF guard / HTTPS-only
RATE_LIMIT_TRUST_PROXY_HEADERS                   # whether to trust XFF / cf-connecting-ip
API_KEY_PREFIX = "op_"   secret prefix = "whsec_"
BACKOFF_MS = [30_000, 120_000, 600_000, 3_600_000, 21_600_000]
TICK_MS=30_000  TIMEOUT_MS=10_000  signature tolerance=300s
IP_LIMIT=1_200  AUTHENTICATED_LIMIT=600  WINDOW_MS=60_000
DELIVERY_LIMIT=50  PER_USER_DELIVERY_LIMIT=10  PROCESSING_LEASE_MS=15*60_000
```

**Notable patterns & decisions.**
- **No agent/tool surface.** Despite "automation API", there is no LLM/agent/MCP/tool-calling here. Automation = stable read API + outbound event webhooks. The API is explicitly **read-only**, write-in happens only through the UI/Plaud sync loop.
- **DNS-pinning SSRF guard** is the standout: resolve → validate every address → pin the validated IPs into the actual socket, defeating DNS rebinding. Hand-rolled IPv4/IPv6 private-range math (bigint for v6) rather than a dependency.
- **DB-as-queue, no broker.** Webhook delivery is a Postgres table polled by an in-process worker booted from `instrumentation.ts`. Atomic two-phase claim with a `processing` lease + per-user fairness window via `row_number()` gives at-least-once delivery and crash recovery (expired leases get reclaimed) without Redis/SQS. Stored payloads are compact (`recording_id` only); the full body is re-hydrated at send time so a recording edited/deleted after enqueue reflects current state (and `recording.deleted` reads soft-delete tombstones).
- **Fail-open rate limiting** + HMAC'd bucket keys: the limiter never takes down the API, and the bucket store can't be used to enumerate tenants.
- **Defense-in-depth tenancy:** every webhook query re-asserts `userId` even on internal worker paths; v1 queries always `AND userId = ... AND deletedAt IS NULL`.
- **Secrets at rest:** API keys stored only as HMAC + prefix; webhook URL *and* secret encrypted (URLs can carry path/query secrets); display always masks.
- **Stable contract discipline:** snake_case JSON, additive-only versioning, stable `ErrorCode` strings, `err_*` ids for 5xx correlation, per-handler duplicated guard sequence (explicit over a shared middleware).

**Relevance to Plaude Local (anti-Plaud, local-first).**

*ADAPT (design, reimplement in Rust — do not copy AGPL source):*
- The **read-API data model is directly reusable** as a local IPC/HTTP contract. `V1Recording`/`V1RecordingDetail`/`V1Transcript`/`V1Summary` map cleanly onto Handy's `history.db` + our `session.rs` long-form sessions (a session ↔ recording, diarized segments ↔ transcript, speaker labels are an extra field). The `links.self/transcript/audio` + `has_transcription/has_summary` HATEOAS-lite shape is a clean way to expose sessions to the React frontend or a localhost automation port.
- **Keyset (cursor) pagination** (`updated_at DESC, id DESC` + opaque base64url cursor) is the right pattern for a growing local SQLite session list — better than offset paging in a Tauri command returning history.
- **Outbound signed webhooks are genuinely useful even locally**: a single-user desktop "anti-Plaud" can fire `transcription.completed` / `session.ended` to a user's own n8n/Obsidian/shortcut. Adapt the **`t=<ts>,v1=<hmac>` signing + ±300s tolerance + constant-time compare** scheme verbatim as a spec; `node:crypto` HMAC ↔ Rust `hmac`/`sha2` is a trivial port.
- **The DB-as-queue delivery worker with backoff + lease** translates well to a single Rust background task over SQLite (`status`/`next_attempt_at`/`attempts`), giving durable retry across app restarts without any broker.
- The **unified error envelope** (`{error, code, details}` + stable code enum) is a good convention for Tauri command results.

*Does NOT apply (cloud/web/multi-tenant machinery):*
- The entire **multi-tenant apparatus** — `userId` scoping on every query, per-user fairness caps, suspension checks, HMAC'd bucket keys to prevent tenant enumeration — is moot for a single-user local app. Drop it.
- **API-key auth + IP/authenticated rate limiting** is web-perimeter defense. A local app has no untrusted callers; at most a localhost token if you expose an HTTP port. The `op_`+CRC32 key scheme and the two-bucket limiter are over-engineering for local.
- The **SSRF guard** is only needed because riffado lets arbitrary tenants register arbitrary webhook URLs on a shared egress. For a local single-user tool the user *owns* the target, so HTTPS-only + a light private-IP check is plenty; the full DNS-pinning machinery is unnecessary (though cheap to keep if you ever add team mode).
- **S3 signed-URL redirect**, Better Auth sessions, `IS_HOSTED`, proxy-header IP trust, Postgres/JSONB containment queries — all cloud-stack specifics; our equivalent is local files + Tauri commands + SQLite.
- **Plaud-cloud coupling** (`plaud_devices`, `recording.synced`, the Plaud error codes in `mapErrorToAppError`) is exactly the cloud dependency we are building *against* — ignore it; our "source" is mic + macOS system-audio capture.

*License caveat:* riffado is **AGPL-3.0**. We may study these shapes/algorithms and re-express them in our own Rust/TS, but must not copy `src/lib/...` source files (or close paraphrases) into Plaude Local. The signature scheme, cursor format, and table layouts are facts/specs and safe to reimplement; the actual TypeScript bodies are copyleft.

## Frontend Architecture & UX (Next.js App Router + Radix/Tailwind/cva)

**What it is.** riffado's web UI is a Next.js 15 App Router application organized into route-groups by concern (`(app)` authed workspace, `(auth)`, `(admin)`, `(docs)`, `(legal)`). The product surface is a single "audio workstation": a master-detail dashboard (`Workstation`) composed of a server-rendered RSC page that hydrates one big client component, which owns selection state and four modals (command palette, shortcuts, settings, onboarding). The design system is shadcn-style: Radix primitives + Tailwind v4 (OKLCH tokens) + `class-variance-authority`, themed light/dark via `next-themes`.

**Key files.**
- `src/app/(app)/layout.tsx` — authed shell: flex column with `Footer` + a single global `<Toaster/>`; a hosted-only `RebrandBanner` gated on `env.IS_HOSTED`.
- `src/app/(app)/dashboard/page.tsx` — RSC; runs four parallel Drizzle queries (`Promise.all`), decrypts content fields server-side, builds a `Map<recordingId, {text,language}>`, computes `initialSettingsFromRow`, renders `<Workstation/>`.
- `src/app/(app)/recordings/[id]/page.tsx` — RSC deep-link route for a single recording; `requireAuth` + ownership check + `notFound()`; renders `RecordingWorkstation`.
- `src/app/(app)/settings/page.tsx` — RSC; loads `apiCredentials` providers, renders `SettingsPageContent` (full-page variant of the settings modal).
- `src/app/(app)/onboarding/page.tsx` — RSC; redirects to `/dashboard` if a `plaudConnections` row exists; else renders `OnboardingForm`.
- `src/components/dashboard/workstation.tsx` — composition root + all dashboard state/selection logic.
- `src/components/dashboard/recording-list.tsx` / `recording-row.tsx` — the "timeline": search, sort, density, date-grouped infinite-scroll list.
- `src/components/dashboard/recording-player.tsx`, `src/hooks/use-playback-engine.ts`, `src/components/dashboard/waveform.tsx`, `src/hooks/use-waveform.ts` — audio player, transport-state engine, canvas waveform scrubber, client-side peak decoding.
- `src/components/dashboard/transcription-panel.tsx` — transcript + AI summary (key points / action items) panel.
- `src/components/dashboard/command-palette.tsx` (+ `command-palette-parts.tsx`, `.css`) — `cmdk`-based ⌘K palette.
- `src/components/settings-dialog.tsx`, `settings-content.tsx`, `settings-nav-config.ts`, `src/hooks/use-settings-nav.ts`, `src/hooks/use-settings.ts` — the settings system (modal shell + section router + grouped nav + hash/localStorage persistence + debounced save).
- `src/components/settings-sections/*`, `src/components/settings/*` — individual settings panes (Display, Playback, Sync, Storage, Providers, Webhooks, API Keys…).
- `src/lib/settings/initial-settings.ts` — single source of truth for `InitialSettings` shape + defaults + DB-row coercion.
- `src/hooks/use-theme.ts`, `src/components/theme-provider.tsx`, `src/app/layout.tsx`, `src/app/globals.css` — theming (next-themes class strategy + OKLCH token system).
- `src/components/ui/*` — design-system primitives (`button.tsx` cva variants, `card`, `dialog`, `select`, `sidebar`, `sonner`, `tooltip`, etc.).
- `src/hooks/use-upload-queue.ts`, `use-transcribe-queue.ts`, `use-auto-sync.ts`, `use-list-keyboard-nav.ts`, `use-playback-keyboard.ts` — feature hooks extracted from the workstation.
- `src/components/onboarding-dialog.tsx` / `onboarding-steps.tsx` — the re-runnable 4-step wizard (separate from the first-run `OnboardingForm`).

**How it works.**

*Route-group layout.* The root `src/app/layout.tsx` sets fonts (`Geist`/`Geist_Mono` as CSS vars), `metadataBase`, and wraps everything in `ThemeProvider` → `TooltipProvider delayDuration={200}` → `ConfirmDialogProvider` → `{children}` + `<Toaster/>` + `<RybbitAnalytics/>`. Each parenthesized folder is a Next route-group (URL-invisible) with its own `layout.tsx`: `(app)` adds footer + toaster chrome, `(auth)` is a bare `min-h-screen bg-background` so each auth page can paint full-bleed. The workspace pages are all **RSCs that do auth + data-fetch + decrypt server-side, then hand a fully-hydrated prop bag to one client component** — explicitly to avoid a "waterfall of `/api/settings/user` fetches from three different components" (comment in `dashboard/page.tsx`).

*The Workstation (master-detail).* `Workstation` is the dashboard's brain. It holds `currentRecording`, modal open flags, optimistic `hiddenIds` (deleted rows), `mobileView: "list" | "detail"`, and the fetched `providers`. Layout is a `grid grid-cols-1 lg:grid-cols-3`: `RecordingList` takes one column, `WorkstationDetailPane` takes two. On `<lg`, only one pane shows at a time — selecting a row sets `mobileView="detail"`; the list is hidden via `className={cn("lg:block", mobileView==="detail" && "hidden")}` so **its state (scroll, search, selection) survives** rather than unmounting. State is deliberately split into hooks: uploads→`useUploadQueue`, transcribes→`useTranscribeQueue`, sync→`useAutoSync`, theme→`useTheme`, keyboard→`useListKeyboardNav`. Deletes stay inline because they need `visibleRecordings`/`currentRecording` to pick the next selection. Deletion is **optimistic**: `setHiddenIds` immediately, advance selection to `[idx+1] ?? [idx-1] ?? null`, `DELETE /api/recordings/[id]`, rollback `hiddenIds`+selection on failure. After `router.refresh()` re-supplies server data, a `useEffect` reconciles `currentRecording` and prunes confirmed-deleted ids from `hiddenIds`.

*The timeline (`RecordingList`).* A `RecordingListToolbar` (search input, sort, density) sits above a date-grouped list. `filtered` = client-side search over `filename` + transcript text, then sorted (`newest`/`oldest`/`name`); `grouped` clusters by `dateGroupLabel(startTime)` with sticky `backdrop-blur` headers (skipped when sorting by name). Pagination is **`IntersectionObserver` infinite scroll**: a sentinel `<div ref={sentinelRef} className="h-4"/>` bumps `visibleCount` by `initialChunkSize` (`itemsPerPage`) with `rootMargin: "200px"`. Each `RecordingRow` shows filename + a `transcriptSnippet` (strips `[bracket]` tags and `mm:ss` timestamps, collapses whitespace, ellipsizes at 140 chars) or duration·date fallback; an in-flight badge; a hover-revealed `DropdownMenu` (Open / Delete). Selected row gets `bg-accent shadow-[inset_2px_0_0_0_var(--color-primary)]`. The list exposes an imperative handle (`useImperativeHandle`: `focusSearch/next/prev`) so global keyboard shortcuts and the workstation can drive it. Sort/density changes call `persistSetting(field,value)` — a fire-and-forget `PUT /api/settings/user`.

*Player + waveform.* `RecordingPlayer` is a composition root: `usePlaybackEngine` owns a hidden `<audio src="/api/recordings/[id]/audio">` and all transport state (re-points `src` + `load()` on recording change, wires `timeupdate/loadedmetadata/durationchange/ended/seeked`, exposes `togglePlayPause/seekToRatio/seekRelative/cycleSpeed/toggleMute`; mute stashes prior volume; `PLAYBACK_SPEED_OPTIONS` cycles 0.5–2×). `usePlaybackKeyboard` binds space/←/→/↑/↓. `useWaveform` decodes peaks: if server already has `waveformPeaks` it's `"ready"`; else for recordings `< AUTO_DECODE_MAX_MS` it fetches the audio `arrayBuffer`, runs `decodePeaks(buf, DEFAULT_BUCKETS)` client-side, then **best-effort `POST /api/recordings/[id]/peaks`** to cache; longer recordings show a manual "Generate waveform" button (`"skipped"`). Stale-result guards (`currentIdRef`, `AbortController`) prevent a late decode from clobbering a newer selection. `Waveform` is a `<canvas>` scrubber: DPR-aware, aggregates peaks to a width-derived bar count (`computeVisibleBars`), draws played bars in `--primary` / unplayed in muted, with hover line, playhead glow, a hover time tooltip, and full `role="slider"` keyboard support (←/→ step, shift=coarse, Home/End) that `stopPropagation`s so it doesn't double-seek with the window-level handler.

*Command palette.* `cmdk`-based ⌘K dialog. The cmdk root `value` is **controlled** (`activeValue`) so a `keydown` capture handler can read the highlighted row and run a secondary action (⌘↵ = transcribe the highlighted untranscribed recording) without disturbing cmdk's own Enter (open). Groups: Recordings (capped at `RECORDING_CAP`), Actions (sync/upload/settings/shortcuts), Theme. Actions defer via `runAction = fn => () => { onOpenChange(false); setTimeout(fn,0) }` so a dialog the action opens doesn't race the palette's close transition.

*Settings system.* `SettingsDialog` renders a Radix `Dialog` containing a `SidebarProvider` + `SettingsNavSidebar` (desktop) / `SettingsNavMobile` (a `<md` picker) + a scrollable `<main>` that mounts `<SettingsContent activeSection=.../>`, a plain `switch` that maps a `SettingsSection` union to a section component. Nav is data-driven from `settingsNavGroups` (presentational grouping) flattened to `settingsNav` (the single index source for keyboard nav). `useSettingsNav` resolves the initial section from **URL hash → localStorage (`settings-last-section`)**, writes the active section back to both on change (deep-linkable), focuses the first nav button on open, and runs an arrow/Enter/Space/Escape window listener that bails when focus is in an input. Each section component independently `fetch("/api/settings/user")` on mount, edits local state, and persists via either `useSettings`'s `debouncedSave` (500ms) or an inline optimistic update with rollback-on-failure + `toast.error("…Changes reverted.")` (see `display-section.tsx`). `TranscriptionModelPicker` is a nice escape-hatch pattern: dropdown from curated `knownTranscriptionModels`, OR live-fetched audio models (debounced 400ms, out-of-order guarded via `requestId` ref), OR freeform text, always with a "Custom (type model name)…" sentinel.

*Onboarding.* Two flows. First-run `OnboardingForm` (full page) is a 2-step LED-progress panel (Connect → Complete) with a collapsible `<details>` "How does this work?". The re-runnable `OnboardingDialog` is a 4-step wizard (`welcome/plaud/ai-provider/complete`) driven by a `STEP_ORDER` array with `prevStep`/`nextStep`/`canSkip` derived from `indexOf`; it lazily probes `/api/plaud/connection` and `/api/settings/ai/providers` only while the relevant step is active, renders numbered progress dots, and on finish `PUT`s `{onboardingCompleted:true}`.

*Theming.* `next-themes` with `attribute="class"`, `defaultTheme="system"`, `enableSystem`, `disableTransitionOnChange`. `useTheme` wraps it to add **lazy server persistence** (`PUT /api/settings/user {theme}`) so the choice syncs across devices; it ignores its `initial` arg at runtime because next-themes' inline script restores theme pre-hydration (hence `suppressHydrationWarning` on `<html>`). Tokens are OKLCH CSS custom properties in `globals.css` under `:root` and `.dark`, plus a "Hardware Design System" alias layer (`--panel-surface`, `--led-active`, `--accent-green/blue/purple`) and a theme-stable `--auth-brand` pair that intentionally does not invert.

**Contracts & shapes.**

`InitialSettings` (the prop bag threaded from RSC to every client component), `src/lib/settings/initial-settings.ts`:
```ts
export interface InitialSettings {
    dateTimeFormat: "relative" | "absolute" | "iso";
    recordingListSortOrder: "newest" | "oldest" | "name";
    itemsPerPage: number;
    listDensity: "comfortable" | "compact";
    theme: "light" | "dark" | "system";
    defaultPlaybackSpeed: number;
    defaultVolume: number;
    autoPlayNext: boolean;
    playerScrubber: "waveform" | "slider";
    syncInterval: number;
    autoSyncEnabled: boolean;
    syncOnMount: boolean;
    syncOnVisibilityChange: boolean;
    syncNotifications: boolean;
    browserNotifications: boolean;
}
// defaults: itemsPerPage 50, defaultVolume 75, defaultPlaybackSpeed 1.0,
// theme "system", playerScrubber "waveform", syncInterval 300_000
```

Settings nav contract (`settings-nav-config.ts`):
```ts
export type NavItem = { name: string; id: SettingsSection; icon: typeof Bot };
// groups: AI(providers,transcription,summary) · Plaud(plaud-account,sync) ·
// Personalize(playback,display,notifications) · Data(storage,export) ·
// Integrations(api-keys,webhooks) · Advanced(dev, dev-only)
export const settingsNav: NavItem[] = settingsNavGroups.flatMap(g => g.items);
export const SETTINGS_STORAGE_KEY = "settings-last-section";
```

Button variants (`src/components/ui/button.tsx`, cva):
```ts
variant: default | destructive | outline | secondary | ghost | link
size:    default(h-9) | sm(h-8) | lg(h-10) | icon | icon-sm | icon-lg
// asChild → Radix <Slot>; data-slot="button"
```

Playback speeds (`use-playback-engine.ts`):
```ts
export const PLAYBACK_SPEED_OPTIONS = [
  {label:"0.5x",value:0.5},{label:"0.75x",value:0.75},{label:"1x",value:1.0},
  {label:"1.25x",value:1.25},{label:"1.5x",value:1.5},{label:"2x",value:2.0}] as const;
```

Client-side feature endpoints the UI calls (all relative, fetch from client components):
```
GET/PUT  /api/settings/user          // settings load + persist (PUT is partial {field:value})
GET      /api/settings/ai/providers
POST     /api/settings/ai/providers/models   // {provider,apiKey,baseUrl} → {models:[{id,name}]}
GET      /api/recordings/[id]/audio          // <audio src> + waveform decode source
POST     /api/recordings/[id]/peaks          // {peaks:number[]} best-effort cache
DELETE   /api/recordings/[id]
POST     /api/recordings/upload              // multipart "file"
GET      /api/plaud/connection
```
Waveform constants (`src/lib/audio/waveform`): `AUTO_DECODE_MAX_MS`, `DEFAULT_BUCKETS`. Theme tokens read by the canvas at draw time: `--primary` (fallback `oklch(0.6171 0.1375 39.0427)`), `--muted-foreground`.

**Notable patterns & decisions.**
- **RSC-as-data-loader, one client island.** Every workspace page is a thin async server component that authenticates, runs parallel Drizzle queries, decrypts at-rest fields, and passes a fully-formed prop bag (incl. `initialSettings`) to a single `"use client"` root. This collapses the first-paint fetch waterfall and keeps decryption keys off the client.
- **Hook-per-feature extraction.** The workstation is a composition root; concerns are surgically removed into `useUploadQueue/useTranscribeQueue/useAutoSync/usePlaybackEngine/useWaveform/useSettingsNav`, each with a doc comment stating its single responsibility and why it's split (e.g. keyboard separated from engine so the test surface "control an audio element" stays distinct from "react to keys").
- **Optimistic UI everywhere with rollback + toast.** Deletes (`hiddenIds`), uploads (`pending:`-namespaced placeholder rows), settings edits (revert + "Changes reverted" toast), waveform peak caching (best-effort, never toasts on failure — "graceful degradation, not an error").
- **Controlled-cmdk trick** to layer a secondary keyboard action on a library that otherwise owns its highlight state; `setTimeout(fn,0)` deferral to dodge dialog-stacking races.
- **Imperative refs across component boundaries** (`RecordingListHandle`, `useConfirm()` promise-returning confirm provider in the root layout) instead of prop-drilling callbacks.
- Tailwind v4 + OKLCH tokens + a semantic alias layer ("hardware/rack/panel/LED" naming) gives the skin its audiophile identity while components only ever reference semantic vars. `cva` + `cn` (clsx+tailwind-merge) + Radix `Slot`/`asChild` is the standard shadcn idiom throughout `ui/`.
- Mobile master-detail via `hidden`-toggling (state-preserving) rather than conditional unmount; sticky `backdrop-blur` headers; per-button `sm:`/`md:` collapse to icon-only.

**Relevance to Plaude Local (anti-Plaud, local-first).**

*Adapt directly (design, not code):*
- **The whole UX skeleton maps 1:1 to our app.** A master-detail "workstation" (recording list ↔ detail pane with player + speaker-labelled transcript) is exactly the Sessions UI the HANDOFF calls our top gap. Adopt: date-grouped list with sticky headers, transcript-text search, density/sort toggles, optimistic delete, `IntersectionObserver` pagination, hover row actions, and selection-driven detail pane.
- **The canvas `Waveform` + `usePlaybackEngine` design is highly reusable** and framework-agnostic — but in Tauri we'd feed peaks from Rust (we already decode audio) over a Tauri command/`convertFileSrc` instead of fetching an HTTP `arrayBuffer` and `POST`ing peaks back. The `role="slider"` a11y, DPR scaling, played/unplayed coloring, and `--primary` token-reading at draw time all transfer.
- **`InitialSettings` single-source-of-truth + section-router + hash/localStorage nav** is a clean settings pattern for our Settings view. Our equivalents: ASR model (Whisper/Parakeet), diarization model + the "download diarization models" button (HANDOFF §3), playback prefs, theme. The `TranscriptionModelPicker`'s curated-list-with-custom-escape-hatch is a good template for our **local model picker / download manager** (swap "fetch models from provider" for "list bundled/installed ONNX models").
- **Command palette + global keyboard nav + numbered onboarding wizard + `next-themes` OKLCH token theming** are all worth porting as-is conceptually.

*Does NOT apply (cloud/web/multi-tenant machinery):* `requireAuth`/`session.user.id` row-scoping, `env.IS_HOSTED` gating and the `RebrandBanner`, the entire `(auth)`/`(admin)` route groups, server-side field encryption (we're single-user on-device — file perms suffice), the `/api/settings/user` round-trips and debounced server saves (replace with a local store / Tauri SQLite settings table or a JSON file — no network, no optimistic-rollback-on-network-failure needed), `RybbitAnalytics`, webhooks/API-keys/storage-quota/Plaud-sync sections, BYO OpenAI-compatible provider config and the `/v1/models` live fetch (our ASR/summary is local). The `next-themes` cross-device server sync collapses to a single local preference.

*AGPL-3.0 caveat:* riffado is AGPL-3.0 and our app must not link/copy its source. Everything above is **study the design and reimplement** in our Tauri/Rust+React stack. Note specifically that `src/components/ui/*` is shadcn-derived (MIT-origin generated code), and the Radix/Tailwind/cva/`cmdk`/`next-themes`/`sonner` libraries are permissively licensed and may be used directly — but the riffado-authored composition (Workstation, RecordingList, Waveform logic, settings system, hooks) is AGPL and may only be referenced, not lifted. Reimplement from the documented behavior, not by pasting files.

## Notifications, Admin & Testing

**What it is.** Three loosely-coupled subsystems on the server side of riffado: a **multi-channel notification fan-out** (transactional SMTP email via Nodemailer + React Email, push via the Bark iOS app, and client-side browser notifications) fired when Plaud recordings sync; a **hosted-only admin console** (email + IP + reauth-cookie gate, audited mutation dispatcher, suspension, install-script analytics); and a **Vitest suite** (~55 test files) split into unit/integration/regression/admin buckets plus a static-analysis "PII guard."

**Key files.**
- `src/lib/notifications/email.ts` — Nodemailer transporter + `sendEmail` / `sendEmailWithError` / `sendNewRecordingEmail` / `sendPasswordResetEmail` / `sendTestEmail`.
- `src/lib/notifications/bark.ts` — `sendBarkNotification` / `sendNewRecordingBarkNotification` (HTTP POST to a Bark push URL, 3s `AbortController` timeout).
- `src/lib/notifications/browser.ts` — Web Notification API wrappers (`requestNotificationPermission`, `showBrowserNotification`, `showNewRecordingNotification`, `showSyncCompleteNotification`).
- `src/lib/notifications/email-templates/*.tsx` — React Email components (`new-recording-email.tsx`, `password-reset-email.tsx`, `test-email.tsx`) + `styles.ts` / `brand-colors.ts` inline-style tokens.
- `src/lib/admin/guard.ts` — the gate: `evaluateAdminGate`, `requireAdminPage` / `requireAdminApi` / `requireAdminMutation`, `isAdminEmail`.
- `src/lib/admin/elevated-cookie.ts` — HMAC-signed reauth cookie (`signElevatedCookie`, `verifyElevatedCookie`, TTL checks).
- `src/lib/admin/ip-allowlist.ts` — fail-closed CIDR matcher (`ipMatchesAllowlist`, `clientIpFromHeaders`, `warnIfIpAllowlistTrustsXff`).
- `src/lib/admin/actions.ts` — transactional, audited mutation dispatcher (`suspendUser`, `unsuspendUser`, `forceDisconnectPlaud`, `softDeleteRecording`, `logCsvExport`).
- `src/lib/admin/suspension.ts` — cooperative-suspension predicate (`isSuspended`).
- `src/lib/admin/install-hits.ts` — install-script hit counter (`recordInstallHit`, `getInstallHitStats`).
- `vitest.config.ts` — Vitest config (`@` alias, node env).
- `src/tests/**` — unit, `regressions/<issue#>-*.test.ts`, `admin/*.test.ts`, `transcription/`, `fixtures/sample.mp3`.

**How it works.**

*Notifications.* The sync worker (`src/lib/sync/sync-recordings.ts`) is the only production caller of the recording channels. Email: `getTransporter()` lazily memoizes a single `nodemailer.Transporter` only when `isSmtpConfigured()` (from `@/lib/smtp`) is true, picking port `465` if `SMTP_SECURE` else `587`. `sendEmail` returns a boolean and **swallows errors** (best-effort, used by sync); `sendEmailWithError` throws with `code`-specific messages (`ETIMEDOUT`/`ECONNREFUSED`/`EAUTH`) and backs the user-facing "send test email" button. `sendNewRecordingEmail` renders `NewRecordingEmail` with `@react-email/render`'s `render(..., { pretty: false })` to HTML, then hand-builds a plaintext fallback; the `pretty:false` flag is deliberate — the inline comment notes it keeps Prettier out of the Next 16 runtime require graph. Bark: `sendBarkNotification` builds a payload object, races a `fetch` POST against a 3000ms `AbortController` timeout, and returns a boolean (never throws into sync). Browser notifications run entirely client-side off the `Notification` global, tagged (`new-recording`, `sync-complete`) so repeats collapse. Channel selection is driven by per-user columns (`emailNotifications`, `barkNotifications`, `browserNotifications`, `notificationEmail`, `barkPushUrl`) read by the sync worker.

*Admin.* `evaluateAdminGate(opts)` is the single chokepoint. It **returns `null` (→ 404 / `notFound`) unless every layer passes**: `env.IS_HOSTED` true, non-empty `ADMIN_EMAILS`, optional `ADMIN_IP_ALLOWLIST` match against `clientIpFromHeaders`, a Better-Auth session whose lowercased email is in the allowlist, and a valid `riffado_admin_elev` cookie whose `userId` matches the session and is within the reauth TTL. For reads, a missing/expired cookie yields `{ mode: "reauth", returnTo }` (UI sends the admin to reauthenticate); for mutations it hard-fails to `null`. On success it inserts an `adminAuditLog` row (route, method, ip, UA) — a log-insert failure is caught and logged, not fatal (audit-of-access is best-effort). Three thin wrappers expose it: `requireAdminPage` (404s server components), `requireAdminApi` / `requireAdminMutation` (throw `AppError(NOT_FOUND, 404)`). `isAdminEmail` is a render-only predicate that explicitly does **not** verify the cookie. The elevated cookie is `userId.issuedAt.hmacSHA256(userId.issuedAt, BETTER_AUTH_SECRET)`, verified with `timingSafeEqual` after a length-equality check; two TTLs (`ADMIN_REAUTH_TTL_MINUTES`, `ADMIN_MUTATION_TTL_MINUTES`) give mutations a shorter freshness window than reads.

`actions.ts` is the second line of defense: each mutation runs inside `db.transaction`, takes a `SELECT ... FOR UPDATE` row lock to serialize concurrent toggles, asserts `reason` (≥4 chars), and writes the mutation **and** its `adminActionLog` row (with `before`/`after` JSON snapshots) in the **same transaction** so an audit-insert failure rolls the mutation back. Idempotent paths still write a `*_noop` audit row (e.g. `suspend_user_noop`). `softDeleteRecording` only sets `deletedAt` (blob cleanup is deliberately separate). `getInstallHitStats` aggregates the `install_script_hits` table via Drizzle `sum`/`groupBy` for a dashboard tile; `recordInstallHit` upserts a `(day, version)` row with `onConflictDoUpdate` and normalizes versions to `latest` / valid tag / `invalid` to cap cardinality. Both are **no-ops when `!IS_HOSTED`**.

*Testing.* `vitest run` (node environment, single `@`→`src` alias). Tests mock at the module boundary with `vi.mock` (`@/db`, `@/lib/auth`, `@/lib/env`, `openai`, storage factory, webhooks). Notable patterns: `guard.test.ts` proves failures map to 404 AppErrors; `elevated-cookie.test.ts` exercises sign/verify/tamper/TTL with `vi.useRealTimers`; `csv-escape.test.ts` is a **copy** of the route's inline `csvEscape` re-tested for formula-injection (the test acknowledges the duplication and asks future refactors to port it); `queries-pii.test.ts` is a **static source-text guard** — it `readFileSync`s `src/db/queries/admin.ts`, strips comments, and fails if any `FORBIDDEN_TOKENS` (e.g. `transcriptions.text`, `plaudConnections.bearerToken`, `api_key`) appears. `regressions/<issue#>-*.test.ts` each pin one fixed bug to its GitHub issue number with a prose header (e.g. `161` proves `buildChatCompletionParams` swaps `max_tokens`→`max_completion_tokens` for `gpt-5*`/`o*` models).

**Contracts & shapes.**
```ts
// notifications/email.ts
interface EmailOptions { to: string; subject: string; html: string; text?: string }
function sendEmail(options: EmailOptions): Promise<boolean>          // swallows errors
function sendEmailWithError(options: EmailOptions): Promise<void>    // throws coded msg
sendNewRecordingEmail(email, count, recordingNames?): Promise<boolean>

// notifications/bark.ts
interface BarkNotificationOptions { title?; subtitle?; body: string; badge?; sound?;
  icon?; group?; url?; level?: "critical"|"active"|"timeSensitive"|"passive"; ... }
sendBarkNotification(pushUrl, options, timeoutMs = 3000): Promise<boolean>

// admin/guard.ts
type AdminGuardOk     = { mode: "ok"; user: {id,email}; elevatedIssuedAt: number }
type AdminGuardReauth = { mode: "reauth"; user: {id,email}; returnTo: string }
```
```ts
// admin/elevated-cookie.ts
export const ADMIN_ELEVATED_COOKIE = "riffado_admin_elev";
// cookie value format: `${userId}.${issuedAt}.${HMAC-SHA256(userId.issuedAt, BETTER_AUTH_SECRET)}`
```
DB columns (`src/db/schema.ts`):
```
users.suspendedAt (timestamp, null), users.suspendedReason
userPreferences: sync_notifications, browser_notifications, email_notifications,
  bark_notifications, notification_sound, notification_email (varchar 255),
  bark_push_url (text)
admin_audit_log  (id, admin_user_id FK→users set null, admin_user_email, route, method, ip, user_agent)
admin_action_log (id, admin_user_id set null, admin_user_email, action, target_user_id, target_resource_id, reason, before jsonb, after jsonb, ip)
install_script_hits (day date, version text, count int)  PK(day, version)
```
Env vars: `SMTP_HOST` `SMTP_PORT` `SMTP_SECURE` `SMTP_USER` `SMTP_PASSWORD` `SMTP_FROM`, `APP_URL`, `IS_HOSTED`, `ADMIN_EMAILS` (csv), `ADMIN_IP_ALLOWLIST` (csv CIDR), `ADMIN_REAUTH_TTL_MINUTES`, `ADMIN_MUTATION_TTL_MINUTES`, `BETTER_AUTH_SECRET`. Audit actions: `suspend_user`, `suspend_user_noop`, `unsuspend_user`, `force_disconnect_plaud(_noop)`, `soft_delete_recording`, `csv_export_<kind>`. Scripts: `"test": "vitest run"`, `"test:watch": "vitest"`.

**Notable patterns & decisions.**
- **Fail-closed everywhere.** Empty IP allowlist = disabled; non-empty but unparseable = deny. Admin gate failures uniformly surface as **404 (not 403)** to hide the console's existence. Reads degrade to a `reauth` prompt; mutations never do.
- **Audit-with-the-mutation.** Mutation + `adminActionLog` insert share one transaction with `FOR UPDATE` locks; an unaudited mutation is treated as worse than a refused one. `before`/`after` JSON snapshots + `_noop` rows give a complete paper trail.
- **Email is a React-render-to-HTML pipeline.** Components from `@react-email/components` styled with inline JS objects (`styles.ts`/`brand-colors.ts`), rendered with `pretty:false` for a Next-16 bundling reason. Best-effort (`sendEmail`) vs strict (`sendEmailWithError`) split.
- **Bark** is a clever zero-SDK push: a plain authenticated POST to a user-pasted `https://api.day.app/<key>` URL, hard-timeout-boxed so a slow push can't stall sync.
- **Tests as guardrails, not just behavior**: the `queries-pii` source-grep and the duplicated-on-purpose `csvEscape` test encode security invariants that pure unit tests would miss. Regression tests are issue-numbered and self-documenting.

**Relevance to Plaude Local (anti-Plaud, local-first).**
- **ADAPT (notifications).** A "session finished / transcription ready" notification is the natural fit. On macOS, use **native OS notifications via Tauri** (`tauri-plugin-notification`) — this is the desktop analogue of `browser.ts`, far simpler than its consumer-side ` Notification` permission dance, and fully local. The channel-preference pattern (per-user boolean toggles + a sound flag) maps cleanly to a single-user settings table in `history.db`. **DO NOT** port the SMTP/Nodemailer/React-Email machinery or Bark: those are cloud-delivery channels for a hosted multi-tenant service; a local single-user app has no remote recipient and no mail server. (Bark is conceptually interesting only if you later want phone push for the deferred iPhone target.)
- **ADAPT (testing strategy).** The structure is the takeaway: per-feature unit tests + `regressions/<issue#>-*` pinning fixed bugs to their tracker, plus **static-source guard tests** (the PII-grep idea translates directly — e.g. assert your Rust diarization/ASR code never logs raw transcript text or audio paths). Our stack is `cargo test` (already 79 passing) + a future Vitest/bun-test suite for the React frontend; the module-boundary mocking and the `sample.mp3` fixture convention are worth mirroring with a short bundled WAV fixture.
- **DOES NOT APPLY (admin).** The entire `src/lib/admin/*` subsystem — HMAC reauth cookie, IP allowlist, suspension, install-script analytics, CSV PII export, audit logs — is **multi-tenant hosted-operator machinery**, all explicitly gated behind `IS_HOSTED` and inert otherwise. A local single-user desktop app has no operator, no other users to suspend, no remote IPs, and (per project goals) should emit no install telemetry. Skip it wholesale. The one transferable idea is the `before`/`after` transactional-audit pattern, which could inform a local *undo/history* log for destructive edits — but that's a re-implementation, not a port.
- **AGPL caveat.** riffado is AGPL-3.0; study these designs but do **not** copy source (templates, the CIDR matcher, `csvEscape`, the gate) into Plaude Local. The notification *concept* and *schema shape* are fine to re-implement from scratch on Tauri/Rust; verbatim TS/TSX must not land in the codebase.

---

# Lessons for Plaude Local (anti-Plaud, local-first)

## Lessons for Plaude Local (anti-Plaud, local-first)

We read riffado as a *mature reference implementation* of our product category. But riffado is the **philosophical inverse** of Plaude Local: it is a *companion that pulls from Plaud's cloud* into a *multi-tenant web server*; we *are* the recorder, capturing on-device (mic + CoreAudio system-audio tap) and running ASR/diarization fully local via `transcribe-rs` + ONNX. So the editorial rule for every subsystem is: **adapt the data model and the hard-won correctness patterns; drop the cloud/web/multi-tenant machinery.**

### The AGPL-3.0 boundary (read this first)
riffado is **AGPL-3.0**; Plaude Local is itself meant to be open-source, so contamination is a real risk. **Facts and interfaces are safe to mirror** — table/column names, the `op_` key format, the `t=…,v1=…` signature scheme, SRT/VTT formats (open standards), the `v1:iv:tag:ciphertext` layout, the `TranscriptionStyle` enum shape. **Concrete source is copyleft and must NOT be copied or paste-translated**: `schema.ts`, the migration SQL, `encryption.ts`/`fields.ts`, the Plaud client files, `sync-recordings.ts`, the CIDR matcher, the cookie HMAC, the Workstation/Waveform components, the prompt-template strings (copyrightable expression). Re-implement clean-room in Rust/React from the documented behavior, and document our work as independently authored. Note one nuance: `src/components/ui/*` is shadcn-derived (MIT-origin) and the underlying libs (Radix, Tailwind, cva, cmdk, next-themes, sonner) are permissive — we may use those directly; only riffado's *authored composition* is encumbered.

### ADAPT — architecture & data model
- **The relational spine maps ~1:1 onto our SQLite `history.db`.** Adopt `recordings` (duration ms, start/end, filesize, `file_md5`, `storage_path`, `is_trash`, **soft-delete `deleted_at` tombstone**, `waveform_peaks`) → `transcriptions` (text, `detected_language`, `provider`, `model`) → `ai_enhancements` (summary, action_items, key_points). Adopt the **one-recording→one-transcript→one-enhancement** split (their `UNIQUE(recording_id, user_id)` idiom, minus `user_id`).
- **Add the table riffado lacks: `segments`.** riffado's transcript is a flat `text` blob — its `diarized_json` path even *flattens* speaker turns to `"speaker: text"` strings and throws away timestamps. Our Fase 2 sherpa-onnx diarization output needs a first-class `segments(recording_id, speaker_label, start_ms, end_ms, text)` table. **This is where we diverge upward, not copy** — and it must propagate into our SRT/VTT export as **one cue per diarized segment with a `Speaker N:` prefix**, fixing riffado's single-cue-per-recording weakness.
- **Idempotent, resumable processing keyed by a stable id + version** (their `plaudFileId` + `version_ms` diff loop) → our `session.rs` should record a stable recording id + mtime so re-processing is a diff, not a duplicate.
- **`waveform_peaks` as a write-once normalized-float array** is the single most reusable idea — but invert the architecture: the Rust side already has decoded PCM during capture/diarization, so **compute peaks server-side at capture time in `recorder.rs`/`session.rs`**, eliminating riffado's browser-decode + POST-back round trip entirely. Keep their defaults (500 buckets, `[0,1]` norm, 32–2048 clamp).
- **Pure in-process metadata extraction** (their no-ffprobe `music-metadata` decision) → use the `symphonia` Rust crate for duration/codec, zero external binary.
- **Boot-time idempotent migrations** (their advisory-locked runner) → run migrations on every launch; we're single-process single-user so drop the advisory lock, but keep the ordered-file + journal discipline.
- **Zod-validated single config object** (`env.ts`, "never read env directly") → one validated Rust settings struct at startup (model paths, storage dir).
- **`InitialSettings` single-source-of-truth + section-router settings UI + `TranscriptionModelPicker`'s curated-list-with-custom-escape-hatch** → our Settings view and **local model picker / download manager** (swap "fetch provider models" for "list bundled/installed ONNX models").

### ADAPT — correctness patterns worth stealing wholesale
- **Invert riffado's no-status-column mistake.** Their implicit "transcript row exists = done" costs them retry/backoff/observability. Our sessions are long-running and crash-prone, so **add an explicit state column** (`pending|transcribing|done|failed`); on startup, reset anything stuck in `transcribing` → `pending`. This drives the live Sessions-UI indicator.
- **Bounded worker** (their slice-of-5 `Promise.allSettled`) → a `tokio::Semaphore(N)` feeding the diarizer + ASR runtimes; keep "one failure never aborts the batch" (`Result` accumulation).
- **Single-flight coalescing** (`inFlightSyncs` Map) → a `Mutex<HashSet<SessionId>>` so "transcribe now" and auto-transcribe-on-stop don't double-run.
- **`FOR UPDATE` re-check + tombstone guard after slow I/O** → after a long ASR pass, re-check the session wasn't deleted mid-flight before writing, and clean up partial artifacts. Same TOCTOU lesson, local files + SQLite.
- **Versioned, pass-through field encryption** (`v1:` prefix + legacy detection) → re-implement in Rust (`aes-gcm` crate) for the *one* secret we'll actually have: a user-supplied OpenAI-compatible API key for optional cloud summaries.
- **The `ProviderPreset` + `TranscriptionStyle` abstraction** → a Rust enum (`Whisper{base_url} | Chat{base_url} | GeminiNative | LocalOnnx`) so one config screen targets our bundled local Whisper/Parakeet *and* optional Ollama/LM Studio "cloud boost" — their localhost presets ARE our local story.
- **The worker message protocol** (`{type:"transcribe"} → progress/complete/error`) → Tauri `command` + `emit`/`listen` events; do progress *better* (emit real percentages — our native pipeline can report them, theirs only ever sent the string `"transcribing"`).
- **Outbound signed webhooks are genuinely useful even locally** — fire `session.ended`/`transcription.completed` to the user's own n8n/Obsidian. Adopt the `t=…,v1=hmac` + ±300s tolerance + `timingSafeEqual` spec and a SQLite-backed durable-retry worker (status/next_attempt_at/attempts + backoff), giving retry-across-restart with no broker.
- **Path-traversal hardening** (`LocalStorage.getFilePath` two-layer guard) → port into Rust (`canonicalize` + `starts_with(base)`) for the planned **KB folder**; keep `storage_path` relative to a configurable root so the KB is relocatable.
- **Native OS notifications** via `tauri-plugin-notification` (the desktop analogue of their `browser.ts`).
- **Testing structure**: per-feature unit tests + issue-numbered `regressions/<#>-*` + **static-source guard tests** (their PII-grep idea → assert our Rust never logs raw transcript text / audio paths). We have 79 `cargo test` passing; add a `bun test` frontend suite + a short bundled WAV fixture.

### DROP — cloud/web/multi-tenant machinery (does NOT apply)
- **The entire Plaud-cloud integration** (`src/lib/plaud/*`): OTP login, `-302` region redirects, UT→WT escalation, workspace lists, Webshare proxy bot-evasion, `temp-url` signed downloads. We capture locally; there is no cloud to pull from. The *only* salvageable use is an **optional one-shot "import my old Plaud library" migration importer** — clearly fenced off from the always-local core, clean-room from the documented endpoint sequence, and explicitly **without** the ToS-sensitive UA-spoofing/residential-proxy layer.
- **The whole sync engine premise** (discover-in-cloud → download → store) and the browser `useAutoSync` scheduler.
- **Postgres + Better Auth + multi-tenancy**: `users/sessions/accounts/verifications`, `userId`-on-every-query, `onDelete: cascade`, `FOR UPDATE SKIP LOCKED` multi-process plumbing, per-user fairness queues, `IS_HOSTED` branches. We are single-user SQLite — drop `user_id` from every table.
- **The hosted admin tier** (`src/lib/admin/*`): HMAC reauth cookie, IP allowlist, suspension, audit logs, install-script analytics, CSV PII export. No operator, no other users. (The lone transferable idea: the transactional `before`/`after` audit pattern could inform a local *undo/edit-history* log.)
- **Public API + perimeter defense**: `op_` API keys, two-bucket rate limiting, the DNS-pinning SSRF guard (the user owns the webhook target locally — HTTPS-only + a light private-IP check suffices), HMAC'd bucket keys to prevent tenant enumeration.
- **Storage cloud machinery**: S3/R2/MinIO, presigned URLs, 302-redirect reads, `createUserStorageProvider(userId)` tenancy seam, env-driven backend selection. Our `LocalStorage` is just `~/Library/Application Support/com.pais.handy/recordings/`; our "signed URL" is a Tauri `convertFileSrc` asset path.
- **The browser-WASM transcriber** — a downgrade for us; we have native ONNX. And its **CDN model fetch** is antithetical to our offline ethos: keep bundling models in `handy/src-tauri/resources/models/` and auto-installing on first run.
- **Server-held-key envelope encryption** (decrypt-at-request-so-the-server-can-run-AI) — the exact trust model we are the opposite of. Our data never leaves the device. **Key management is the one place riffado's answer doesn't transfer**: their `ENCRYPTION_KEY` env var assumes an operator; we should use the **macOS Keychain** (or Argon2-derive from a passphrase), and encrypt/decrypt inside the single Rust data-access layer so the React frontend never sees ciphertext (riffado's manual call-site discipline leaks ciphertext if a dev forgets a `decryptText`).
- **The >25 MiB ffmpeg→Opus compression dance** (irrelevant for on-device inference), SMTP/Nodemailer/React-Email/Bark, Rybbit analytics, the `RebrandBanner`, cross-device theme sync, HTTP Range serving (Tauri serves files directly).

### Concrete roadmap mapping
- **Sessions UI (top gap, HANDOFF §3)** ← riffado's `Workstation` master-detail skeleton: date-grouped list with sticky headers + transcript search + density/sort + optimistic delete + IntersectionObserver pagination, selection-driven detail pane. Add the start/stop + Mic/System selector + live indicator (driven by our new explicit state column), replacing the `--toggle-session` CLI flags.
- **Local models on iPhone / model manager** ← `ProviderPreset`/`TranscriptionStyle` enum + `TranscriptionModelPicker` curated-list pattern + lazy/memoized model load (but **invert** their `transcribeInBrowser` teardown-per-file bug — keep models resident across recordings).
- **Diarization timeline** ← the new `segments` table (our addition) + the canvas `Waveform`/`use-playback-engine` design feeding peaks from Rust, with `role="slider"` a11y, DPR scaling, and `--primary`-token coloring. Improve the export to per-segment SRT/VTT + a Markdown speaker-labelled transcript.
- **KB folder** ← `StorageProvider` path-traversal guard + relative-`storage_path` + compensating-delete orphan-avoidance, all re-implemented in Rust.
- **Agent-over-KB** ← riffado has **no agent/LLM-tool surface** (its "automation API" is a read API + webhooks), so there is nothing to copy here; our best leverage is the read-data-model shapes (`V1Recording`/`V1Transcript`/`V1Summary` + keyset cursor pagination) as the clean local IPC contract a future agent queries over our sessions.
