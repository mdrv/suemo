//! GPUI front-end for suemo. M2: a full-window app (a Normal window — NOT
//! a layer-shell overlay, proposal §GUI) showing a read-only day view with
//! live updates over the daemon socket (decisions.md Q3, Q8). Editing and
//! the week view land in M3.

mod day_view;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};

use anyhow::{Context as _, Result, bail};
use gpui::{
    App, AppContext, Bounds, Focusable as _, KeyBinding, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use gpui_platform::application;
use suemo::{config, ipc};

use crate::day_view::{DayView, Quit};

/// Single-GUI-instance lock (decisions.md Q8): binding `gui.sock` fails
/// while another GUI is alive; a leftover file from a crash is probed and
/// removed. The listener is held for the lifetime of the process.
fn gui_lock() -> Result<UnixListener> {
    let mut path = ipc::socket_path();
    path.set_file_name("gui.sock");
    let dir = path
        .parent()
        .context("gui socket has no parent dir")?
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("suemo gui is already running");
        }
        let _ = std::fs::remove_file(&path); // stale socket from a crash
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub fn run() -> Result<()> {
    let _gui_lock = gui_lock()?;
    // Surface config errors before any window exists.
    let _config = config::load()?;
    // The GUI is a plain socket client; it auto-starts the daemon like any
    // other verb (decisions.md Q8).
    ipc::ensure_daemon()?;

    application().run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("escape", Quit, None)]);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(80.), px(60.)),
                    size: size(px(960.), px(1180.)),
                })),
                titlebar: Some(TitlebarOptions {
                    title: Some("suemo".into()),
                    ..Default::default()
                }),
                focus: true,
                show: true,
                kind: WindowKind::Normal,
                window_background: WindowBackgroundAppearance::Opaque,
                ..Default::default()
            },
            |window, cx| {
                let entity = cx.new(DayView::new);
                window.focus(&entity.read(cx).focus_handle(cx), cx);
                entity
            },
        )
        .expect("opening the day view window");
        cx.on_action(|_: &Quit, cx| cx.quit());
    });
    Ok(())
}
