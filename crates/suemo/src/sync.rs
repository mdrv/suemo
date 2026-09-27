//! One-way sync (proposal §Sync): the desktop daemon schedules recovery
//! backups (`<ts>-daemon`, debounced on change ≈10 min + hourly, verify-ok
//! required); a replica daemon watches an incoming dir for pushed backups,
//! verifies them, and swaps them in under the engine lock. Push transport
//! (rsync) is external — drafted privately, never hardcoded here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::engine::Db;

/// Backup cadence (proposal §Sync 1).
const DEBOUNCE_MS: i64 = 10 * 60_000;
const HOURLY_MS: i64 = 60 * 60_000;

/// Desktop-side scheduler state, ticked by the engine loop.
pub struct BackupState {
    last_change: i64,
    last_backup: i64,
}

impl BackupState {
    pub fn new(now: i64) -> Self {
        Self {
            last_change: now,
            last_backup: now,
        }
    }

    pub fn on_change(&mut self, now: i64) {
        self.last_change = now;
    }

    /// Run a backup if due: ≥10 min after the last change (and no backup
    /// since it), or hourly regardless. Returns whether one ran.
    pub fn tick(&mut self, now: i64, db: &Db, root: &Path) -> bool {
        let due = (now - self.last_change >= DEBOUNCE_MS && self.last_change > self.last_backup)
            || now - self.last_backup >= HOURLY_MS;
        if !due {
            return false;
        }
        let dest = root.join("recovery").join(format!("{now}-daemon"));
        if let Err(err) = run_backup(db, &dest) {
            log::error!("backup failed ({err:#}); retrying after the debounce window");
            self.last_backup = now;
            return true;
        }
        if let Err(err) = db.verify() {
            log::error!("post-backup verify failed: {err:#}");
        } else {
            log::info!("backup ok at {}", dest.display());
        }
        self.last_backup = now;
        true
    }
}

fn run_backup(db: &Db, dest: &Path) -> Result<()> {
    db.backup(dest)
        .with_context(|| format!("backing up into {}", dest.display()))
}

