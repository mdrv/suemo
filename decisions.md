# suemo — decisions (grill round 4, 2026-09-27)

Resolves every item in proposal §OPEN. The proposal stays the contract; this
file records the owner's answers. Later rounds append here.

1. **Editing UX** — full direct manipulation. Drag on empty grid = create
   with that span; click empty slot = create a 60-min event; click a block =
   editor (title, kind, start/end, delete); drag block = move; drag block's
   top/bottom edge = resize. Past events behave identically ("correct the
   record"). Escape cancels, Enter commits. **Snap is configurable**:
   `[ui] snap_minutes = 5` (default), `10`, or `15` in
   `~/.config/suemo/config.toml` (validated; anything else is a boot error).
2. **Palette + theme** — dark theme. Kind color = stable hash of the kind
   string into 16 evenly-spaced hue buckets at fixed S=0.5, L=0.6; block
   text via WCAG `contrast_text` (upperadd `src/markdown.rs` approach).
3. **Live updates** — daemon **socket broadcast** (`{"changed":<lsn>}`) to
   watching clients. SSE for the v0.2 web app is served from the same
   internal bus; the GUI takes the socket transport.
4. **Stats** — week-header strip only (colored dot + hours per kind, sorted
   desc) plus the `suemo week` CLI table. More stats wait for real usage.
5. **REST v0.1 = read-only.** `GET /api/events?from=<ms>&to=<ms>` (both
   required, max range 366 days, no pagination); `GET /api/stats?from&to` →
   `{kind, hours}` rows; `GET /api/stream` = SSE hint payloads
   (`{"changed":<lsn>}` → client refetches). Bearer token required in
   `--replica` mode and whenever bound beyond loopback; plain loopback dev
   mode is unauthenticated. Write endpoints arrive with the v0.2 web app.
6. **Caddy + replica** — replica binds `127.0.0.1:8917` by default (env
   `SUEMO_HTTP_ADDR`); caddy terminates TLS and proxies; token check lives
   in the app. We draft the Caddyfile snippet, replica unit, and env-file
   template into `/x/m/v270/suemo/` (private); the owner deploys.
7. **Concurrency** — last-write-wins: edits are full-row updates; the GUI
   reloads the visible range on every broadcast. No version checks in v0.1.
8. **Lifecycle + conventions** — auto-start the daemon (detached
   `process_group(0)` spawn + socket poll) from any client verb; single GUI
   instance (second `suemo gui` prints a notice and exits); socket at
   `$XDG_RUNTIME_DIR/suemo/daemon.sock` (mode 0600); data root env `SUEMO_DB`
   (default `/x/db/suemo`); weeks run Mon–Sun local; midnight-crossing
   events render clipped per day (data untouched); default duration 60 min
   for click-create and `suemo add` without an end.

Also settled by this round:

- **Fork pin**: `mdrv-gpui-0.0.260925.5` (newest tag per PROMPT; impin
  already ships on it).
- **systemd unit**: no `upperadd.service` exists on disk to mirror, so the
  unit is shaped from the kickoff spec alone: `daemon --foreground`,
  `Restart=on-failure`.
- **`suemo add` default kind**: `general` when `--kind` is omitted (a real
  bucket in the stats strip, not an empty string).

## Grill round 5 — 2026-09-27: GUI becomes a summoned fullscreen overlay

Owner direction: "not another window; layer-shell with transparency, full
screen, immersive." Decisions:

1. **Summoned, not ambient** (Q1/Q2/Q3): `suemo toggle` shows a fullscreen
   transparent overlay or hides it; while shown it takes the keyboard
   (`KeyboardInteractivity::Exclusive` — Esc hides, focus returns on hide).
2. **Layer::Overlay** (Q4): visible even over fullscreen apps (§19).
3. **Toggle = process spawn/exit** (Q5 + owner: erase `suemo gui`): the
   fork cannot hide/show or destroy its last window at runtime (§16, §27),
   so the overlay is a short-lived process. `toggle` probes `gui.sock`:
   answering → send `stop` : spawn self detached with the hidden `overlay`
   verb. `gui` is no longer a verb. The engine daemon is untouched and
   stays headless.
4. Layout: centered ~880px column over a translucent backdrop (α 0.92);
   other layouts deferred.

Hyprland bind (owner-side): `bind = <mod>, S, exec, suemo toggle`.
