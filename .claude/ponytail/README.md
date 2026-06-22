# Ponytail — project-local install

This is a **project-scoped** vendored copy of [Ponytail](https://github.com/DietrichGebert/ponytail)
(MIT, by Dietrich Gebert). It is intentionally **NOT installed globally** — it is active only
inside this project folder ("Plaude Local").

## What "lazy senior dev mode" does
Before writing code, the agent stops at the first rung that holds:
1. Does this need to exist? (YAGNI) 2. Stdlib does it? 3. Native platform feature?
4. Installed dependency? 5. One line? 6. Only then the minimum that works.
Validation, error handling, security, accessibility are never cut.

## How it is wired (project-local, not global)
- `hooks/` — the upstream Node lifecycle scripts (copied verbatim from the repo).
- `../settings.json` (project `.claude/settings.json`) registers two hooks pointing at these scripts
  via `$CLAUDE_PROJECT_DIR`, so they fire **only when Claude Code runs from this folder**:
  - `SessionStart` → `ponytail-activate.js` (injects the ruleset, default mode `full`).
  - `UserPromptSubmit` → `ponytail-mode-tracker.js` (handles `/ponytail lite|full|ultra|off`).
- `../commands/ponytail*.md` — the six `/ponytail*` slash commands, also project-local.
- `PONYTAIL_DEFAULT_MODE=full` is set in `../settings.json` `env` (project-scoped).

The only thing it touches outside this folder is a tiny mode-marker file
`~/.claude/.ponytail-active` (one word, used by the optional statusline badge). Harmless and global-by-design.

## Activate
Hooks declared in project settings require a one-time trust prompt. Run `/hooks` in Claude Code,
review the two ponytail hooks, and approve them. They take effect from the next session start.
Toggle intensity any time with `/ponytail lite|full|ultra|off`.

## Upstream
Pinned from `DietrichGebert/ponytail` (plugin v4.7.0). To update, re-copy `hooks/` and the
command prompts from a fresh clone.
