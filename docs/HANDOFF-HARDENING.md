# HANDOFF — Hardening end-to-end (2026-07-05, sessione "fix ogni aspetto")

Esito di un review end-to-end (4 agent paralleli: core sessioni, strato audio/auto-capture,
frontend+MCP, gap-analysis Meetily) seguito dalla correzione sistematica di **tutti** i finding:
25 bug, 18 refactor, 20 famiglie di test mancanti. Guidato dai framework di *Agile Technical
Practices Distilled* (smells, connascence, seams per la testabilità, TDD sul fix).

**Stato finale verificato (dopo la seconda passata sui residui): 156 test Rust (da 106) +
13 test MCP (da 4), 0 fail; `bun run lint` e `bun run build` frontend verdi; clippy 38→31
warning (i restanti sono pre-esistenti in file non toccati).**

> **Nota seconda passata (stessa data):** il ciclo di vita dei PCM descritto in §1 è stato
> POI evoluto — vedi §3 punti 1-3: i PCM ora sopravvivono all'archive (stream-mix da disco,
> sorgente della trascrizione per-track) e muoiono a finalize riuscita; la recovery raggruppa
> per session-id e applica la regola "WAV esiste → solo cleanup". L'invariante di sicurezza
> è lo stesso (mai cancellare l'unica copia dell'audio su un failure path), l'implementazione
> è quella nuova.

## 1. I fix che cambiano il comportamento (leggere prima di toccare)

### Audio-safety invariant (il fix più importante)
- `handy/src-tauri/src/managers/session.rs` — `archive_tracks()` (nuova): decodifica → allinea
  → mixa → salva WAV → **solo allora** cancella i PCM. Su errore (disco pieno, PCM illeggibile)
  i PCM restano per `recover_interrupted`. Prima: `finalize_session` cancellava i PCM anche su
  `Err` **prima** che il WAV esistesse → perdita permanente dell'audio. Un track illeggibile non
  tiene più in ostaggio l'altro (skip + keep). Effetto collaterale voluto: i PCM spariscono
  prima di `save_pending_entry`, quindi un crash post-row non produce più la **riga duplicata**
  al riavvio. Test: `archive_*` in session.rs (5).
- `spawn_pcm_writer(.., on_fail)`: un writer che muore (disco pieno a metà meeting) invoca la
  callback → `SessionManager::stop()` da un thread fresco → l'audio già flushato si finalizza e
  l'orecchio si spegne, invece di "recording" fantasma per ore. Test: `writer_failure_invokes_on_fail_once`.

### Auto-capture: churn risolto, probation ridefinita
- `managers/auto_capture.rs` — riscritto il supervisor come **`Supervisor::tick(view, now, dt) -> Option<Effect>`
  puro** (Effect: Start/Stop/Cancel + callback `after_start/after_stop/after_cancel`). Il loop
  I/O (`supervise`) raccoglie `TickView` e applica gli effetti. 9 test nuovi.
- **La lezione churn**: un'app di meeting tiene lo stream di output aperto anche con tutti in
  muto. Finché la sessione auto-avviata non ha **mai** sentito audio vero, la presenza resta il
  sensore per-processo esterno (trigger ancora valido); solo dopo il primo audio la loudness
  catturata decide lo STOP. Prima: probation 2s → discard → cooldown → re-trigger ogni ~11s,
  prime parole perse. Test: `muted_meeting_join_does_not_churn`.
