# HANDOFF — Landing page Vercel (sessione 2026-07-05, pomeriggio)

> Per il prossimo agent. Contesto generale del progetto: `docs/HANDOFF.md` (entry point) e
> `CLAUDE.md`. Questo file copre solo il lavoro landing/Vercel di questa sessione.

## 1. Cosa esiste ora (stato verificato)

- **Landing live: https://plaudy.vercel.app** — pagina statica singola, **zero JavaScript**,
  stile Chris Do / The Futur (hero nero con Helvetica gigante, strip statement rossa,
  sezione comparativa dark, footer statement). Sorgente: `site/index.html` (418 righe,
  CSS inline, nessuna dipendenza).
- **Vercel ↔ GitHub agganciati**: il progetto Vercel `plaudy` è git-connected a
  `github.com/uppifyagency/plaudy`; **ogni push su `main` deploya da solo**
  (verificato E2E due volte in questa sessione: push → deployment production automatico).
- **Validazione finale**: Lighthouse mobile **100/100/100/100**
  (SEO · Accessibility · Best Practices · Agentic Browsing), 50/50 audit passed,
  eseguito sulla pagina live dopo l'ultima revisione.

### File in `site/`
| File | Ruolo |
| --- | --- |
| `index.html` | tutta la pagina: head SEO + JSON-LD + CSS + markup |
| `og.png` | social card 1200×630 (come rigenerarla: §4) |
| `robots.txt` | allow-all + puntatore sitemap |
| `sitemap.xml` | singolo URL, `lastmod` 2026-07-05 |
| `vercel.json` | `cleanUrls` + security headers + cache su og.png |

### Commit di questa sessione (in ordine)
| Hash | Contenuto |
| --- | --- |
| `e942691` | prima versione landing + infra Vercel |
| `77e5517` | `.gitignore`: `.vercel` + `.env*` (aggiunti da `vercel link`) |
| `00926d4` | fix contrasto WCAG AA (`--red-ink`, grigi label) |
| `d9df60a` | **de-slop pass con impeccable** (emoji/em-dash/eyebrow/stat-tile rimossi) + og.png rigenerata |

## 2. Infrastruttura Vercel — come è cablata (e i gotcha)

- Account CLI **già autenticato su questo Mac**: `~/Library/Application Support/com.vercel.cli/auth.json`
  (utente `emailvladvrinceanu-4380`). Usa `bunx vercel …` (CLI non installata globalmente);
  serve `export PATH="$HOME/.bun/bin:$PATH"`.
- Progetto: `plaudy` (`.vercel/project.json` ha projectId/orgId; è gitignorato).
- **`rootDirectory = site`** — impostato via REST API perché la CLI non lo espone:
  `PATCH https://api.vercel.com/v9/projects/<projectId>?teamId=<orgId>` con body
  `{"rootDirectory":"site","framework":null}` e bearer token da `auth.json`.
- **Gotcha 10MB**: il repo supera il limite upload della CLI (modelli ONNX in `handy/`).
  Risolto con `.vercelignore` alla root (`/*` + `!/site`). Non rimuoverlo, o
  `bunx vercel deploy` torna a fallire con "Request body too large".
  I deploy git-triggered non hanno questo problema.
- Deploy manuale (di rado necessario, il push basta): `bunx vercel deploy --prod --yes`
  dalla root del repo.

## 3. Come è stata costruita la pagina (le due skill, dove trovarle)

### Skill 1 — `seo-2026-sota` (SEO/GEO stato dell'arte)
Path: `…/PROGETTI ANTYGRAVITY/Book to Skill/.claude/skills/seo-2026-sota/` (SKILL.md,
cheatsheet.md, patterns.md, 20 capitoli). Scelte applicate, da NON regredire editando:

- **Zero JS**: gli AI crawler non eseguono JavaScript; la pagina è HTML puro → 100%
  citabile da LLM/AIO. INP di fatto perfetto (INP è il collo di bottiglia CWV 2026).
- **Font di sistema** (Helvetica Neue): zero download → LCP testuale istantaneo, zero CLS.
- **Heading-as-question + risposta atomica**: ogni H2 è una domanda reale, la prima frase
  sotto è una risposta autonoma di 1-2 righe. È il pattern che AI Overviews e LLM estraggono.
- **Formato BoFu "X vs Y"**: la tabella "Plaudy vs Plaud/Otter/Fireflies" è il formato che
  gli LLM leggono per raccomandare prodotti.
- **JSON-LD**: `SoftwareApplication` (price 0, MIT, macOS 14.4+) + `FAQPage`.
  ⚠️ **Le risposte FAQ visibili e quelle nel JSON-LD devono restare sincronizzate**:
  se editi una, edita l'altra.
- **Title 52 char** keyword-left, **meta description ~155 char**, canonical
  auto-referenziale, OG/Twitter card.
- E-E-A-T: sezione "Is it real, or another demo?" = "come abbiamo testato" con numeri
  verificabili **come testo** (gli LLM non fanno OCR delle immagini).

### Skill 2 — `impeccable` (anti AI-slop)
Path: `…/PROGETTI ANTYGRAVITY/impeccable/skill/` (SKILL.src.md + reference/brand.md).
Il **detector** è il tool chiave, da rilanciare dopo ogni modifica al copy/CSS:

