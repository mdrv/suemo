# suemo v0.1.0 — kickoff prompt

You are implementing **suemo v0.1.0** in this repo, from scratch.

Read, in order, before writing any code:

1. `proposal.md` — the decided design. It is the contract; deviate only after
   asking. OPEN items there are questions for the owner.
2. `/x/m/v270/mdrv-db/01-architecture.md` + `02-engine-api.md` — the engine
   you embed (Rust crate, `TursoPort` behind the `turso` feature; SQL-only
   app surface; one `execute` = one LSN = one tx).
3. `/x/m/v270/gpui-ce/gpui-ce.md` — the GPUI fork's practical docs (§6
   layer-shell, §19 z-order, §28 animation-freeze). skimming §16 (IPC/wake,
   re-entrancy) is mandatory before daemon/GUI wiring.
4. `/g/gpui-ce/MDRV.md` — fork consumption rules (git-tag deps, package
   renames `mdrv-gpui-ce` / `mdrv-gpui-platform`, patch inventory).
5. `/g/alarm/AGENTS.md` — read before M4 (PKGBUILD conventions).
6. `/g/mdrv-oc/.github/workflows/release.yml` — the release-pipeline shape
   to adapt (but build in Arch containers, not musl — see proposal).

Build in milestone order (proposal §Milestones): M1 daemon+CLI+engine →
M2 GPUI day view → M3 CRUD+week → M4 sync+release+package. Each milestone
ends with the proposal's verification green and a small conventional
commit. Do not start M(n+1) with M(n) red.

Hard rules:

- **Grill first.** Your first task is a grilling round with the owner on the
  proposal's OPEN items (editing UX, palette, live-update mechanism, REST
  details, caddy ownership, and anything the milestones surface). Ask the
  whole frontier in numbered questions with recommendations; update this
  repo's docs with the answers before building the affected milestone.
- The GPUI fork is consumed from its git tag per `/g/gpui-ce/MDRV.md`
  §Registry (check for the newest `mdrv-gpui-0.0.<ts>` tag at start; upperadd
  currently pins `.4`). NEVER modify `/g/gpui-ce` or `/g/mdrv-db` — hit a
  fork/engine bug? Stop and report it.
- Never touch `/x/db/suemo/live/` by hand; every write through the engine.
  `mdrv-db verify /x/db/suemo` must pass after schema-affecting changes
  (daemon stopped first — fjall lock).
- **No private identifiers in this repo**: no hostnames, domains, IPs, or
  server nicknames in code, config examples, docs, or commits. Everything
  endpoint-ish is env/config-driven. Private deployment specifics live in
  `/x/m/v270/suemo/` (not here).
- v0.1 scope only (proposal §Out of scope): no web app, no Android, no
  recurrence/reminders/notifications, no day-notes, no quick-add overlay.
- Windows is CI-compile-check only; Linux/Wayland is the daily-driver.
- systemd user unit mirrors `~/.config/systemd/user/upperadd.service` in
  shape (`daemon start --foreground`, `Restart=on-failure`,
  `KillMode=process` not needed unless you spawn subprocesses).
- Release assets are named `<name>-<tag>-<rust-target-triple>.tar.gz`
  (owner preference; e.g. `suemo-v0.1.0-aarch64-unknown-linux-gnu.tar.gz` —
  same convention as upperadd's release workflow).
- Conventional commits, small and frequent; commit only with checks green.
  No new dependency without a sentence of justification in the commit body.

First task: `git status` (should be proposal + this file only), then the
grilling round, then scaffold exactly per proposal §Architecture (single
`suemo` bin: `daemon/gui/add/today/week/status/stop`) and get
`cargo check` green with the engine opening `/x/db/suemo` and `suemo add` +
`suemo today` doing a real round trip.
