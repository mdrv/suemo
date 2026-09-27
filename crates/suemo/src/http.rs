//! Read-only HTTP API for the replica (proposal §Architecture, decisions.md
//! Q5): `GET /api/events?from&to`, `GET /api/stats?from&to`, `GET
//! /api/stream` (SSE change hints). Writes stay on the socket IPC — the
//! replica serves restored snapshots and must stay read-only.
//!
//! Hand-rolled HTTP/1.1 on purpose: three GET routes, `Connection: close`
//! JSON answers, one held-open SSE stream. No framework, no new deps.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;

use serde_json::json;

use crate::daemon::{Bus, EngineMsg};
use crate::ipc::{Request, Response};

/// Requests are capped at 366 days (proposal §REST).
const MAX_RANGE_MS: i64 = 366 * 24 * 3_600_000;

pub struct HttpOpts {
    pub addr: String,
    /// Shared bearer token (decisions.md Q5/Q6): set via `SUEMO_HTTP_TOKEN`
    /// on the VPS; when present every request must carry
    /// `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

pub(crate) fn serve(
    opts: HttpOpts,
    tx: mpsc::Sender<EngineMsg>,
    bus: Arc<Bus>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(&opts.addr)?;
    log::info!("http api on {} (replica, read-only)", opts.addr);
    let token = opts.token;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (tx, bus, token) = (tx.clone(), bus.clone(), token.clone());
            std::thread::spawn(move || {
                if let Err(err) = handle_conn(stream, tx, bus, token) {
                    log::debug!("http connection ended: {err}");
                }
            });
        }
    });
    Ok(())
}

fn handle_conn(
    stream: TcpStream,
    tx: mpsc::Sender<EngineMsg>,
    bus: Arc<Bus>,
    token: Option<String>,
) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut peer = stream;
    let request_line = match read_request_line(&mut reader) {
        Some(line) => line,
        None => return Ok(()), // empty probe / TLS to an HTTP port
    };
    let headers = read_headers(&mut reader)?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if let Some(token) = &token {
        let ok = headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&format!("authorization: bearer {token}")));
        if !ok {
            return respond_json(&mut peer, 401, &json!({"error": "unauthorized"}));
        }
    }

    match (method, path) {
        ("GET", "/api/events") => {
            let Some((from, to)) = parse_range(query) else {
                return respond_json(
                    &mut peer,
                    400,
                    &json!({"error": "from and to (epoch ms) are required; window ≤ 366 days"}),
                );
            };
            match ask(&tx, Request::Range { from, to }) {
                Some(Response::Events { events }) => {
                    respond_json(&mut peer, 200, &serde_json::to_value(events).unwrap())
                }
                Some(Response::Err { message }) => {
                    respond_json(&mut peer, 500, &json!({"error": message}))
                }
                Some(_) => respond_json(&mut peer, 500, &json!({"error": "unexpected response"})),
                None => respond_json(&mut peer, 502, &json!({"error": "engine is gone"})),
            }
        }
        ("GET", "/api/stats") => {
            let Some((from, to)) = parse_range(query) else {
                return respond_json(
                    &mut peer,
                    400,
                    &json!({"error": "from and to (epoch ms) are required; window ≤ 366 days"}),
                );
            };
            match ask(&tx, Request::Stats { from, to }) {
                Some(Response::Stats { stats }) => {
                    respond_json(&mut peer, 200, &serde_json::to_value(stats).unwrap())
                }
                Some(Response::Err { message }) => {
                    respond_json(&mut peer, 500, &json!({"error": message}))
                }
                Some(_) => respond_json(&mut peer, 500, &json!({"error": "unexpected response"})),
                None => respond_json(&mut peer, 502, &json!({"error": "engine is gone"})),
            }
        }
        ("GET", "/api/stream") => stream_sse(peer, reader, bus),
        ("GET", _) => respond_json(&mut peer, 404, &json!({"error": "not found"})),
        _ => respond_json(&mut peer, 405, &json!({"error": "method not allowed"})),
    }
}

/// SSE: an initial `retry:` hint, then one `data:` frame per change; the
/// client refetches (decisions.md Q5: hints, not data).
fn stream_sse(
    mut peer: TcpStream,
    mut reader: BufReader<TcpStream>,
    bus: Arc<Bus>,
) -> std::io::Result<()> {
    let head = "\
        HTTP/1.1 200 OK\r\n\
        content-type: text/event-stream\r\n\
        cache-control: no-cache\r\n\
        connection: close\r\n\
        \r\n";
    peer.write_all(head.as_bytes())?;
    peer.write_all(b"retry: 3000\n\n")?;
    peer.flush()?;
    let rx = bus.subscribe();
    // Drain the client quietly; a disconnect shows up as a write error.
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });
    for lsn in rx {
        peer.write_all(format!("data: {{\"changed\":{lsn}}}\n\n").as_bytes())?;
        peer.flush()?;
    }
    Ok(())
}

/// Round-trip a request to the engine thread; `None` if it is gone.
fn ask(tx: &mpsc::Sender<EngineMsg>, request: Request) -> Option<Response> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    tx.send(EngineMsg::Serve {
        request,
        reply: reply_tx,
    })
    .ok()?;
    reply_rx
        .recv()
        .ok()
        .and_then(|r| r.ok())
        .map(|reply| reply.response)
}

fn respond_json(
    stream: &mut TcpStream,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let body = serde_json::to_string(body).expect("json serializes");
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\
         \r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn read_request_line(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let trimmed = line.trim_end().to_string();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed)
}

fn read_headers(reader: &mut BufReader<TcpStream>) -> std::io::Result<Vec<String>> {
    let mut headers = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || headers.len() > 128 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        headers.push(trimmed.to_ascii_lowercase());
    }
    Ok(headers)
}

/// `from=<ms>&to=<ms>`, both required, `from < to`, window ≤ 366 days.
fn parse_range(query: &str) -> Option<(i64, i64)> {
    let mut from = None;
    let mut to = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("from", v)) => from = v.parse().ok(),
            Some(("to", v)) => to = v.parse().ok(),
            _ => {}
        }
    }
    let (from, to) = (from?, to?);
    if from >= to || to - from > MAX_RANGE_MS {
        return None;
    }
    Some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_query_validation() {
        assert!(
            parse_range("from=100&to=100").is_none(),
            "from >= to rejected"
        );
        assert!(
            parse_range("from=200&to=100").is_none(),
            "from > to rejected"
        );
        assert!(parse_range("from=100").is_none(), "missing to");
        assert!(parse_range("to=100").is_none(), "missing from");
        assert!(parse_range("from=x&to=200").is_none(), "non-numeric");
        let big = MAX_RANGE_MS + 1;
        assert!(parse_range(&format!("from=0&to={big}")).is_none(), ">366d");
        assert_eq!(parse_range("from=10&to=200&junk=1"), Some((10, 200)));
    }

    #[test]
    fn header_match_is_exact_after_normalizing_case() {
        let headers = vec!["HOST: x".to_string(), "Authorization: Bearer s3cret".into()];
        let ok = headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("authorization: bearer s3cret"));
        assert!(ok);
        let miss = headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("authorization: bearer wrong"));
        assert!(!miss);
    }
}
