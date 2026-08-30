# docs/

Index of this folder. Contributors start with `components.md`; porters start
with `PORT.md` at the repo root. Everything else here is a dated historical
record, kept for the reasoning it captures, not maintained against the code.

## Current

- **`components.md`** — module-by-module map of the app for contributors,
  kept in sync with the code.
- **`../PORT.md`** (repo root) — the transfer dossier for porting the music
  stack into `never-off-beat` (nob): module map, locked decisions, paid-for
  gotchas.

## Historical records (dated, not maintained)

- **`hardening-plan.md`** (2026-07-10) — the v0.5 hardening plan and its 12
  locked decisions. Superseded — see `PORT.md`.
- **`audit2-followup.md`** (2026-07-10) — audit #2 follow-up: which of its
  128 findings were fixed on the hardening branch vs deferred. Superseded —
  see `PORT.md`.
- **`audit3-report.md`** (2026-07-16) — audit #3's full narrative findings
  (157 confirmed, 0 critical / 15 high). Superseded — see `PORT.md`.
- **`audit3-fixlist.md`** (2026-07-16) — audit #3's findings as a grouped,
  line-referenced fix-list. Superseded — see `PORT.md`.
- **`ui-plan.md`** (2026-02-03) — a backburner design note for a possible
  desktop tray UI; never built, and unlikely to ship now that spotibot is
  folding into nob (VPS-deployed, Discord-controlled). Not superseded by the
  player-core refactor — just dormant.
