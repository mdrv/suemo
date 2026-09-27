//! The daemon owns the engine 24/7 (proposal §Architecture): unix-socket
//! IPC for CLI/GUI clients + a change broadcast bus (M4's SSE shares it).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::domain;
use crate::engine::Db;
use crate::ipc::{self, Changed, DaemonStatus, KindHours, Request, Response};

pub struct DaemonOpts {
    pub replica: bool,
}

pub(crate) enum EngineMsg {
    Serve {
        request: Request,
        reply: mpsc::SyncSender<std::result::Result<Reply, String>>,
    },
    /// Replica only: a staged backup to verify and swap in.
    Restore {
        staging: std::path::PathBuf,
    },
    Stop,
}

/// Engine reply: the per-request response plus the LSN to broadcast.
pub(crate) struct Reply {
    pub(crate) response: Response,
    changed: Option<i64>,
}

pub fn run(opts: DaemonOpts) -> Result<()> {
    let sock = ipc::socket_path();
    let runtime_dir = sock
        .parent()
        .context("socket has no parent dir")?
        .to_path_buf();
    std::fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("creating {}", runtime_dir.display()))?;
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            bail!(
                "another suemo daemon is already running (socket {})",
                sock.display()
            );
        }
        let _ = std::fs::remove_file(&sock); // stale socket from a crash
        log::info!("removed stale socket {}", sock.display());
    }
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    log::info!("listening on {}", sock.display());

    let started_utc = domain::now_ms();
    let bus: Arc<Bus> = Arc::new(Bus::default());
    let (tx, rx) = mpsc::channel::<EngineMsg>();
    let replica = opts.replica;
    if replica {
        let addr =
            std::env::var("SUEMO_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8917".to_string());
        let token = std::env::var("SUEMO_HTTP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let incoming = std::env::var("SUEMO_REPLICA_IN")
            .ok()
            .filter(|s| !s.is_empty());
        crate::http::serve(
            crate::http::HttpOpts { addr, token },
            tx.clone(),
            Arc::clone(&bus),
        )?;
        match incoming {
            Some(dir) => {
                crate::sync::watch_incoming(dir.into(), crate::engine::default_root(), tx.clone())
            }
            None => log::warn!(
                "--replica: SUEMO_REPLICA_IN unset — serving HTTP without restore-watching"
            ),
        }
    }
    let engine_bus = Arc::clone(&bus);
    let engine_thread =
        std::thread::spawn(move || engine_loop(rx, engine_bus, replica, started_utc));
    accept_loop(listener, tx, bus);
    let _ = engine_thread.join();
    Ok(())
}

/// Change broadcast: watchers get `{"changed":<lsn>}` lines (Q3); the
/// replica's SSE stream subscribes to the same bus.
#[derive(Default)]
pub(crate) struct Bus {
    watchers: Mutex<Vec<mpsc::Sender<i64>>>,
}

impl Bus {
    pub(crate) fn subscribe(&self) -> mpsc::Receiver<i64> {
        let (tx, rx) = mpsc::channel();
        self.watchers.lock().unwrap().push(tx);
        rx
    }

    pub(crate) fn publish(&self, lsn: i64) {
        let mut watchers = self.watchers.lock().unwrap();
        watchers.retain(|w| w.send(lsn).is_ok());
    }
}

fn accept_loop(listener: UnixListener, tx: mpsc::Sender<EngineMsg>, bus: Arc<Bus>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let tx = tx.clone();
        let bus = bus.clone();
        std::thread::spawn(move || {
            if let Err(err) = serve_conn(stream, tx, bus) {
                log::debug!("connection ended: {err}");
            }
        });
    }
}

