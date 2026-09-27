# suemo

Personal schedule + activity record for one human: one `events` table, a
headless daemon that owns the engine, a CLI, and a summoned full-screen
GPUI overlay. Linux/Wayland is the daily-driver target; Windows is a CI
compile-check only. v0.1 scope: no recurrence, reminders, notifications,
web app, or Android.

## Layout

```
crates/suemo        domain, engine wrapper, socket IPC, HTTP API, daemon, CLI
crates/suemo-gpui   the `suemo` binary + the GPUI overlay front-end
dist/suemo.service  user unit for a dev checkout (packaged unit ships with the PKGBUILD)
```

The binary lives in `suemo-gpui` because cargo forbids the
`suemo(bin) → suemo-gpui → suemo(lib)` cycle; it is still a single
`suemo` executable.

## Usage

```
suemo daemon [--foreground] [--replica]   # owns the engine 24/7
suemo add <title> [HH:MM|now] [+90m|HH:MM] [--kind k]
suemo today            # today's events
suemo week             # week grid + hours per kind
suemo toggle           # show/hide the full-screen overlay
suemo status           # daemon state (does not auto-start)
suemo stop             # stop the daemon
```

Any client verb auto-starts a detached daemon (except `status`/`stop`,
which only probe). The overlay is summoned with `suemo toggle` — bind it
to a key in your compositor. Esc hides it; while shown it takes the
keyboard (Wayland layer-shell, `Layer::Overlay`).

Editing (overlay): click empty space to create a 60-min event, drag for an
arbitrary span, click a block to edit, drag to move, drag the top/bottom
edge to resize. Enter saves, Esc cancels. Snap defaults to 5 minutes —
configure via `~/.config/suemo/config.toml`:

```toml
[ui]
snap_minutes = 5 # 5 | 10 | 15
```

## Storage & sync

Data lives under `SUEMO_DB` (default `/x/db/suemo`), one event row per
commitment or record — no planned/actual duplication. Every write goes
through the embedded engine (one `execute` = one LSN = one transaction,
fsync per write). Never touch `live/` by hand; check it offline with
`mdrv-db verify <data-root>` (daemon stopped).

Sync is one-way, desktop-authoritative (proposal §Sync):

1. the desktop daemon schedules recovery backups into
   `<root>/recovery/<ts>-daemon` — ~10 min after a change, hourly
   regardless, verified;
2. an external push (e.g. rsync) copies new backup dirs to the replica's
   incoming directory (transport lives outside this repo, in the private
   deployment drafts);
3. the replica (`suemo daemon --replica`) watches that directory, stages
   each complete backup (`manifest.json` is written last = completion
   marker), verifies it, swaps it in atomically, and serves reads. The
   replica is read-only by design — writes would be destroyed by the next
   restore.

### HTTP API (replica mode)

Read-only, loopback by default — put it behind a TLS proxy for LAN use.

| Endpoint                            | Notes                                                       |
| ----------------------------------- | ----------------------------------------------------------- |
| `GET /api/events?from=<ms>&to=<ms>` | overlap window, both required, ≤ 366 days, no pagination    |
| `GET /api/stats?from=<ms>&to=<ms>`  | hours per kind, clipped to the window                       |
| `GET /api/stream`                   | SSE; `data: {"changed":<lsn>}` hints — refetch, don't parse |

Environment: `SUEMO_HTTP_ADDR` (default `127.0.0.1:8917`),
`SUEMO_HTTP_TOKEN` (when set, every request needs
`Authorization: Bearer <token>`), `SUEMO_REPLICA_IN` (incoming dir to
watch; unset = serve without restore-watching).

## Development

```
cargo check --workspace   # deps: mdrv-db (engine), mdrv-gpui-ce (GPUI fork, git tag)
cargo test --workspace
cargo build --release
```

Tagging `vX.Y.Z` triggers the release workflow: glibc builds in Arch
containers for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
(asset naming `<name>-<tag>-<rust-target-triple>.tar.gz`), a Windows
compile-check gate, then SHA256SUMS + GitHub Release.

Arch packaging lives in the alarm repo (`packages/suemo`): binary package
installing the release tarball to `/usr/bin` plus the packaged systemd
user unit.
