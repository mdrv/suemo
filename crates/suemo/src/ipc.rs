//! Local IPC: unix socket, newline-delimited JSON (proposal §Architecture).
//! CLI and GUI are clients; the daemon is the single writer.
//!
//! The message types are portable; the transport is a unix socket. The
//! Windows CI job is a compile check only (proposal §Platforms), so
//! non-unix targets get stubs that fail at runtime, not at compile time.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::Event;

#[cfg(unix)]
#[path = "ipc_transport_unix.rs"]
mod unix_transport;
pub use unix_transport::{
    connect, ensure_daemon, gui_running, gui_socket_path, gui_stop, open_watch, read_changed,
    read_response, round_trip, spawn_daemon, spawn_gui_detached, write_request,
};

#[cfg(not(unix))]
mod unix_transport {
    //! Non-unix stubs: same names, runtime errors (compile-check only).
    use anyhow::{Result, anyhow};

    pub fn connect() -> std::io::Result<std::fs::File> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "suemo IPC requires a unix socket",
        ))
    }
    pub fn ensure_daemon() -> Result<std::fs::File> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
    pub fn spawn_daemon() -> Result<()> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
    pub fn write_request(_stream: &mut std::fs::File, _request: &Request) -> Result<()> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
    pub fn round_trip(_request: &Request) -> Result<Response> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
    pub fn open_watch() -> Result<std::fs::File> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
    pub fn read_changed(_reader: &mut std::fs::File) -> Result<Option<i64>> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
    pub fn gui_socket_path() -> PathBuf {
        socket_path().with_file_name("gui.sock")
    }
    pub fn gui_running() -> bool {
        false
    }
    pub fn gui_stop() {}
    pub fn spawn_gui_detached() -> Result<()> {
        Err(anyhow!("suemo IPC requires a unix socket"))
    }
}

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
