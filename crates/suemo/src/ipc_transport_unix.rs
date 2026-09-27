//! Unix transport for the socket IPC (see `ipc.rs`): newline-delimited
//! JSON over `UnixStream`, detached spawns via process groups.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};

use super::{Changed, Request, Response, socket_path};

pub fn connect() -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path())
}

/// Connect, spawning a detached daemon first if none is answering (Q8.1).
pub fn ensure_daemon() -> Result<UnixStream> {
    if let Ok(stream) = connect() {
        return Ok(stream);
    }
    spawn_daemon()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(stream) = connect() {
            return Ok(stream);
        }
        ensure!(
            Instant::now() < deadline,
            "daemon did not come up at {} (see daemon.log in that dir)",
            socket_path().display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Detached spawn: own process group, stdio to the runtime-dir log (§27).
pub fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("resolving current exe")?;
    let runtime_dir = socket_path()
        .parent()
        .context("socket has no parent dir")?
        .to_path_buf();
    std::fs::create_dir_all(&runtime_dir)?;
    let log =
        std::fs::File::create(runtime_dir.join("daemon.log")).context("creating daemon.log")?;
    let err = log.try_clone()?;
    Command::new(exe)
        .args(["daemon", "--foreground"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .context("spawning daemon")?;
    Ok(())
}

pub fn write_request(stream: &mut UnixStream, request: &Request) -> Result<()> {
    let line = serde_json::to_string(request)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

pub fn read_response(stream: &mut UnixStream) -> Result<Response> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    ensure!(!line.is_empty(), "daemon closed the connection");
    Ok(serde_json::from_str(line.trim())?)
}

/// One request, one reply — the CLI workhorse.
pub fn round_trip(request: &Request) -> Result<Response> {
    let mut stream = ensure_daemon()?;
    write_request(&mut stream, request)?;
    read_response(&mut stream)
}

/// Subscribe to change broadcasts; the first line is the `Ok` handshake,
/// every later line is a `Changed` hint. (GUI live updates, M2.)
pub fn open_watch() -> Result<BufReader<UnixStream>> {
    let mut stream = ensure_daemon()?;
    write_request(&mut stream, &Request::Watch)?;
    match read_response(&mut stream)? {
        Response::Ok => Ok(BufReader::new(stream)),
        Response::Err { message } => Err(anyhow!("watch rejected: {message}")),
        _ => Err(anyhow!("unexpected reply to watch")),
    }
}

/// Next change hint from a watch stream; `None` on clean EOF (the daemon
/// closes watch streams when it stops).
pub fn read_changed(reader: &mut BufReader<UnixStream>) -> Result<Option<i64>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let changed: Changed = serde_json::from_str(line.trim())?;
    Ok(Some(changed.changed))
}

/// Overlay control socket, sibling of the daemon socket (grill round 5).
pub fn gui_socket_path() -> PathBuf {
    let mut path = socket_path();
    path.set_file_name("gui.sock");
    path
}

/// Is an overlay process answering on the control socket?
pub fn gui_running() -> bool {
    UnixStream::connect(gui_socket_path()).is_ok()
}

/// Ask the overlay to exit, then wait (≤2 s) until it stops answering so a
/// fast re-toggle doesn't race the teardown.
pub fn gui_stop() {
    let Ok(mut stream) = UnixStream::connect(gui_socket_path()) else {
        return;
    };
    let _ = stream.write_all(b"stop\n").and_then(|_| stream.flush());
    let deadline = Instant::now() + Duration::from_secs(2);
    while gui_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Detached overlay spawn (§27): own process group, stdio to gui.log.
pub fn spawn_gui_detached() -> Result<()> {
    let exe = std::env::current_exe().context("resolving current exe")?;
    let runtime_dir = socket_path()
        .parent()
        .context("socket has no parent dir")?
        .to_path_buf();
    std::fs::create_dir_all(&runtime_dir)?;
    let log = std::fs::File::create(runtime_dir.join("gui.log")).context("creating gui.log")?;
    let err = log.try_clone()?;
    Command::new(exe)
        .args(["overlay"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .context("spawning overlay")?;
    Ok(())
}
