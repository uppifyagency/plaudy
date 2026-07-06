# Plaude Local — Onboarding per Gianni (in-depth, aggiornato 2026‑06‑23)

> **⚠️ ADDENDUM 2026‑07‑05 — leggi anche [HANDOFF-AUTOCAPTURE.md](HANDOFF-AUTOCAPTURE.md).** Due punti di questo documento sono superati: (1) il trigger auto‑capture **non è più accantonato** — sensore per‑processo CoreAudio (PID nostro escluso), validato E2E live, ora **102 test Rust**; il pivot mic‑VAD è retrocesso a fallback opzionale (la saga in §4 è storia, non stato). (2) La decisione AI title/summary (§12 di HANDOFF.md) è **risolta: la via è l'MCP locale** — gli agent del cliente/utente riassumono on‑demand col proprio abbonamento; niente sidecar LLM.

Benvenuto Gianni 👋. Questo documento è scritto per farti **partire produttivo oggi**: stato reale, come si builda, mappa del codice, le trappole che ci sono costate tempo (così non le ripaghi), e i prossimi passi. È self‑contained, ma le verità "canoniche" stanno in:

- [`docs/HANDOFF.md`](HANDOFF.md) — briefing autorevole per agenti (build, sicurezza, verifica).
- [`docs/CODEBASE.md`](CODEBASE.md) — riferimento tecnico esteso (architettura, file map, data model).
- [`CLAUDE.md`](../CLAUDE.md) — istruzioni di progetto + status in cima.
- [`docs/DECISIONS.md`](DECISIONS.md) — verdetto su cosa adottare/scartare dal teardown di `riffado`.

---

## 0. Cos'è, in 2 minuti

