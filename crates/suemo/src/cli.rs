//! CLI verbs: daemon, add, today, week, status, stop, toggle. (`overlay`
//! is the hidden GUI-process verb — dispatched by the binary in suemo-gpui.)

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};

use crate::domain::{self, Event};
use crate::ipc::{self, KindHours, Request, Response};

#[derive(Parser)]
#[command(name = "suemo", version, about = "Personal schedule + activity record")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the daemon (owns the engine; systemd + auto-start run this)
    Daemon {
        /// foreground (detaching is the spawner's job; accepted for clarity)
        #[arg(long)]
        foreground: bool,
        /// VPS replica mode: ingest pushed backups, serve REST+SSE (M4)
        #[arg(long)]
        replica: bool,
    },
    /// Show the day-view overlay, or hide it if it is already shown
    Toggle,
    /// Run the overlay process itself (spawned detached by `toggle`)
    #[command(hide = true)]
    Overlay,
    /// Add an event: suemo add "Title" [HH:MM|now] [HH:MM|+90m] [--kind k]
    Add {
        title: String,
        start: Option<String>,
        end: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
    /// List today's events
    Today,
    /// Events per day + hours per kind for the current Mon–Sun week
    Week,
    /// Daemon status
    Status,
    /// Stop the daemon
    Stop,
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Daemon { replica, .. } => crate::daemon::run(crate::daemon::DaemonOpts { replica }),
        // Reached only if a front-end forgets to intercept `overlay`.
        Cmd::Overlay => bail!("suemo overlay is wired by the suemo-gpui front-end"),
        Cmd::Toggle => toggle(),
        Cmd::Add {
            title,
            start,
            end,
            kind,
        } => add(title, start.as_deref(), end.as_deref(), kind.as_deref()),
        Cmd::Today => today(),
        Cmd::Week => week(),
        Cmd::Status => status(),
        Cmd::Stop => stop(),
    }
}

fn add(title: String, start: Option<&str>, end: Option<&str>, kind: Option<&str>) -> Result<()> {
    ensure!(!title.trim().is_empty(), "title must not be empty");
    let starts_utc = match start {
        None | Some("now") => domain::now_ms(),
        Some(spec) => domain::parse_start(spec)?,
    };
    let ends_utc = match end {
        None => starts_utc + domain::DEFAULT_DURATION_MINUTES * 60_000,
        Some(spec) => domain::parse_end(spec, starts_utc)?,
    };
    ensure!(ends_utc > starts_utc, "end must be after start");
    let kind = kind.unwrap_or(domain::DEFAULT_KIND).to_string();
    match ipc::round_trip(&Request::Add {
        title,
        kind,
        starts_utc,
        ends_utc,
    })? {
        Response::Event { event } => {
            println!(
                "added {} {}–{} [{}] {}",
                event.id,
                fmt_local(event.starts_utc),
                fmt_local(event.ends_utc),
                event.kind,
                event.title
            );
            Ok(())
        }
        Response::Err { message } => bail!("{message}"),
        _ => bail!("unexpected reply to add"),
    }
}

fn today() -> Result<()> {
    let (from, to) = domain::today_window();
    let events = request_range(from, to)?;
    if events.is_empty() {
        println!("no events today");
        return Ok(());
    }
    for event in &events {
        println!("{}", event_line(event));
    }
    Ok(())
}

fn week() -> Result<()> {
    let (from, to) = domain::this_week_window();
    let events = request_range(from, to)?;
    let stats = match ipc::round_trip(&Request::Stats { from, to })? {
        Response::Stats { stats } => stats,
        Response::Err { message } => bail!("{message}"),
        _ => bail!("unexpected reply to stats"),
    };

    let mut by_day: BTreeMap<chrono::NaiveDate, Vec<&Event>> = BTreeMap::new();
    for event in &events {
        let day = DateTime::from_timestamp_millis(event.starts_utc)
            .context("bad timestamp")?
            .with_timezone(&Local)
            .date_naive();
        by_day.entry(day).or_default().push(event);
    }
    for (day, day_events) in &by_day {
        println!("{}", day.format("%a %Y-%m-%d"));
        for event in day_events {
            println!("  {}", event_line(event));
        }
    }
    if events.is_empty() {
        println!("no events this week");
    }
    if !stats.is_empty() {
        println!("\nhours per kind this week:");
        for KindHours { kind, hours } in &stats {
            println!("  {kind:<12} {hours:.1}");
        }
    }
    Ok(())
}

/// `suemo toggle` — show the overlay, or hide a running one (round 5).
fn toggle() -> Result<()> {
    if ipc::gui_running() {
        ipc::gui_stop();
        println!("suemo hidden");
    } else {
        ipc::spawn_gui_detached()?;
        println!("suemo shown");
    }
    Ok(())
}

fn status() -> Result<()> {
    let mut stream = match ipc::connect() {
        Ok(stream) => stream,
        Err(_) => bail!("suemo daemon is not running"),
    };
    ipc::write_request(&mut stream, &Request::Status)?;
    match ipc::read_response(&mut stream)? {
        Response::Status { status } => {
            let up_min = (domain::now_ms() - status.started_utc).max(0) / 60_000;
            println!(
                "daemon v{} · {} event(s) · db {} · up {up_min} min · replica: {}",
                status.version, status.events, status.db_root, status.replica
            );
            Ok(())
        }
        Response::Err { message } => bail!("{message}"),
        _ => bail!("unexpected reply to status"),
    }
}

fn stop() -> Result<()> {
    let mut stream = match ipc::connect() {
        Ok(stream) => stream,
        Err(_) => {
            println!("suemo daemon is not running");
            return Ok(());
        }
    };
    ipc::write_request(&mut stream, &Request::Stop)?;
    match ipc::read_response(&mut stream)? {
        Response::Ok => {
            println!("suemo daemon stopped");
            Ok(())
        }
        Response::Err { message } => bail!("{message}"),
        _ => bail!("unexpected reply to stop"),
    }
}

fn request_range(from: i64, to: i64) -> Result<Vec<Event>> {
    match ipc::round_trip(&Request::Range { from, to })? {
        Response::Events { events } => Ok(events),
        Response::Err { message } => bail!("{message}"),
        _ => bail!("unexpected reply to range"),
    }
}

fn fmt_local(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "??:??".into())
}

/// `08:00–09:30 [kind] title` — the line format for today/week listings.
fn event_line(event: &Event) -> String {
    format!(
        "{}–{} [{:<10}] {}",
        fmt_local(event.starts_utc),
        fmt_local(event.ends_utc),
        event.kind,
        event.title
    )
}