fn serve_conn(
    stream: UnixStream,
    tx: mpsc::Sender<EngineMsg>,
    bus: Arc<Bus>,
) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // client hung up
        }
        let request: Request = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(err) => {
                write_response(
                    &mut writer,
                    &Response::Err {
                        message: format!("bad request: {err}"),
                    },
                )?;
                continue;
            }
        };
        match request {
            Request::Watch => {
                write_response(&mut writer, &Response::Ok)?;
                let rx = bus.subscribe();
                for lsn in rx {
                    if write_line(&mut writer, &Changed { changed: lsn }).is_err() {
                        break;
                    }
                }
                return Ok(());
            }
            Request::Stop => {
                write_response(&mut writer, &Response::Ok)?;
                let _ = tx.send(EngineMsg::Stop);
                // engine_loop drains pending work, closes the engine,
                // removes the socket, and process-exits.
                return Ok(());
            }
            request => {
                let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                if tx
                    .send(EngineMsg::Serve {
                        request,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    write_response(
                        &mut writer,
                        &Response::Err {
                            message: "engine is gone".into(),
                        },
                    )?;
                    return Ok(());
                }
                match reply_rx.recv() {
                    Ok(Ok(reply)) => {
                        if let Some(lsn) = reply.changed {
                            bus.publish(lsn);
                        }
                        write_response(&mut writer, &reply.response)?;
                    }
                    _ => {
                        write_response(
                            &mut writer,
                            &Response::Err {
                                message: "engine did not reply".into(),
                            },
                        )?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn write_response(stream: &mut UnixStream, response: &Response) -> std::io::Result<()> {
    let line = serde_json::to_string(response).expect("response serializes");
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn write_line<T: Serialize>(stream: &mut UnixStream, value: &T) -> std::io::Result<()> {
    let line = serde_json::to_string(value).expect("notification serializes");
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn engine_loop(rx: mpsc::Receiver<EngineMsg>, bus: Arc<Bus>, replica: bool, started_utc: i64) {
    let root = crate::engine::default_root();
    let mut db: Option<Db> = Some(open_or_die(&root));
    log::info!("engine open at {} (replica: {replica})", root.display());
    // Proposal §Sync 1: debounced-on-change + hourly recovery backups.
    let mut backups = crate::sync::BackupState::new(started_utc);
    loop {
        let msg = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(db) = db.as_ref() {
                    backups.tick(domain::now_ms(), db, &root);
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            EngineMsg::Serve { request, reply } => {
                let result = handle(
                    db.as_ref().expect("engine present on serve path"),
                    &request,
                    replica,
                    started_utc,
                );
                if matches!(&result, Ok(reply) if reply.changed.is_some()) {
                    backups.on_change(domain::now_ms());
                }
                let _ = reply.send(result);
            }
            EngineMsg::Restore { staging } => {
                match crate::sync::adopt(db.take(), &root, &staging) {
                    Ok((new_db, applied)) => {
                        db = Some(new_db);
                        bus.publish(applied);
                    }
                    Err(err) => {
                        log::error!("adopting {}: {err:#}", staging.display());
                        db = Some(open_or_die(&root));
                    }
                }
            }
            EngineMsg::Stop => break,
        }
    }
    // The engine must close (persist + release the fjall lock) before
    // `mdrv-db verify` can run — the PROMPT's schema gate.
    drop(db);
    drop(bus);
    let _ = std::fs::remove_file(ipc::socket_path());
    log::info!("engine closed, socket removed, exiting");
    std::process::exit(0);
}

fn open_or_die(root: &std::path::Path) -> Db {
    match Db::open(root) {
        Ok(db) => db,
        Err(err) => {
            log::error!("opening engine at {}: {err:#}", root.display());
            eprintln!(
                "suemo daemon: opening engine at {}: {err:#}",
                root.display()
            );
            std::process::exit(1);
        }
    }
}

fn handle(
    db: &Db,
    request: &Request,
    replica: bool,
    started_utc: i64,
) -> std::result::Result<Reply, String> {
    let err = |message: String| Reply {
        response: Response::Err { message },
        changed: None,
    };
    let ok_changed = |changed: i64| Reply {
        response: Response::Ok,
        changed: Some(changed),
    };
    match request {
        Request::Add {
            title,
            kind,
            starts_utc,
            ends_utc,
        } => match db.add(title, kind, *starts_utc, *ends_utc) {
            Ok((event, lsn)) => Ok(Reply {
                response: Response::Event { event },
                changed: Some(lsn),
            }),
            Err(e) => Ok(err(format!("{e:#}"))),
        },
        Request::Update { event } => match db.update(event) {
            Ok(lsn) => Ok(ok_changed(lsn)),
            Err(e) => Ok(err(format!("{e:#}"))),
        },
        Request::Delete { id } => match db.delete(id) {
            Ok((true, lsn)) => Ok(ok_changed(lsn)),
            Ok((false, _)) => Ok(err(format!("no event with id {id}"))),
            Err(e) => Ok(err(format!("{e:#}"))),
        },
        Request::Range { from, to } => match db.range(*from, *to) {
            Ok(events) => Ok(Reply {
                response: Response::Events { events },
                changed: None,
            }),
            Err(e) => Ok(err(format!("{e:#}"))),
        },
        Request::Stats { from, to } => match db.kind_hours(*from, *to) {
            Ok(stats) => Ok(Reply {
                response: Response::Stats {
                    stats: stats
                        .into_iter()
                        .map(|(kind, hours)| KindHours { kind, hours })
                        .collect(),
                },
                changed: None,
            }),
            Err(e) => Ok(err(format!("{e:#}"))),
        },
        Request::Status => match db.count() {
            Ok(events) => Ok(Reply {
                response: Response::Status {
                    status: DaemonStatus {
                        version: env!("CARGO_PKG_VERSION").into(),
                        events,
                        db_root: crate::engine::default_root().display().to_string(),
                        replica,
                        started_utc,
                    },
                },
                changed: None,
            }),
            Err(e) => Ok(err(format!("{e:#}"))),
        },
        // Both are consumed by the connection handler before the engine.
        Request::Watch | Request::Stop => {
            Ok(err("internal: handled by the connection handler".into()))
        }
    }
}
