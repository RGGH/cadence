//! SQLite persistence for rides.
//!
//! * One row per ride in `rides`, one row per second in `samples`.
//! * WAL mode + incremental writes, so a crash or dead laptop loses at most
//!   the last second. Unfinished rides are recovered the next time the DB opens.
//! * Schema changes go through `MIGRATIONS` (tracked with `PRAGMA user_version`).
//!
//! The DB lives at `~/.local/share/cadence/rides.db` (XDG data dir).

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::{params, Connection};

/// Rides with less pedalling time than this are discarded rather than saved.
pub const MIN_SAVED_PEDALLING_SECS: u64 = 10;

/// Append-only list of schema migrations. Never edit an existing entry;
/// add a new one at the end instead.
const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE rides (
        id              INTEGER PRIMARY KEY,
        started_at      INTEGER NOT NULL,          -- unix seconds
        ended_at        INTEGER,                   -- NULL while recording / after a crash
        pedalling_secs  INTEGER NOT NULL DEFAULT 0,
        avg_cadence     REAL    NOT NULL DEFAULT 0,
        max_cadence     REAL    NOT NULL DEFAULT 0,
        target_cadence  REAL    NOT NULL,
        rpe             INTEGER,
        notes           TEXT
    );
    CREATE INDEX idx_rides_started ON rides (started_at DESC);

    CREATE TABLE samples (
        ride_id  INTEGER NOT NULL REFERENCES rides (id) ON DELETE CASCADE,
        t        INTEGER NOT NULL,                 -- seconds since ride start
        cadence  REAL    NOT NULL,                 -- rpm, 0 when stopped
        PRIMARY KEY (ride_id, t)
    ) WITHOUT ROWID;
    ",
];

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Final numbers written when a ride ends.
#[derive(Debug, Clone, Copy)]
pub struct RideStats {
    pub pedalling_secs: u64,
    pub avg_cadence: f64,
    pub max_cadence: f64,
}

/// A finished ride, as shown in the history list.
#[derive(Debug, Clone)]
pub struct RideSummary {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub pedalling_secs: u64,
    pub avg_cadence: f64,
    pub max_cadence: f64,
}

impl RideSummary {
    pub fn duration_secs(&self) -> u64 {
        (self.ended_at - self.started_at).max(0) as u64
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    // ───────────── opening ─────────────

    pub fn default_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("", "", "cadence")
            .context("could not work out a data directory (is $HOME set?)")?;
        Ok(dirs.data_dir().join("rides.db"))
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&Self::default_path()?)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // journal_mode returns a row, so it has to go through query_row.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;