/// Watch `incoming` for pushed backup dirs (`<ms>-daemon` with a
/// `manifest.json` — it is written last, so its presence means the rsync is
/// complete). Complete backups are copied to a staging dir under `root` and
/// handed to the engine thread via `tx`.
pub(crate) fn watch_incoming(
    incoming: PathBuf,
    root: PathBuf,
    tx: std::sync::mpsc::Sender<crate::daemon::EngineMsg>,
) {
    std::thread::spawn(move || {
        let mut last_adopted: i64 = 0;
        loop {
            if let Err(err) = scan_once(&incoming, &root, &mut last_adopted, &tx) {
                log::warn!("replica scan failed: {err:#}");
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

pub(crate) fn scan_once(
    incoming: &Path,
    root: &Path,
    last_adopted: &mut i64,
    tx: &std::sync::mpsc::Sender<crate::daemon::EngineMsg>,
) -> Result<()> {
    let entries =
        std::fs::read_dir(incoming).with_context(|| format!("reading {}", incoming.display()))?;
    let mut newest = *last_adopted;
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let Some(ts) = backup_ts(&path) else {
            continue;
        };
        if ts <= *last_adopted || !path.join("manifest.json").is_file() {
            continue;
        }
        newest = newest.max(ts);
        let staging = stage_backup(root, &path)?;
        tx.send(crate::daemon::EngineMsg::Restore { staging })
            .map_err(|e| anyhow!("engine gone: {e}"))?;
    }
    *last_adopted = newest;
    Ok(())
}

/// `<digits>-<label>` per mdrv-db convention; the digits are the creation ts.
pub fn backup_ts(path: &Path) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || !name[digits.len()..].starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

/// Copy a backup dir (app.db + fjall/ + blobs/ + manifest.json) into
/// `<root>/staging-<ts>/` — never write inside the rsync target itself.
fn stage_backup(root: &Path, backup: &Path) -> Result<PathBuf> {
    let ts = backup_ts(backup).ok_or_else(|| anyhow!("backup without ts: {backup:?}"))?;
    let staging = root.join(format!("staging-{ts}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clearing {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)?;
    std::fs::copy(backup.join("app.db"), staging.join("app.db"))
        .with_context(|| format!("copying app.db from {backup:?}"))?;
    copy_dir(&backup.join("fjall"), &staging.join("fjall"))?;
    if backup.join("blobs").is_dir() {
        copy_dir(&backup.join("blobs"), &staging.join("blobs"))?;
    }
    std::fs::copy(backup.join("manifest.json"), staging.join("manifest.json"))?;
    Ok(staging)
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {src:?}"))? {
        let path = entry?.path();
        if path.is_dir() {
            copy_dir(&path, &dest.join(path.file_name().unwrap()))?;
        } else {
            std::fs::copy(&path, dest.join(path.file_name().unwrap()))?;
        }
    }
    Ok(())
}

/// Restore-swap under the engine lock (proposal §Sync 3): the caller drops
/// the old engine, we verify the staged backup offline, make it `live/`,
/// reopen + re-verify. On any failure the previous `live/` returns and the
/// replica keeps serving stale-but-good data.
pub fn adopt(old: Option<Db>, root: &Path, staging: &Path) -> Result<(Db, i64)> {
    drop(old); // release the fjall lock first
    let applied = manifest_applied(staging)?;

    let live = root.join("live");
    let old_live = root.join("live.old");
    if old_live.exists() {
        std::fs::remove_dir_all(&old_live).context("clearing stale live.old")?;
    }
    if live.exists() {
        std::fs::rename(&live, &old_live).context("moving live aside")?;
    }
    match std::fs::rename(staging, &live) {
        Ok(()) => {}
        Err(err) => {
            // put the old live back before reporting
            let _ = std::fs::rename(&old_live, &live);
            return Err(anyhow!("swapping staging in: {err}"));
        }
    }

    match Db::open(root).and_then(|db| db.verify().map(|_| db).map_err(|e| anyhow!(e.to_string())))
    {
        Ok(db) => {
            let _ = std::fs::remove_dir_all(&old_live);
            log::info!("adopted backup (applied_lsn {applied})");
            Ok((db, applied))
        }
        Err(err) => {
            log::error!("adopted backup failed to open/verify ({err:#}); rolling back");
            let _ = std::fs::remove_dir_all(&live);
            if let Err(rollback) = std::fs::rename(&old_live, &live) {
                log::error!("rollback failed: {rollback}; next boot recreates live/");
            }
            Err(anyhow!("backup at {staging:?} did not open: {err:#}"))
        }
    }
}

fn manifest_applied(staging: &Path) -> Result<i64> {
    let raw =
        std::fs::read_to_string(staging.join("manifest.json")).context("reading manifest.json")?;
    let v: serde_json::Value = serde_json::from_str(&raw).context("parsing manifest.json")?;
    Ok(v.get("applied_lsn")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("manifest has no applied_lsn"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("suemo-sync-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn backup_then_adopt_into_a_fresh_replica() {
        let src_root = test_root("src");
        let rep_root = test_root("replica");
        let _ = std::fs::remove_dir_all(&src_root);
        let _ = std::fs::remove_dir_all(&rep_root);

        let db = Db::open(&src_root).unwrap();
        let (_, lsn) = db.add("sync me", "work", 0, 60_000).unwrap();
        let dest = src_root.join("recovery").join("123-daemon");
        db.backup(&dest).unwrap();

        // Replica side: verify the offline marker + adoption swap.
        assert_eq!(backup_ts(&dest), Some(123));
        assert!(dest.join("manifest.json").is_file());
        let staged = stage_backup(&rep_root, &dest).unwrap();
        let (replica, _applied) = adopt(None, &rep_root, &staged).expect("adoption succeeds");
        let (got, events) = (replica.count().unwrap(), replica.range(0, 70_000).unwrap());
        assert_eq!(got, 1);
        assert_eq!(events.len(), 1);
        assert!(lsn >= 1);
        assert!(rep_root.join("live").join("app.db").is_file());
        assert!(!rep_root.join("live.old").exists());

        // A bad backup must roll back to the previous live.
        let bad = rep_root.join("staging-999");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("app.db"), b"not a database").unwrap();
        std::fs::write(bad.join("manifest.json"), r#"{"applied_lsn":9}"#).unwrap();
        let keep = adopt(Some(replica), &rep_root, &bad);
        assert!(keep.is_err(), "junk backup must be rejected");
        assert!(rep_root.join("live").join("app.db").is_file());

        std::fs::remove_dir_all(&src_root).ok();
        std::fs::remove_dir_all(&rep_root).ok();
    }

    #[test]
    fn scheduler_respects_debounce_and_hourly() {
        let root = test_root("sched");
        let _ = std::fs::remove_dir_all(&root);
        let db = Db::open(&root).unwrap();
        let mut st = BackupState::new(0);
        // Nothing due immediately, nor before any change.
        assert!(!st.tick(5 * 60_000, &db, &root));
        // A change at t=1min: due 10 min later, not before.
        st.on_change(60_000);
        assert!(!st.tick(6 * 60_000 + 299_999, &db, &root));
        assert!(st.tick(11 * 60_000 + 1, &db, &root));
        // Just backed up; the hourly branch holds it off.
        assert!(!st.tick(61 * 60_000, &db, &root));
        // An hour past the backup: hourly due even without changes.
        assert!(st.tick(71 * 60_000 + 1, &db, &root));
        // A fresh change moves the debounce horizon forward again.
        st.on_change(72 * 60_000);
        assert!(!st.tick(76 * 60_000, &db, &root));
        assert!(st.tick(82 * 60_000 + 1, &db, &root));
        std::fs::remove_dir_all(&root).ok();
    }
}