```bash
cd "…/PROGETTI ANTYGRAVITY/impeccable/skill" && node scripts/detect.mjs --json "<path>/site/index.html"
```

Ban applicati (rispettali quando editi, il detector li ri-becca):
- **niente emoji** (nemmeno nella favicon — ora è un monogramma SVG "P·" — né nella og.png);
- **niente em/en dash** nel copy (0 attuali; usa virgole/due punti/punti);
- **niente eyebrow** (label maiuscole tracciate sopra i titoli) **né marker numerati 01/02/03**;
- **niente stat-tile** "numerone+etichetta" (hero-metric template) → prosa/lista;
- **niente griglie di card identiche** → righe asimmetriche con filetti;
- **cadenza aforistica max 2 momenti deliberati** (strip rossa + footer): non aggiungerne;
- hero ≤ **6rem**, letter-spacing ≥ −0.04em, line-height ≥ 1.3, all-caps solo su micro-label.

Falsi positivi noti del detector (ignorali, già verificati a mano): `cramped-padding`
(il padding orizzontale arriva dal `.wrap`, l'analisi statica non lo risolve),
`single-font` (Helvetica unica = voce deliberata del brand, ammessa dal register brand),
`flat-type-hierarchy` (legge solo i px fissi, non i `clamp()` dei display).

### Token colore (contrasti calcolati, non cambiarli a occhio)
- `--red: #e8340c` — SOLO per testo grande su nero (h1 em, footer em, selection).
- `--red-ink: #d42e0a` — bottoni con testo bianco (5.01:1) e testo rosso piccolo su carta
  (4.59:1), sfondo della strip.
- `--grey: #62625e` su carta (5.62:1); sul nero usare `#8f8e8a` (6.04:1).

## 4. Rigenerare la og.png (procedura)

1. Sorgente: `og.html` nello scratchpad di sessione (ricreala: body 1200×630, bg `#0a0a0a`,
   headline 92px Helvetica bold, brand "Plaudy." con punto rosso, **niente emoji**).
2. Aprila in Chrome (chrome-devtools MCP: `new_page` su `file://…`), `resize_page` 1200×630,
   `take_screenshot` con `filePath` → `site/og.png` (esce @2x su retina).
3. `sips -z 630 1200 site/og.png` per riportarla a 1200×630 esatti. Commit+push.

## 5. Cosa rimane da fare (in ordine di valore)

1. **Dominio custom** (es. `plaudy.app`). Al cambio, aggiornare IN BLOCCO: `<link canonical>`,
   `og:url`, `og:image`/`twitter:image`, `sitemap.xml`, `robots.txt`, e i JSON-LD (`url`).
   Oggi tutto punta a `plaudy.vercel.app` — la coerenza canonical/alias è ciò che tiene SEO 100.
2. **Google Search Console**: verificare la proprietà, inviare la sitemap, "Richiedi
   indicizzazione" sull'URL. Senza GSC non c'è feedback loop (è l'unico canale misurabile
   per AIO/Gemini secondo la skill SEO, ch05/ch14).
3. **IndexNow** per Bing/Copilot/Perplexity (la skill: complemento, non sostituto di GSC).
4. **Distribuzione GEO off-site** (skill ch03/ch06, regola 80/20): il repo GitHub è già la
   leva giusta per Claude; mancano Reddit autentico (r/macapps, r/privacy) e in prospettiva
   YouTube (leva #1 per Gemini/AIO). Niente review-farm.
5. **Link alla landing dal README** del repo (oggi il flusso è solo landing→repo, non viceversa).
6. **Badge/link "Website" nel repo GitHub** (About → website = plaudy.vercel.app).
7. Eventuale **versione italiana** → hreflang bidirezionale + `x-default` (skill ch13);
   oggi la pagina è solo EN, va bene così finché non c'è una strategia i18n.

## 6. Vincoli di coerenza copy ↔ prodotto (trappole note)

- La claim **"no copy of your audio anywhere but your own disk"** regge finché non arriva
  il **sync iPhone** (roadmap): quando si lavorerà lì, aggiornare la landing PRIMA di shippare.
- La tabella comparativa dice "based on each product's public docs, July 2026": se Plaud/
  Otter/Meetily cambiano feature, la tabella va rivista (c'è l'invito "Open a PR" apposta).
- I numeri in pagina (106 test: 102 Rust + 4 MCP; trigger ~1.4s) vengono da
  `CLAUDE.md`/`docs/HANDOFF.md`: se i test crescono, aggiornare la sezione "Is it real".
- La pagina promette "notarized .dmg on the roadmap" (FAQ "How much does it cost?"):
  quando il signing arriva, quella FAQ e il Quick start del README cambiano insieme.

## 7. Stato complessivo del progetto (un rigo)

Il prodotto (app macOS) è allo stato descritto in `CLAUDE.md` §Status — Fase 0/1/2 validate
live, auto-capture per-processo unshelvato e validato E2E (vedi `docs/HANDOFF-AUTOCAPTURE.md`),
MCP = via AI ufficiale; il prossimo lavoro prodotto è in `docs/HANDOFF-GIANNI.md`.
Questa sessione ha aggiunto **la faccia pubblica**: repo pubblico + landing SEO-SOTA live
agganciata in CI. Prodotto e vetrina ora evolvono con lo stesso `git push`.
