//! mdrv-db engine wrapper — this DB is user data, the source of truth
//! (proposal §Data model). Never hand-edit `<root>/live/`; every write
//! goes through the engine (one `execute` = one LSN).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use mdrv_db::engine::{Engine, EngineConfig, MutateRequest};
use mdrv_db::{Op, PortValue, SqlKind};
use serde_json::Value;

use crate::domain::Event;

pub const DB_NAME: &str = "suemo";
pub const ACTOR: &str = "suemo-daemon";

const EVENTS: &str = "events";
const EVENT_COLUMNS: [&str; 8] = [
    "id",
    "starts_utc",
    "ends_utc",
    "title",
    "kind",
    "note",
    "created_utc",
    "updated_utc",
];

/// Data root: env override, else the fleet default (decisions.md Q8.4).
pub fn default_root() -> PathBuf {
    std::env::var_os("SUEMO_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/x/db/suemo"))
}

fn create_events() -> String {
    // Proposal §Data model, verbatim columns. `idem_key` stays NULL until
    // the v0.2 offline-capture replay.
    format!(
        "CREATE TABLE IF NOT EXISTS {EVENTS} (
			id          TEXT PRIMARY KEY,
			starts_utc  INTEGER NOT NULL,
			ends_utc    INTEGER NOT NULL,
			title       TEXT NOT NULL,
			kind        TEXT NOT NULL,
			note        TEXT NOT NULL DEFAULT '',
			idem_key    TEXT UNIQUE,
			created_utc INTEGER NOT NULL,
			updated_utc INTEGER NOT NULL
		)"
    )
}

fn create_indexes() -> Vec<String> {
    vec![
        format!("CREATE INDEX IF NOT EXISTS idx_{EVENTS}_starts ON {EVENTS}(starts_utc)"),
        format!("CREATE INDEX IF NOT EXISTS idx_{EVENTS}_ends ON {EVENTS}(ends_utc)"),
    ]
}

pub struct Db {
    engine: Engine,
}

impl Db {
    /// Open (creating `<root>/live/` first — `live_dir` resolution order).
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root.join("live"))
            .with_context(|| format!("creating {}", root.join("live").display()))?;
        let live = mdrv_db::live_dir(root);
        std::fs::create_dir_all(&live).with_context(|| format!("creating {}", live.display()))?;
        let port = mdrv_db::TursoPort::open(live.join("app.db"))
            .map_err(|e| anyhow!("opening turso port: {e}"))?;
        // User data, not derived: per-write durability (proposal §Data model).
        let engine = Engine::open(
            root,
            DB_NAME,
            Box::new(port),
            EngineConfig {
                fsync_each_write: true,
            },
        )
        .context("opening mdrv-db engine")?;
        let db = Self { engine };
        let mut ddl = vec![create_events()];
        ddl.extend(create_indexes());
        db.engine.bootstrap(&ddl).context("bootstrapping schema")?;
        Ok(db)
    }

    /// Insert a new event; returns it (fresh id/timestamps) + the LSN.
    pub fn add(
        &self,
        title: &str,
        kind: &str,
        starts_utc: i64,
        ends_utc: i64,
    ) -> Result<(Event, i64)> {
        let event = Event::new(title, Some(kind.to_string()), starts_utc, ends_utc);
        event.validate()?;
        let lsn = self.execute(&event, SqlKind::Insert)?;
        Ok((event, lsn))
    }

    /// Full-row replace — last-write-wins (decisions.md Q7). Returns the LSN.
    pub fn update(&self, event: &Event) -> Result<i64> {
        event.validate()?;
        self.execute(event, SqlKind::Upsert)
    }

    /// Delete by id → `(existed, lsn)`. Existence comes from a SELECT:
    /// mdrv-db 0.5.1's turso port does not report a reliable affected-row
    /// count for no-match DELETEs (verified empirically), and reads are
    /// serialized with writes behind the single engine thread anyway.
    pub fn delete(&self, id: &str) -> Result<(bool, i64)> {
        let hits = self.select(
            "SELECT id FROM events WHERE id = ? LIMIT 1",
            vec![PortValue::Text(id.into())],
        )?;
        if hits.is_empty() {
            return Ok((false, 0));
        }
        let outcome = self
            .engine
            .execute(MutateRequest {
                actor: ACTOR.into(),
                ops: vec![Op::Sql {
                    kind: SqlKind::Delete,
                    table: EVENTS.into(),
                    pk_col: "id".into(),
                    columns: vec!["id".into()],
                    values: vec![PortValue::Text(id.into())],
                    pk: PortValue::Text(id.into()),
                }],
                idem_key: None,
                response: None,
            })
            .map_err(|e| anyhow!("deleting event: {e}"))?;
        Ok((true, outcome.lsn as i64))
    }

    /// Events overlapping `[from, to)`, ordered by start.
    pub fn range(&self, from: i64, to: i64) -> Result<Vec<Event>> {
        ensure!(from < to, "range: from must be before to");
        let rows = self.select(
            "SELECT * FROM events WHERE starts_utc < ? AND ends_utc > ? ORDER BY starts_utc, id",
            vec![PortValue::Int(to), PortValue::Int(from)],
        )?;
        Ok(rows.iter().filter_map(row_to_event).collect())
    }

    /// Hours per kind, clipped to `[from, to)`, descending (Q4).
    pub fn kind_hours(&self, from: i64, to: i64) -> Result<Vec<(String, f64)>> {
        ensure!(from < to, "stats: from must be before to");
        let rows = self.select(
            "SELECT kind, \
					SUM(MIN(ends_utc, ?) - MAX(starts_utc, ?)) / 3600000.0 AS hours \
			 FROM events \
			 WHERE starts_utc < ? AND ends_utc > ? \
			 GROUP BY kind \
			 ORDER BY hours DESC, kind",
            vec![
                PortValue::Int(to),
                PortValue::Int(from),
                PortValue::Int(to),
                PortValue::Int(from),
            ],
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some((
                    row.get("kind")?.as_str()?.to_string(),
                    row.get("hours")?.as_f64()?,
                ))
            })
            .collect())
    }

    pub fn count(&self) -> Result<u64> {
        let rows = self.select("SELECT COUNT(*) AS n FROM events", vec![])?;
        Ok(rows
            .first()
            .and_then(|row| row.get("n").and_then(Value::as_u64))
            .unwrap_or(0))
    }

    fn select(&self, sql: &str, params: Vec<PortValue>) -> Result<Vec<Value>> {
        let out = self.engine.query(sql, params).map_err(|e| anyhow!("{e}"))?;
        Ok(out.as_array().cloned().unwrap_or_default())
    }

    /// One execute = one LSN = one tx (engine contract).
    fn execute(&self, event: &Event, kind: SqlKind) -> Result<i64> {
        let outcome = self
            .engine
            .execute(MutateRequest {
                actor: ACTOR.into(),
                ops: vec![Op::Sql {
                    kind,
                    table: EVENTS.into(),
                    pk_col: "id".into(),
                    columns: EVENT_COLUMNS.iter().map(|c| c.to_string()).collect(),
                    values: row_values(event),
                    pk: PortValue::Text(event.id.clone()),
                }],
                idem_key: None,
                response: None,
            })
            .map_err(|e| anyhow!("writing event: {e}"))?;
        Ok(outcome.lsn as i64)
    }
}

