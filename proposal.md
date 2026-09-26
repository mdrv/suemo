# suemo — proposal (v0.1.0)

Personal schedule **and** activity record in one: a local-first, hour-based
timeline (Google-Calendar-shaped, minus the bloat) whose past is the record.
Decided with the owner 2026-09-25/26 (grilling rounds 1–3); this document is
the contract. OPEN items are marked — they are questions for the owner, not
decisions to bake in silently. Companion docs:
[/x/m/v270/suemo/](/x/m/v270/suemo/) (private: deployment details, hosts,
domains — the repo must stay free of private identifiers).

## Product

One entity, two readings: an **event** has a start and end; upcoming events
are your commitments, past events are your activity record — and records can
be corrected in place ("planned vs actual" duplication is explicitly
rejected). The focus is **commitment and self-reflection**: get the day's
shape at a glance, log what actually happened, fix it when it drifted.
Freeform kind strings (`workout`, `deep-work`, …) with stable auto-assigned
colors give the reflection layer its texture. **No recurrence, no reminders,
no notifications** — decided, keep them out.

## Architecture

```
suemo.service (systemd --user)          suemo daemon — owns the engine 24/7
  ├─ engine: mdrv-db slug "suemo" (/x/db/suemo, single writer, fjall lock)
  ├─ local IPC: unix socket, newline-delimited JSON (CLI + GUI are clients)
  ├─ REST + SSE (loopback + LAN): /api/events, /api/stats, /api/stream
  └─ sync: scheduled offline backup → push to VPS replica (one-way)

suemo gui (GPUI full-window client)     suemo add/today/week (CLI clients)
VPS: suemo daemon --replica             ingests pushed backups, serves REST+SSE
                                        web app (Svelte) + Android (Capacitor) = v0.2
```

- **Single binary** `suemo` (clap): `daemon [--foreground] [--replica]`,
  `gui`, `add <title> [HH:MM|now] [HH:MM|+90m] [--kind k]`, `today`,
  `week`, `status`, `stop`. No `suemod` — the daemon is a subcommand.
- The daemon owning the engine (not the GUI) is what makes quick-add from a
  terminal work while the GUI is open, and what keeps sync + the VPS replica
  alive when the GUI is closed.
- GUI = full-window app (NOT an overlay). Day view is the landing view; Week
  view second. The upperadd-style layer-shell quick-add popup is a v0.2 idea
  and must NOT shape v0.1 architecture.

## Data model (mdrv-db)

Engine crate `mdrv-db = { version = "0.5.1", features = ["turso"] }`
(`TursoPort::open(live/app.db)` → `Engine::open`; see
/x/m/v270/mdrv-db/02-engine-api.md "Rust embedders"). This is user data, not
derived: `EngineConfig { fsync_each_write: true }` (per-write durability),
unlike upperadd's index.

```sql
CREATE TABLE events (
  id          TEXT PRIMARY KEY,  -- ULID
  starts_utc  INTEGER NOT NULL,  -- epoch ms
  ends_utc    INTEGER NOT NULL,
  title       TEXT NOT NULL,
  kind        TEXT NOT NULL,     -- freeform; color = stable hash → palette
  note        TEXT NOT NULL DEFAULT '',
  idem_key    TEXT UNIQUE,       -- reserved for v0.2 offline capture replay
  created_utc INTEGER NOT NULL,
  updated_utc INTEGER NOT NULL
);
```

- UTC epoch-ms everywhere; **day boundaries are computed at render** in the
  system's local timezone. No per-event timezones.
- Stats = SQL aggregates over `kind` (hours per kind this week).
- Kind taxonomy is freeform strings; no CRUD UI for kinds in v0.1.
- Writes: one `engine.execute` per mutation (one LSN). Reads via
  `engine.query`. Schema via `engine.bootstrap` at daemon boot.
- A `suemo reindex`-style rebuild is NOT required (this DB is the source of
  truth, not a derived index) — but `mdrv-db verify /x/db/suemo` must pass
  and backups are load-bearing (they ARE the sync payload).

## v0.1 scope (views + interactions)

- **Day view** (landing): local-time hour grid, now-line, events as blocks.
- **Week view**: 7-day column grid + header strip with hours-per-kind this
  week (minimal stats).
- **CRUD**: click/drag to create; **inline editing of past events** (fix
  times, title, kind — "correct the record" is a first-class verb);
  delete. Editing UX details = OPEN (successor proposes, owner approves).
- Kind colors: stable hash of the kind string into a fixed accessible
  palette (Hsla, WCAG-checked text contrast — reuse upperadd's
  `contrast_text` approach). Exact palette = OPEN.