        let mut db = Self { conn };
        db.migrate()?;
        db.recover_unfinished()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {} failed", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Close off rides left open by a crash / power loss: derive their totals
    /// from the samples that did get written, and drop them if they're trivial.
    /// Returns how many rides were found.
    pub fn recover_unfinished(&mut self) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare("SELECT id FROM rides WHERE ended_at IS NULL")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            ids
        };
        for id in &ids {
            tx.execute(
                "UPDATE rides SET
                    ended_at = started_at
                        + COALESCE((SELECT MAX(t) FROM samples WHERE ride_id = ?1), 0),
                    pedalling_secs =
                        (SELECT COUNT(*) FROM samples WHERE ride_id = ?1 AND cadence > 0),
                    avg_cadence = COALESCE(
                        (SELECT AVG(cadence) FROM samples WHERE ride_id = ?1 AND cadence > 0), 0),
                    max_cadence = COALESCE(
                        (SELECT MAX(cadence) FROM samples WHERE ride_id = ?1), 0)
                 WHERE id = ?1",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM rides WHERE id = ?1 AND pedalling_secs < ?2",
                params![id, MIN_SAVED_PEDALLING_SECS as i64],
            )?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    // ───────────── writing a ride ─────────────

    pub fn start_ride(&self, started_at: i64, target_cadence: f64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO rides (started_at, target_cadence) VALUES (?1, ?2)",
            params![started_at, target_cadence],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn add_sample(&self, ride_id: i64, t: u64, cadence: f64) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO samples (ride_id, t, cadence) VALUES (?1, ?2, ?3)")?
            .execute(params![ride_id, t as i64, cadence])?;
        Ok(())
    }

    pub fn finish_ride(&self, id: i64, ended_at: i64, stats: &RideStats) -> Result<()> {
        self.conn.execute(
            "UPDATE rides
             SET ended_at = ?2, pedalling_secs = ?3, avg_cadence = ?4, max_cadence = ?5
             WHERE id = ?1",
            params![
                id,
                ended_at,
                stats.pedalling_secs as i64,
                stats.avg_cadence,
                stats.max_cadence
            ],
        )?;
        Ok(())
    }

    // ───────────── reading / deleting ─────────────

    /// Finished rides, newest first.
    pub fn list_rides(&self, limit: usize) -> Result<Vec<RideSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, ended_at, pedalling_secs, avg_cadence, max_cadence
             FROM rides
             WHERE ended_at IS NOT NULL
             ORDER BY started_at DESC
             LIMIT ?1",
        )?;
        let rides = stmt
            .query_map(params![limit as i64], |r| {
                Ok(RideSummary {
                    id: r.get(0)?,
                    started_at: r.get(1)?,
                    ended_at: r.get(2)?,
                    pedalling_secs: r.get::<_, i64>(3)?.max(0) as u64,
                    avg_cadence: r.get(4)?,
                    max_cadence: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rides)
    }

    /// Delete a ride (its samples go with it via ON DELETE CASCADE).
    pub fn delete_ride(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM rides WHERE id = ?1", params![id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_count(db: &Db, id: i64) -> i64 {
        db.conn
            .query_row("SELECT COUNT(*) FROM samples WHERE ride_id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn ride_lifecycle() {
        let db = Db::open_in_memory().unwrap();
        let id = db.start_ride(1_000, 90.0).unwrap();
        for t in 1..=15 {
            db.add_sample(id, t, 90.0).unwrap();
        }
        // not listed until finished
        assert!(db.list_rides(10).unwrap().is_empty());

        db.finish_ride(
            id,
            1_015,
            &RideStats { pedalling_secs: 15, avg_cadence: 90.0, max_cadence: 95.0 },
        )
        .unwrap();

        let rides = db.list_rides(10).unwrap();
        assert_eq!(rides.len(), 1);
        assert_eq!(rides[0].duration_secs(), 15);
        assert_eq!(rides[0].pedalling_secs, 15);
        assert_eq!(rides[0].max_cadence, 95.0);
    }

    #[test]
    fn rides_listed_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let stats = RideStats { pedalling_secs: 20, avg_cadence: 80.0, max_cadence: 90.0 };
        for started in [100, 300, 200] {
            let id = db.start_ride(started, 90.0).unwrap();
            db.finish_ride(id, started + 20, &stats).unwrap();
        }
        let order: Vec<i64> = db.list_rides(10).unwrap().iter().map(|r| r.started_at).collect();
        assert_eq!(order, vec![300, 200, 100]);
    }

    #[test]
    fn crashed_ride_is_recovered_from_samples() {
        let mut db = Db::open_in_memory().unwrap();
        let id = db.start_ride(5_000, 90.0).unwrap();
        for t in 1..=30 {
            db.add_sample(id, t, if t <= 20 { 80.0 } else { 0.0 }).unwrap();
        }
        assert_eq!(db.recover_unfinished().unwrap(), 1);

        let rides = db.list_rides(10).unwrap();
        assert_eq!(rides.len(), 1);
        assert_eq!(rides[0].duration_secs(), 30);
        assert_eq!(rides[0].pedalling_secs, 20);
        assert!((rides[0].avg_cadence - 80.0).abs() < 1e-9);
    }

    #[test]
    fn trivial_crashed_ride_is_dropped() {
        let mut db = Db::open_in_memory().unwrap();
        let id = db.start_ride(5_000, 90.0).unwrap();
        db.add_sample(id, 1, 0.0).unwrap();
        db.add_sample(id, 2, 70.0).unwrap();
        db.recover_unfinished().unwrap();
        assert!(db.list_rides(10).unwrap().is_empty());
        assert_eq!(sample_count(&db, id), 0);
    }

    #[test]
    fn deleting_a_ride_removes_its_samples() {
        let db = Db::open_in_memory().unwrap();
        let id = db.start_ride(1, 90.0).unwrap();
        db.add_sample(id, 1, 90.0).unwrap();
        db.finish_ride(
            id,
            2,
            &RideStats { pedalling_secs: 20, avg_cadence: 90.0, max_cadence: 90.0 },
        )
        .unwrap();
        db.delete_ride(id).unwrap();
        assert!(db.list_rides(10).unwrap().is_empty());
        assert_eq!(sample_count(&db, id), 0);
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut db = Db::open_in_memory().unwrap();
        db.migrate().unwrap();
        let v: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }
}
