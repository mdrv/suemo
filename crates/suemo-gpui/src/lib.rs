//! GPUI front-end: a summoned full-screen layer-shell overlay (grill round
//! 5). `suemo toggle` spawns this process detached (hidden `overlay` verb)
//! or stops it through the `gui.sock` control socket. While shown it takes
//! the keyboard (Exclusive; Esc hides), sits on Layer::Overlay, and lets
//! the desktop ghost through a translucent backdrop. Live updates arrive
//! over the daemon socket (decisions.md Q3); the engine daemon is separate
//! and stays up when the overlay hides.

mod editor;
mod schedule;
mod theme;

use anyhow::Result;
#[cfg(unix)]
use anyhow::{Context as _, bail};
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedSender},
};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;

use gpui::{
    App, AppContext, Bounds, Focusable as _, KeyBinding, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions,
    point, px, size,
};
#[cfg(unix)]
use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
use gpui_platform::application;
use suemo::{config, ipc};

use crate::schedule::{Quit, ScheduleView};

/// Single-overlay lock (decisions.md Q8): binding `gui.sock` fails while an
/// overlay process is alive; a leftover file from a crash is probed and
/// removed. Doubles as the `suemo toggle` control socket. (Unix only —
/// Windows is a compile-check target, proposal §Platforms.)
#[cfg(unix)]
fn gui_lock() -> Result<UnixListener> {
    let path = ipc::gui_socket_path();
    let dir = path
        .parent()
        .context("gui socket has no parent dir")?
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    if path.exists() {
        if ipc::gui_running() {
            bail!("suemo overlay is already running");
        }
        let _ = std::fs::remove_file(&path); // stale socket from a crash
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Control-socket thread: one line per connection; `stop` quits the
/// overlay (§27 pattern — the UI side only sees a channel message).
#[cfg(unix)]
fn spawn_control_thread(listener: UnixListener, stop_tx: UnboundedSender<()>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut line = String::new();
            let mut reader = BufReader::new(stream);
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => continue,
                Ok(_) if line.trim() == "stop" => {
                    let _ = stop_tx.unbounded_send(());
                    break;
                }
                Ok(_) => {}
            }
        }
    });
}

pub fn run() -> Result<()> {
    // Lock + control socket are unix-only; elsewhere the channel simply
    // never fires (Windows is a compile-check target, proposal §Platforms).
    #[cfg(unix)]
    let mut stop_rx = {
        let listener = gui_lock()?;
        let (stop_tx, rx) = mpsc::unbounded::<()>();
        spawn_control_thread(listener, stop_tx);
        rx
    };
    #[cfg(not(unix))]
    let mut stop_rx = mpsc::unbounded::<()>().1;

    // Surface config errors before any window exists.
    let _config = config::load()?;
    // The overlay is a plain socket client; it auto-starts the daemon like
    // any other verb (decisions.md Q8).
    ipc::ensure_daemon()?;

    application().run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("escape", Quit, None)]);
        // Control socket → quit, without blocking the UI thread (§16.1).
        cx.spawn(async move |cx| {
            if stop_rx.next().await.is_some() {
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
        // The overlay is a Wayland layer-shell surface (fork patch):
        // top-level, all anchors, keyboard-exclusive. Windows is a
        // compile-check target (proposal §Platforms) and the fork's
        // layer-shell window kind is Wayland-only.
        #[cfg(unix)]
        let kind = WindowKind::LayerShell(LayerShellOptions {
            namespace: "suemo".into(),
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            exclusive_zone: Some(px(-1.)),
            exclusive_edge: None,
            margin: None,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
        });
        #[cfg(not(unix))]
        let kind = WindowKind::Normal;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(1920.), px(1080.)),
                })),
                focus: true,
                show: true,
                kind,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |window, cx| {
                let entity = cx.new(ScheduleView::new);
                window.focus(&entity.read(cx).focus_handle(cx), cx);
                entity
            },
        )
        .expect("opening the schedule overlay");
        // Esc while the editor is open cancels the editor first (handled by
        // the view's root on_action); a second Esc hides the overlay.
        cx.on_action(|_: &Quit, cx| cx.quit());
    });
    Ok(())
}