**Plaude Local** = alternativa **local‑first, offline, privata** a Plaud (registratore vocale + "chi ha detto cosa"), per **macOS**, costruita estendendo il fork di **[Handy](https://github.com/cjpais/Handy)** (Tauri 2: Rust in `handy/src-tauri/`, React/TS in `handy/src/`).

Tutto è on‑device: cattura, ASR (Parakeet/Whisper via ONNX) e diarizzazione girano **localmente**; **niente esce dal Mac**. Claude si collega alla libreria tramite un **MCP server locale** (`handy/mcp/`).

Il nostro codice vive **dentro `handy/`** (estendiamo il fork in‑place), non in una cartella separata.

---

## 1. Stato OGGI (2026‑06‑23)

Ultimi commit su `main`:
```
1d88db2 feat(tray): menu-bar "ear" listening signal + experimental auto-capture engine (off)
52ff393 feat(history): session-card result view in Cronologia
bd0c996 feat: dual-stream meeting capture, graffetta tray, local MCP, bleed de-dup
```

### ✅ Solido e validato live (puoi fidarti)
- **Cattura sessioni**: mic, audio di sistema (CoreAudio Process Tap), e **meeting dual‑stream** (mic="Me" + sistema="Speaker N") → un transcript unico attribuito per speaker.
- **Diarizzazione locale** (sherpa‑onnx), **anti‑eco** (`drop_bleed`) per quando l'audio esce dalle casse e il mic lo ri‑cattura.
- **Graffetta** = toggle nella menu‑bar (un click avvia/ferma il meeting).
- **Orecchio** 👂 nella menu‑bar quando una sessione registra (vedi §5).
- **Cronologia a card** (vedi §6): icona‑sorgente, titolo‑topic, data·durata·sorgente, chip‑speaker, timeline collassabile + player + azioni.
- **MCP server** read‑only (`handy/mcp/`) verificato contro il vero `history.db`.
- **Self‑healing** all'avvio (sessioni interrotte/righe stale recuperate).
- **98 test Rust + 4 test MCP verdi.**

### 🧪 Sperimentale, SPENTO di default
- **Auto‑capture engine** (`managers/auto_capture.rs` + `audio_toolkit/audio/output_sensor.rs`): doveva far partire la registrazione **da sola** quando esce audio dalle casse. **Il trigger su audio‑di‑sistema è accantonato** — vedi la saga in §4. La *logica di stop* e la *probation* (scarta i falsi avvii senza sporcare la Cronologia) **funzionano e sono testate**; manca un **segnale di trigger affidabile** (il prossimo passo è il **mic‑VAD**). `auto_capture_enabled` è `false`.

### ❌ Non fatto
- `.app`/`.dmg` firmato+notarizzato (serve **Xcode completo**; ora siamo CLT‑only).
- AI title/summary delle card (bloccato sulla decisione provider, §12 in HANDOFF.md).
- Target iPhone (serve Xcode).
- i18n delle nostre chiavi oltre `en`/`it` (gli altri locale cadono in inglese).

---

## 2. Build / run / test (questo Mac: Apple Silicon, solo CLT, no Homebrew/Xcode)

**Ogni shell ha bisogno di questo prelude** — la toolchain è installata ma non sul PATH non‑interattivo:
```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5   # CMake 4 ha tolto le policy pre‑3.5 (whisper.cpp non configura senza)
export HANDY_FORCE_AI_STUB=1              # la CLT non ha il macro plugin @Generable → stub Apple Intelligence
```

| Cosa | Comando |
| --- | --- |
| App completa (dev) | `cd handy && bun tauri dev` (rigenera `src/bindings.ts` all'avvio) |
| Test backend | `cd handy/src-tauri && cargo test --lib` → **98 passed** |
| Test MCP | `cd handy/mcp && bun test` → **4 pass** |
| Type‑check FE | `cd handy && bunx tsc --noEmit` |
| Lint FE (i18n enforced) | `cd handy && bun run lint` |
| Binario release | `cd handy && bun tauri build --no-bundle` → `target/release/handy` (~36 MB) |

- **Dati app:** `~/Library/Application Support/com.uppify.plaudy/` → `history.db` (SQLite) + `recordings/` (`*.session.pcm` live, `*.wav` finalizzati). Impostazioni: `settings_store.json` (chiavi sotto `settings.*`).
- **Log:** `~/Library/Logs/com.uppify.plaudy/handy.log` (e nello stdout di `bun tauri dev`).

> ⚠️ Il dev server **osserva `src-tauri/`** e ricompila+rilancia a ogni salvataggio `.rs`. Se tocchi più file in sequenza vedrai errori *intermedi* (es. `non-exhaustive match`) finché non hai completato: è normale. Per una conferma autorevole di compilazione, ferma il dev server e lancia `cargo check --lib` (altrimenti i due `cargo` litigano sul lock di `target/`).

---

## 3. Mappa del codice (il nostro delta su Handy)

### Backend (`handy/src-tauri/src/`)
- **`managers/session.rs`** — sessioni long‑form + meeting dual‑stream. `Track`, `ActiveSession`, `start_sources`/`stop`/`cancel`/`toggle_sources`, `mix_tracks`, `finalize_session`, `recover_interrupted`. Qui vive anche `AudioActivity` (clock di "ultimo frame udibile" per l'auto‑stop) e lo `spawn_pcm_writer` (scrive PCM + calcola l'RMS della traccia di sistema).
- **`managers/diarization.rs`** — `align` / `label_segments` / `merge_segments` / `drop_bleed` + `DiarizationManager`. Logica pura, unit‑testata.
- **`managers/auto_capture.rs`** — 🧪 cervello auto‑capture (macchina a stati pura `AutoCaptureDecider` + 6 test) e `run_supervisor` (I/O shell con probation/cooldown). **Sperimentale, off** (vedi §4).
- **`managers/history.rs`** — SQLite, migrazioni (#5 diarization, #6 status), `write_segments` (doppio namespace speaker), `fail_stale_transcribing`.
- **`managers/transcription.rs`** — `transcribe_with_segments`. **Il modello deve essere residente al `finalize`** (vedi §8).
- **`audio_toolkit/audio/system_audio.rs`** — tap CoreAudio Process Tap (macOS 14.4+), tutto l'`unsafe` confinato. `with_chunk_sink`/`with_level_callback`.
- **`audio_toolkit/audio/recorder.rs`** — `AudioRecorder` (mic, cpal), `with_chunk_sink` (tap fedele un‑VAD‑gated).
- **`audio_toolkit/audio/output_sensor.rs`** — 🧪 sensore tap‑free "casse attive?" (`kAudioDevicePropertyDeviceIsRunningSomewhere`). ⚠️ **Inaffidabile in‑app** (vedi §4).
- **`audio_toolkit/vad/silero.rs`** — Silero VAD (`SileroVad::push_frame` → `VadFrame::Speech|Noise`, 480 campioni @ 16 kHz). **È il segnale affidabile per il pivot del trigger** (§4/§9).
- **`tray.rs`** — la graffetta + l'**orecchio** (`TrayIconState::{Idle,Recording,Transcribing,Listening}`, `change_tray_icon`, `update_tray_menu`). Icona renderizzata come **template** (`set_icon_as_template(true)`) → si adatta a barra chiara/scura.
- **`lib.rs`** — setup, manager init, listener `SessionStateChanged` (→ icona orecchio), spawn del supervisor auto‑capture, CLI flags, self‑heal all'avvio.
- **`settings.rs`** — `AppSettings` (+ `auto_capture_enabled`). Pattern: campo `#[serde(default="default_x")]` + `fn default_x()`. ⚠️ **C'è un costruttore esplicito** in `get_default_settings()` (~riga 776) che elenca i campi: se aggiungi un campo, aggiorna anche lì.

### Frontend (`handy/src/`)
- **`components/settings/sessions/SessionsSettings.tsx`** — pannello "Sessioni" (hero record + selettore sorgente + timer live).
- **`components/settings/history/HistorySettings.tsx`** — Cronologia a card (vedi §6).
- **`bindings.ts`** — tipi auto‑generati (tauri‑specta) — **rigenerato a ogni `bun tauri dev`**, non editarlo a mano.
- **`i18n/locales/en|it/translation.json`** — stringhe. **i18n è build‑blocking** (ESLint vieta stringhe JSX hardcoded; aggiungi le chiavi a `en` (sorgente), gli altri locale fanno fallback).

### MCP (`handy/mcp/`)
- `db.ts` / `server.ts` — server dependency‑free (Bun + `bun:sqlite`), **read‑only**, stdio. Tool: `list_sessions` / `get_session` / `search_sessions`. Registrato in `.mcp.json` (root).

---

## 4. La saga auto‑capture (leggi PRIMA di toccarlo — ti risparmia una giornata)

**Obiettivo:** far partire/fermare la registrazione **da sola** quando esce audio dalle casse (call/video/meeting), con un segnale onesto sempre visibile (l'orecchio). Posture di privacy (decisa, vedi memory + HANDOFF §12): trigger su **audio di sistema**, mic solo in contesto meeting, **niente auto‑registrazione del mic nudo**.

**Cosa abbiamo costruito** (tutto presente, gira solo se `auto_capture_enabled=true`):
1. **Sensore tap‑free** (`output_sensor.rs`): legge `kAudioDevicePropertyDeviceIsRunningSomewhere` sul device di output. **Validato da un processo esterno**: `false` in silenzio, `true` quando suona — niente tap, niente indicatore di registrazione.
2. **Cervello** (`AutoCaptureDecider`): macchina a stati pura con debounce (start dopo ~1.2s di audio, stop dopo ~4s di silenzio). **6 unit test.**
3. **Supervisor** (`run_supervisor`): thread che campiona, pilota `SessionManager`, con **probation** (se nei primi ~2s non cattura audio reale → `SessionManager::cancel()` scarta la sessione, **nessuna riga in Cronologia**) e **cooldown** post‑sessione.
4. **Stop tap‑immune**: lo stop NON usa il sensore device (vedi sotto) ma l'**RMS dell'audio di sistema catturato** (`AudioActivity` aggiornato dallo `spawn_pcm_writer`).

**Il muro (perché è accantonato):** il sensore device, **affidabile da fuori, è cronicamente `true` DENTRO la nostra app**. Una volta aperto un Process Tap, macOS riporta il device di output come perpetuamente "running" per il nostro processo. Validato live: con app ferma e nessun audio, **17 avvii su 17 erano falsi** (la probation li scartava correttamente, zero righe‑spazzatura, ma il mic si apriva ~2s ogni ~11s → inaccettabile).

**Verità di fondo (molto Apple):** macOS rende l'ascolto‑sempre‑attivo **visibile per design** (indicatore arancione mic/registrazione). Un auto‑trigger *davvero invisibile* combatte il modello di privacy della piattaforma, sia via audio‑di‑sistema sia via mic.

**Cosa NON rifare:** non insistere col sensore `DeviceIsRunningSomewhere` come gate in‑app — è un vicolo cieco dentro al processo che possiede (o ha posseduto) un tap.

**Il pivot consigliato (§9):** trigger su **mic‑VAD** (`audio_toolkit/vad/silero.rs`) = "parte quando parli tu". È un segnale **affidabile in‑app** ed era uno dei trigger richiesti. Trade‑off onesto: auto‑registra la tua voce e tiene il mic in ascolto (→ indicatore mic acceso). Riusa quasi tutto l'engine già scritto (cervello, probation, cancel, stop). In alternativa: restare sul **manuale** (graffetta a un click), che è affidabile e onesto.

---

## 5. L'orecchio (menu‑bar)

Quando una sessione registra, l'icona Handy in barra diventa un **orecchio** 👂 (`TrayIconState::Listening` → `resources/tray_listening.png`, generata dall'**SF Symbol di sistema "ear"**, immagine **template** → auto chiaro/scuro). Il dettato push‑to‑talk mantiene il **puntino** (`Recording`). Instradato dal listener `SessionStateChanged` in `lib.rs` (un solo source of truth: tray, CLI, pannello → tutti passano di lì).

Per rigenerare l'asset (se cambi forma): lo script Swift usato sta in scratchpad; in sostanza renderizza `NSImage(systemSymbolName:"ear")` in un PNG 64×64 con alpha. È un nostro asset raster, zero dipendenze a runtime.

---

## 6. La Cronologia a card (`HistorySettings.tsx`)

Ogni riga è una **card di sessione**, non un dump:
- **Icona‑sorgente** inferita dai label speaker — `inferSource()`: `Me`+`Speaker N`→meeting (👥), solo `Me`→mic (🎤), solo `Speaker`→sistema (🔊), niente segmenti→dettatura (📄). ⚠️ `"Me"` è il label letterale scritto da `finalize_session` (managers/history.rs): se lo cambi lì, cambia anche in `inferSource`.
- **Titolo** = `deriveTitle()`: prime ~8 parole del transcript (placeholder **non‑AI**; il titolo AI vero è il next‑next, gated §12). Ignora `entry.title` perché upstream ci mette il timestamp.
- **Meta**: data localizzata · durata (da `end_ms` dei segmenti) · sorgente.
- **Corpo collassabile** (chevron): chiuso = solo header+chip (+stati "trascrizione/fallita/nessun parlato"); aperto = `SpeakerTimeline` + `AudioPlayer` + azioni (copia/ri‑trascrivi/elimina). La ⭐ è sempre in header.
- `TranscriptBody` è un sub‑componente unico così collassato/espanso non divergono.

---

## 7. Data model + MCP

`history.db` (SQLite, `rusqlite_migration`, **append‑only** — non editare una migrazione spedita):
```
transcription_history(id, file_name, timestamp, saved, title, transcription_text,
  post_processed_text, post_process_prompt, post_process_requested,
  status TEXT DEFAULT 'done'  -- 'transcribing'|'done'|'failed')
speakers(id, history_id→ ON DELETE CASCADE, label, embedding)   -- "Me" o "Speaker N"
transcription_segments(id, history_id→, speaker_id→ ON DELETE SET NULL, start_ms, end_ms, text, confidence)
```
`transcription_text` è il transcript canonico; `speakers`+`segments` sono l'overlay attribuito. `ON DELETE CASCADE` richiede `PRAGMA foreign_keys=ON` per connessione.

MCP (`handy/mcp/`): read‑only, stdio, ogni query **parametrizzata**. Smoke test in HANDOFF.md §1.4.

---

## 8. Trappole che costano tempo (le abbiamo pagate noi)

1. **Modello ASR freddo = transcript vuoto (§6.3 in HANDOFF).** Diarizzazione+trascrizione girano solo se `is_model_loaded()`. Il modello si scarica sul timer di idle; una sessione finalizzata col modello freddo dà una riga **vuota** ("nessun parlato") anche se l'audio c'era. Tieni `unload_timeout ≠ Immediately`, o "scalda" con un dettato mentre c'è audio. *Molte "righe vuote" misteriose erano questo, non bug.*
2. **Drop del lock PRIMA dell'emit (§6.1).** Il listener `SessionStateChanged` gira **inline** sul thread che emette e ri‑entra nel manager (`change_tray_icon`→`update_tray_menu`→`is_active()`), ri‑lockando il `Mutex` non rientrante. `start_sources` fa `drop(guard)` prima di `emit`. Ripeti il pattern per ogni nuovo evento di manager.
3. **Sensore device cronicamente `true` in‑app** (§4). Non usarlo come gate dentro l'app.
4. **i18n build‑blocking.** Stringhe JSX hardcoded = errore ESLint. Aggiungi a `en/translation.json`; ⚠️ le nostre chiavi nuove sono solo in `en`+`it`, gli altri 18 locale fanno fallback in inglese (debito noto).
5. **Il CLI toggle ha bisogno di un primario vivo.** `handy --toggle-meeting` funziona solo come *seconda* istanza che inoltra a un `bun tauri dev` già attivo; senza primario boota una nuova istanza e ignora il flag.
6. **La cattura "aggancia" l'output di default all'avvio sessione.** Audio su cuffie/Bluetooth o mutato → il tap registra silenzio → transcript vuoto (graceful, non bug).
7. **`get_default_settings()` elenca i campi a mano** — aggiungendo un setting, aggiorna anche lì o non compila.

---

## 9. Prossimi passi (prioritizzati)

| # | Cosa | Dove | Note / stima |
| --- | --- | --- | --- |
| 1 | **Bottone "Abilita diarizzazione"** | `SessionsSettings.tsx` + comandi `downloadDiarizationModels`/`isDiarizationAvailable` (già esistono, mai chiamati dal FE) | S — quick win |
| 2 | **Auto‑trigger su mic‑VAD** (pivot) | `auto_capture.rs` (riusa cervello/probation/cancel) + `silero.rs` + un monitor mic always‑on | M — è la strada per il "seamless" affidabile; trade‑off privacy (mic in ascolto, indicatore acceso). Decidi con Vlad. |
| 3 | **Errori non ingoiati** | FE: ~56 `console.error` senza toast (es. `settingsStore.ts`) | S–M — polish |
| 4 | **i18n: backfill o taglio** | `i18n/locales/*` | S — o backfilli le chiavi o tagli onestamente le lingue |
| 5 | **AI title/summary delle card** | dopo la decisione provider (§12) — l'utente ha indicato **Claude Code / MCP** come provider | M — sblocca il vero JTBD "note AI" |
| 6 | **Firma + notarizzazione `.dmg`** | serve **Apple Developer account + Xcode completo** + secrets; updater è puntato su upstream `cjpais/Handy` → **ri‑puntare** | umano + M |
| 7 | **Test frontend** | nessuno oggi (solo 2 Playwright banali) | M |

---

## 10. Come testare (ricette pronte)

**Test backend puro:**
```bash
cd handy/src-tauri && cargo test --lib            # 98 passed
```

**Riabilitare l'auto‑capture per sperimentare** (è off di default):
```bash
python3 - "$HOME/Library/Application Support/com.uppify.plaudy/settings_store.json" <<'PY'
import json,sys; p=sys.argv[1]; d=json.load(open(p))
d.setdefault("settings",{})["auto_capture_enabled"]=True
json.dump(d,open(p,"w"),indent=2); print("on")
PY
# poi riavvia bun tauri dev (legge lo store all'avvio)
```

**Validare cattura/sessione SENZA voce umana** (usa `say` come "altra parte"):
```bash
say "the quarterly numbers look strong" &   # audio di sistema
# osserva ~/Library/Logs/com.uppify.plaudy/handy.log o lo stdout del dev server
```

**Ispezionare la Cronologia:**
```bash
DB="$HOME/Library/Application Support/com.uppify.plaudy/history.db"
sqlite3 -readonly "$DB" "SELECT id, length(trim(transcription_text)), status FROM transcription_history ORDER BY id DESC LIMIT 10;"
```

**Pulire righe‑spazzatura vuote** (con backup):
```bash
DB="$HOME/Library/Application Support/com.uppify.plaudy/history.db"; cp "$DB" "$DB.bak"
sqlite3 "$DB" "PRAGMA foreign_keys=ON; DELETE FROM transcription_history WHERE length(trim(transcription_text))=0;"
```

> Nota: l'agent non può fare screenshot della webview Tauri — la UI si valida **con gli occhi sull'app**. Tieni `bun tauri dev` aperto e ricarica (Vite HMR ricarica il frontend a ogni salvataggio `.tsx`).

---

## 11. Convenzioni (rispettale)

- **Ponytail attivo** ("lazy senior dev": YAGNI → stdlib → native → dep esistente → una riga → minimo). Mai tagliare validazione, error handling, sicurezza, accessibilità. Marca le scorciatoie intenzionali con un commento `ponytail:` che nomina il tetto + la via d'upgrade.
- **Pratiche agile**: fette verticali sottili, **outside‑in/TDD** (la logica pura — `align`/`merge_segments`/`drop_bleed`/`AutoCaptureDecider` — è testata in isolamento), resta verde, refactor sotto verde, **review avversariale prima di dire "fatto"**.
- **Estendi i manager/pipeline di Handy**; cita il file reale `handy/src-tauri/...` quando proponi modifiche.
- **Commit**: prefissi convenzionali (`feat:`/`fix:`/`docs:`/`refactor:`/`chore:`), messaggio sul *perché*. **Commit/push solo quando Vlad lo chiede.** Trailer `Co-Authored-By:` in coda.

---

## 12. Decisioni aperte (lasciale a Vlad)
- **Auto‑trigger**: pivot su mic‑VAD (con indicatore mic acceso) o restare sul manuale? (vedi §4/§9.2)
- **Provider AI** per title/summary: l'indirizzo è **Claude Code via MCP** — definire l'integrazione.
- **Firma/notarizzazione**: aprire un Apple Developer account?
- **i18n**: backfill di tutte le lingue o taglio a quelle mantenute?

Buon lavoro. In caso di dubbi sul *perché* di una scelta, parti da §4 (auto‑capture) e §8 (trappole) — è lì che abbiamo speso il sangue. — *team Plaude Local, 2026‑06‑23*