fn row_values(e: &Event) -> Vec<PortValue> {
    vec![
        PortValue::Text(e.id.clone()),
        PortValue::Int(e.starts_utc),
        PortValue::Int(e.ends_utc),
        PortValue::Text(e.title.clone()),
        PortValue::Text(e.kind.clone()),
        PortValue::Text(e.note.clone()),
        PortValue::Int(e.created_utc),
        PortValue::Int(e.updated_utc),
    ]
}

fn row_to_event(row: &Value) -> Option<Event> {
    Some(Event {
        id: row.get("id")?.as_str()?.to_string(),
        starts_utc: row.get("starts_utc")?.as_i64()?,
        ends_utc: row.get("ends_utc")?.as_i64()?,
        title: row.get("title")?.as_str()?.to_string(),
        kind: row.get("kind")?.as_str()?.to_string(),
        note: row
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created_utc: row.get("created_utc")?.as_i64()?,
        updated_utc: row.get("updated_utc")?.as_i64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("suemo-db-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn round_trip_and_lock_release() {
        let root = test_root("roundtrip");
        let _ = std::fs::remove_dir_all(&root);
        let db = Db::open(&root).unwrap();

        let (event, lsn1) = db.add("Deep work", "work", 1_000, 2_000).unwrap();
        assert_eq!(lsn1, 1, "bootstrap DDL is not journaled; add is LSN 1");
        assert_eq!(db.count().unwrap(), 1);

        assert_eq!(db.range(0, 5_000).unwrap().len(), 1);
        assert!(db.range(2_000, 5_000).unwrap().is_empty()); // [from, to)

        let mut fixed = event.clone();
        fixed.title = "Fixed".into();
        fixed.ends_utc = 3_000;
        let lsn2 = db.update(&fixed).unwrap();
        assert!(lsn2 > lsn1);
        let got = db.range(0, 5_000).unwrap().pop().unwrap();
        assert_eq!(got.title, "Fixed");
        assert_eq!(got.ends_utc, 3_000);

        assert!(db.delete(&event.id).unwrap().0);
        assert!(!db.delete(&event.id).unwrap().0);
        assert_eq!(db.count().unwrap(), 0);

        drop(db);
        // The fjall lock must be released: a fresh open (post-stop verify
        // path) has to work.
        let db2 = Db::open(&root).unwrap();
        assert_eq!(db2.count().unwrap(), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn kind_hours_are_clipped_to_the_window() {
        let root = test_root("stats");
        let _ = std::fs::remove_dir_all(&root);
        let db = Db::open(&root).unwrap();
        // 1h inside the window, 1h half-out: work = 1.5h total.
        db.add("a", "work", 0, 3_600_000).unwrap();
        db.add("b", "work", 3_600_000, 7_200_000).unwrap();
        let stats = db.kind_hours(0, 5_400_000).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, "work");
        assert!((stats[0].1 - 1.5).abs() < 1e-9);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn invalid_writes_are_rejected() {
        let root = test_root("invalid");
        let _ = std::fs::remove_dir_all(&root);
        let db = Db::open(&root).unwrap();
        assert!(db.add("", "work", 0, 1).is_err());
        assert!(db.add("t", "", 0, 1).is_err());
        assert!(db.add("t", "work", 1, 1).is_err());
        assert!(db.add("t", "work", 2, 1).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
