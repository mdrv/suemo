//! Local IPC: unix socket, newline-delimited JSON (proposal §Architecture).
//! CLI and GUI are clients; the daemon is the single writer.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::domain::Event;

/// Socket under `$XDG_RUNTIME_DIR/suemo/daemon.sock`, mode 0600 (Q8.3).
pub fn socket_path() -> PathBuf {
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::temp_dir(),
    };
    runtime.join("suemo").join("daemon.sock")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Add {
        title: String,
        kind: String,
        starts_utc: i64,
        ends_utc: i64,
    },
    /// Full-row replace (last-write-wins, decisions.md Q7).
    Update {
        event: Event,
    },
    Delete {
        id: String,
    },
    /// Events overlapping `[from, to)`.
    Range {
        from: i64,
        to: i64,
    },
    Stats {
        from: i64,
        to: i64,
    },
    Status,
    /// No reply-cycle: server answers `Ok` once, then streams `Changed`.
    Watch,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "res", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Event { event: Event },
    Events { events: Vec<Event> },
    Stats { stats: Vec<KindHours> },
    Status { status: DaemonStatus },
    Err { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindHours {
    pub kind: String,
    pub hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub events: u64,
    pub db_root: String,
    pub replica: bool,
    pub started_utc: i64,
}

/// Broadcast notification: a hint, not data — clients refetch (Q3/Q5).
/// The daemon serializes it; the GUI deserializes it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Changed {
    pub changed: i64,
}

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
fn spawn_daemon() -> Result<()> {
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