- `PROBATION` ora è 60s e fa da **failsafe** contro sensori bugiardi; una sessione che non ha
  mai sentito audio viene **cancellata** (mai finalizzata → niente righe di silenzio). `dt`
  clampato a `MAX_TICK_DT` (500ms): sleep-wake non bypassa più il debounce. `catch_unwind` con
  restart: un panic non uccide più la feature né orfana la sessione (se nostra → `stop()`, che
  preserva l'audio). Cancel fallito → ownership mantenuta e retry (prima: orfana per sempre).
- Resta **opt-in** (`auto_capture_enabled=false`); la validazione real-meeting è ancora il gate.

### Recorder: deadlock e protocollo
- `audio_toolkit/audio/recorder.rs` — `run_consumer` ora serve i comandi **prima** dei sample e
  attende l'audio con `recv_timeout(CMD_POLL=100ms)`: un device morto (mic scollegato) non può
  più affamare Stop/Shutdown → il deadlock di `stop()`/`close()` è chiuso, con regression test
  (`stop_replies_even_when_the_device_is_stalled`). `stop()` ha `STOP_REPLY_TIMEOUT` (5s).
- `FrameSink` enum (Streaming|Dictation) sostituisce la modalità implicita `chunk_sink.is_some()`;
  sink morto → log latched una volta. Costanti nominate: `DRAIN_TIMEOUT` (2s, referenziata anche
  dal commento IOProc in system_audio), `CMD_POLL`, `STOP_REPLY_TIMEOUT`.
- `system_audio.rs`: `read_tap_format` ora valida anche `kAudioFormatFlagIsNonInterleaved`
  (prima un tap non-interleaved avrebbe registrato solo il canale 0 in silenzio); commenti
  falsi corretti (l'IOProc **alloca** sul thread realtime — ring buffer = upgrade path;
  l'handshake EOS **esiste** ed è identico al path cpal).

### Tray onesta
- `tray.rs` — `resolve_tray_state(requested, session_active)`: **Idle richiesto + sessione
  attiva → Listening**. Una dettatura che finisce durante una sessione non spegne più
  l'orecchio (bug di fiducia). Icona e label del menu derivano dallo **stesso snapshot**.
  Zero `expect()` nei path menu/icona: errore → log + si tiene menu/icona precedente. 3 test.

### History/DB
- `get_connection()`: `busy_timeout(5s)` + WAL → niente più `SQLITE_BUSY` istantanei con
  finalize off-thread + scritture concorrenti.
- `TranscriptionStatus::from_db`: unknown → **Failed** (prima Done — mascherava la corruzione e
  nascondeva il retry). `EntrySource::from_db('') → Unknown`.
- Finalize senza modello → **Failed** con transcript vuoto (prima Done: indistinguibile da
  "silenzio registrato" e retry mai offerto; il retry ora ri-trascrive dal WAV).
- Toggle saved atomico (`UPDATE ... SET saved = NOT saved ... RETURNING`); `delete_entry` una
  connessione, row-first via `DELETE ... RETURNING file_name`; la retention cleanup **emette
  `Deleted`** per riga (la lista UI non resta più stantia) e conta le righe, non i file.
- De-async dei 5 metodi rusqlite bloccanti (`search/get_history_entries`, `toggle_saved_status`,
  `get_entry_by_id`, `delete_entry`) + call-site nei command.
- Seams testabili: `get_entries_conn` (query unica, `WHERE (?1 IS NULL OR id < ?1)`),
  `toggle_saved_conn`, `update_transcription_conn`, `entries_beyond_count`, `entries_older_than`.
  `HISTORY_COLUMNS` const al posto di 7 copie della SELECT. 7 test nuovi (paginazione con
  boundary esatto, retention con esenzione saved, cutoff strict, ecc.).

### Diarizzazione
- `drop_bleed`: un segmento mic di **1 parola non è mai eco** (i backchannel veri — "okay",
  "sì" — venivano cancellati dal transcript); l'overlap deve coprire **≥30%** del segmento mic
  (un graze di 1ms non è bleed — l'eco è fisicamente allineato). La soglia resta 2+ parole
  perché il caso live-validato ("great work") era di 2 parole. 4 test nuovi (incluso il
  boundary 7/10 di `is_echo_text`).

### Timeline & sorgente
- **Skew tra track risolto**: `Track.started: Instant` (stampato all'avvio del recorder);
  `CapturedTrack.lead_silence_ms` = delta dal primo track; `archive_tracks` prepende silenzio
  → mix WAV e timestamp ASR (quindi merge + finestra eco di drop_bleed) condividono un clock.
  Il tap CoreAudio apre centinaia di ms dopo il mic: prima erano tutti allineati a sample 0.
- **Migrazione #7**: colonna `source` ('dictation'|'mic'|'system'|'meeting', default ''=legacy).
  `EntrySource` enum in history.rs, persistita in `save_pending_entry(file, source)` /
  `save_entry` (dictation). Il frontend usa `entry.source` e tiene `inferSource()` solo come
  fallback per righe pre-migrazione. Chiude il coupling sulla label magica "Me" lato UI.
- `MIC_LABEL` const (session.rs) al posto della stringa "Me" in 3 punti; `Source::suffix()` /
  `Source::from_pcm_name()` adiacenti con round-trip test (prima: scrittura e parsing del
  suffisso `.mic./.system.` in due funzioni lontane, rename = recovery misclassificata).
- **`session_elapsed_ms` command** (+ `SessionManager::elapsed_ms`, `ActiveSession.started`):
  la vista Sessions montata a metà sessione mostra l'elapsed vero, non 0:00. bindings.ts
  aggiornato a mano nello stesso formato che specta rigenererà al prossimo `bun tauri dev`.

### Session misc
- `toggle_sources` serializzato da `toggle_lock` (TOCTOU tray+CLI chiuso); gli emit di
  `SessionStateChanged` ri-leggono `is_active()` al momento dell'emissione (l'ultimo evento sul
  wire non può più resuscitare una sessione già fermata); `AudioActivity` = un solo mutex con
  poison-self-heal, resettata anche su stop/cancel (il supervisor non legge più il "heard" della
  sessione precedente); `active_guard()` self-healing; `teardown_track()` estrae la danza
  stop/close/join duplicata tra `stop()` e `cancel()`.

### MCP (`handy/mcp/`)
- `num()` clamp (`limit`∈[1,100] — `LIMIT -1` = intero DB nel contesto del client, verificato
  live, ora irraggiungibile); LIKE escape `[\%_]` + `ESCAPE '\'` + query vuota rifiutata;
  apertura DB **lazy** per tool-call (prima: crash all'avvio su macchina fresca senza
  `history.db` → transport morto; ora initialize ok + errore amichevole). `fixture.ts` condiviso;
  `server.test.ts` copre il protocollo JSON-RPC spawnnando il server vero. 4→13 test.

### Frontend (`handy/src/`)
- `HistorySettings.tsx` 737→330 righe: split in `history/` (IconButton, SpeakerTimeline,
  TranscriptBody, ListRow, DetailPane) + hook `useSessionSegments(entry)` che centralizza la
  derivazione duplicata (l'N+1 IPC resta, batching backend = lavoro futuro).
- `AudioPlayer key={entry.id}`: cambiando selezione non suona più la registrazione precedente
  sotto il transcript nuovo; dead code (`src`/`autoPlay`/`initialSrc`) potato.
- Label live da `event.payload.source` (via `--toggle-system-session` non è più "meeting";
  caveat: il payload porta solo il track primario, mic-only da CLI resta indistinguibile da
  meeting finché l'evento non porta la lista track); `catch` con toast su `toggle`; dedup su
  evento "added"; "Yesterday" DST-safe; recovery di delete che non collassa più a pagina 1;
  `formatClock.ts` unico (4 formatter duplicati rimossi, ≥1h ora `1:01:00`); `EntryStatus` +
  predicati al posto delle stringhe raw in 4 punti.

## 2. Trappole nuove / da sapere

- **`LIMIT -1` interno voluto**: `get_entries_conn` usa `LIMIT -1` per "tutto" quando
  `limit=None` — è deliberato e interno (input clampato); non "sistemarlo".
- **bindings.ts**: aggiornato a mano (sessionElapsedMs, EntrySource, HistoryEntry.source) — il
  prossimo `bun tauri dev` lo rigenera; se diverge, vince la rigenerazione.
- **Migrazione #7 append-only** come le precedenti; le righe legacy hanno `source=''` →
  `Unknown` → la UI usa l'inferenza legacy.
- I test consumer (`recorder.rs`) usano canali in-memory a 16kHz (pass-through del resampler);
  `start_clears_any_leftover_buffer` ha uno sleep di 300ms come sync point (i comandi vengono
  serviti prima dei sample in coda).
- 2 test live-acceptance restano `#[ignore]` (`cargo test --lib output_sensor -- --ignored`).
- I ~31 warning clippy rimanenti sono pre-esistenti (settings.rs, overlay.rs, llm_client.rs…);
  `wants_capture` "never used" è usato dai test del decider.

## 3. Cosa resta (in ordine di valore — include la gap-analysis Meetily)

**Residui del review — CHIUSI nella seconda passata (stessa data, sera):**

1. ✅ **RAM in finalize** — `stream_mix_to_wav` mixa i PCM da disco chunk-by-chunk
   (~256 KiB di picco per il mix) e `transcribe_tracks` legge/trascrive/droppa UN track per
   volta. Nuovo ciclo di vita PCM: restano su disco dopo l'archive (sono la sorgente della
   trascrizione per-track) e vengono rimossi solo a finalize riuscita; su errore la recovery
   applica la regola "WAV esiste → solo cleanup" (niente righe duplicate).
2. ✅ **DI completa di finalize** — trait `Transcriber` + `SessionSink` (seam su
   TranscriptionManager/HistoryManager), split in `persist_session` /
   `transcribe_tracks` / `flat_transcript` / `resolve_status` (+`entry_source_for`).
   I 4 test mancanti ora esistono con stub: no-model→Failed, Failed-solo-se-tutto-fallito,
   partial→Done, label_as_me/Meeting-source, più lead-padding osservato dallo stub.
3. ✅ **Recovery dual-crash** — `plan_recovery` puro (testato): raggruppa gli orfani per
   session-id (mic-first), un crash dual torna come UNA sessione col mic etichettato "Me";
   `RecoveryPlan::CleanupArchived` per i PCM il cui WAV esiste già.
4. ✅ **Batching N+1** — `get_session_overviews(ids)` (query unica su segments+speakers,
   `SessionOverview {speakers, duration_ms}`, conn-level testata) + comando + bindings +
   ListRow che consuma l'overview di pagina invece del fetch per-riga.
5. ✅ parziale — `downmix_interleaved` estratto e condiviso tra callback cpal e IOProc
   (2 test); live-test afplay ora poll-until-timeout (5 s). **Deferrals espliciti:** il test
   di emissione `SessionStateChanged` richiede un harness con AppHandle + device-double del
   capture (non onesto a livello unit senza quel seam); il trait `ProcessLister` per le
   churn-path del sensore richiederebbe il mock delle property-read FFI (valore < costo:
   la decisione pura è già testata e i default sono fail-safe); `RecorderCore` resta
   rinviato per Rule of Three — la duplicazione residua tra i due recorder è ora solo il
   lifecycle cmd_tx/worker (downmix e consumer sono già condivisi).

**Feature (gap-analysis Meetily + product):**

6. **Validazione real-meeting dell'auto-capture** (il gate per il default-on). La scena
   "join muto" ora è coperta da test, ma serve la prova live.
7. **Import & re-transcribe di file audio esterni** — il gap #1 vs Meetily; sblocca il caso
   Plaud-import; `recover_interrupted` è già il 90% della plumbing.
8. **Export Markdown** del transcript speaker-attributed (Meetily lo paywalla: gratis =
   differenziazione).
9. Soft limiter in `mix_tracks` (upgrade path nominato nel commento ponytail).
10. Ricerca in-app nella Cronologia; template di summary come MCP prompts.
11. Evento `SessionStateChanged` con lista track (chiude l'ultimo caveat della label live:
    `--toggle-session` mic-only da CLI è oggi indistinguibile da un meeting).

Nota gap-analysis: la "speaker diarization" di Meetily è una feature **Pro pianificata, non
shippata** (README + sito verificati 2026-07-05); la nostra è live-validated. Idem "chat with
meetings" (loro Coming Soon, noi oggi via MCP) e auto-detect (loro roadmap, noi costruito).
Da NON adottare: provider-abstraction/Ollama sidecar (respinto, §12 HANDOFF), Win/Linux,
streaming real-time, tabella summary persistita.