- GUI keeps its state across focus changes; live updates arrive via the
  daemon (socket broadcast or loopback SSE — successor's call, OPEN).
- Out of scope v0.1: web app, Android, day-notes, recurrence, reminders,
  quick-add overlay, Windows polish (see Post-v0.1).

## Sync (v0.1: one-way, desktop-authoritative)

1. Desktop daemon schedules backups (mdrv-db admin-RPC/CLI `backup` into
   `<slug>/recovery/<ts>-daemon/`, verify-ok required) — debounced on change
   (≈10 min) + hourly.
2. Push new backup dirs to the VPS (rsync over its existing access).
3. VPS runs `suemo daemon --replica`: watches the incoming dir, restores
   (restore is deliberately CLI-only in mdrv-db — so: restore into a fresh
   dir under the lock, verify, atomic swap), then serves REST+SSE.
4. Web/Android (v0.2) are clients of the replica only. **v0.1 remotes are
   read-only** — writes on the replica would be destroyed by the next
   restore. Bidirectional sync / offline mobile capture = v0.2 project
   (idem_key replay already reserved).

The replica endpoint sits behind caddy with a **single shared bearer token**
(env file on the VPS; same discipline as `MDRV_DB_ADMIN_TOKEN`). URL/domain
and hostnames are private — see /x/m/v270/suemo/, never hardcoded.

## Platforms

| Platform | v0.1                   | Later                                                                                                                                                                                                                                                                            |
| -------- | ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux    | daily-driver (Wayland) | —                                                                                                                                                                                                                                                                                |
| Windows  | CI compile check only  | real polish; fork's `gpui_windows` is real (D3D11+DirectComposition+DirectWrite, native CI gate) but the fork's layer-shell patches (set_margin, keyboard interactivity, sync resize) are Wayland-only no-ops there — an overlay UX needs a Windows design, normal windows don't |
| Web      | contract only (REST)   | Svelte 5 + SvelteKit on the VPS replica                                                                                                                                                                                                                                          |
| Android  | —                      | Capacitor wrap of the web app; offline capture with idem replay                                                                                                                                                                                                                  |

## Release & packaging

- Tag `v*` → GitHub Actions → per-arch tarballs + `SHA256SUMS` → GitHub
  Release. **Asset naming (owner preference, same as upperadd):
  `<name>-<tag>-<rust-target-triple>.tar.gz`** — e.g.
  `suemo-v0.1.0-aarch64-unknown-linux-gnu.tar.gz`,
  `suemo-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`. Template: `/g/mdrv-oc/.github/workflows/release.yml` (jobs:
  build matrix → artifacts → release). **Use the latest `uses:` versions**
  (owner requirement; mdrv-oc's are the floor, not the ceiling).
- **Not musl**: gpui/wayland/wgpu on musl is uncharted; consumers are Arch.
  Build inside Arch containers (`archlinux:base-devel` x86_64,
  `archlinuxarm:base-devel` aarch64 — the /g/alarm pattern) so binaries match
  the repo's glibc.
- **PKGBUILD**: `/g/alarm/packages/suemo/` — binary package installing the
  release tarball (`/usr/bin/suemo` + packaged systemd unit with `/usr/bin`
  paths, NOT `/x/g/...` dev paths; `ua`-style: no symlink needed, the binary
  IS `suemo`). Follow `/g/alarm/AGENTS.md` to the letter (header, both arches
  declared, `update.jsonc` entry, `namcap`, README update). Developer machines
  keep using the repo checkout + `dist/` unit; the packaged unit is separate.

## Milestones

- **M1** — scaffold (`crates/suemo` lib: domain/engine/ipc; bin `suemo`;
  `crates/suemo-gpui` UI lib), daemon owns engine + schema bootstrap, socket
  IPC, CLI verbs (`add/today/week/status/stop`) working against the engine,
  systemd unit, fork-tag deps per /g/gpui-ce/MDRV.md.
- **M2** — GPUI shell + Day view read-only (hour grid, now-line, blocks) +
  live updates. Owner can see their day.
- **M3** — CRUD (create, inline edit incl. past events, delete), kinds +
  auto-colors, Week view + weekly stats strip.
- **M4** — sync push loop, replica mode + REST/SSE + token auth, release
  workflow, alarm PKGBUILD, README. (VPS deploy itself = owner-supervised.)
- **Post-v0.1** (documented in this repo, not built): web app, Android,
  offline capture + idem replay, bidirectional sync, day-notes convention
  (`/m` markdown, editor handoff), quick-add overlay, Windows, recurrence
  (schema-compatible: add columns later), reminders.

Definition of done per milestone = verification green (below) + the owner
exercising the feature for real.

## Verification (green before every commit)

```sh
cargo check && cargo test && cargo build --release
systemctl --user restart suemo && suemo status
suemo add "test" now +15m --kind scratch && suemo today   # round trip
suemo stop && mdrv-db verify /x/db/suemo                  # schema gates
```

Never hand-edit `/x/db/suemo/live/`; every write through the engine.

## OPEN items (grill the owner — do not decide silently)

1. Editing UX in Day/Week views: click-to-create vs drag-to-create first;
   how inline time-editing works (handles? dialog?).
2. Color palette (fixed list? saturation/lightness targets?).
3. GUI live-update mechanism (socket broadcast vs loopback SSE).
4. Stats presentation beyond the week-header strip.
5. REST details beyond the contract (pagination? SSE payloads? PATCH
   semantics for "move by 15 min").
6. Caddy site config on the VPS: who writes it, which ports the replica
   binds (deploy is owner-supervised).
7. Concurrency rules for `add` while GUI holds an editing session.
