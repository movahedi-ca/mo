//! SQLite store. V3 section 5.2 and 6.3.
//! One file, WAL mode, synchronous FULL, busy_timeout 5000, foreign_keys ON.
//! Schema version 1. One transaction covers queue, lease, reservation, audit.
//! Store owns all durable state. No module writes state directly.

use crate::core::{AuditEvent, Lease, LeaseUsage, MoError, QueuedRequest, Result};
use rusqlite::{params, Connection, Transaction};
use std::sync::Mutex;

pub const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schema_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS requests (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  resource_class TEXT NOT NULL,
  cpu_cores INTEGER NOT NULL,
  mem_mb INTEGER NOT NULL,
  desktops INTEGER NOT NULL,
  duration_min INTEGER NOT NULL,
  priority_class TEXT NOT NULL,
  deadline TEXT,
  needs_bridge INTEGER NOT NULL DEFAULT 0,
  needs_display INTEGER NOT NULL DEFAULT 0,
  capabilities_json TEXT NOT NULL DEFAULT '[]',
  mem_mb_max INTEGER,
  lease_id TEXT,
  created_unix INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'queued'
);
CREATE INDEX IF NOT EXISTS idx_requests_status ON requests(status, id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_requests_idempotency
  ON requests(agent_id, idempotency_key);
CREATE TABLE IF NOT EXISTS leases (
  lease_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  resource_class TEXT NOT NULL,
  desktop TEXT NOT NULL,
  display TEXT NOT NULL,
  cdp_port INTEGER,
  vnc_port INTEGER,
  profile TEXT,
  cpu_cores INTEGER NOT NULL,
  mem_mb INTEGER NOT NULL,
  state TEXT NOT NULL,
  start_unix INTEGER NOT NULL,
  end_unix INTEGER NOT NULL,
  heartbeat_due_unix INTEGER NOT NULL,
  heartbeat_misses INTEGER NOT NULL DEFAULT 0,
  extensions INTEGER NOT NULL DEFAULT 0,
  last_heartbeat_unix INTEGER NOT NULL DEFAULT 0,
  created_unix INTEGER NOT NULL,
  started_desktop INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_leases_desktop_active
  ON leases(desktop)
  WHERE state IN ('reserving','granted','active','stale','extension_pending','interrupted')
  AND desktop != 'none';
CREATE INDEX IF NOT EXISTS idx_leases_state ON leases(state);
CREATE TABLE IF NOT EXISTS flagged_desktops (
  desktop TEXT PRIMARY KEY,
  reason TEXT NOT NULL,
  flagged_unix INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS reservations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  lease_id TEXT NOT NULL REFERENCES leases(lease_id),
  desktop TEXT NOT NULL,
  cpu_cores INTEGER NOT NULL,
  mem_mb INTEGER NOT NULL,
  bridge_slot INTEGER NOT NULL DEFAULT 0,
  created_unix INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS heartbeats (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  lease_id TEXT NOT NULL REFERENCES leases(lease_id),
  beat_unix INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  event TEXT NOT NULL,
  agent_id TEXT,
  job_id TEXT,
  lease_id TEXT,
  desktop TEXT,
  detail TEXT NOT NULL,
  created_unix INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_created ON audit(created_unix);
CREATE INDEX IF NOT EXISTS idx_hb ON heartbeats(lease_id, beat_unix DESC);
CREATE TABLE IF NOT EXISTS inventory_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  taken_unix INTEGER NOT NULL,
  load1 REAL NOT NULL,
  mem_available_mb INTEGER NOT NULL,
  mem_total_mb INTEGER NOT NULL,
  disk_free_gb REAL NOT NULL,
  chrome_count INTEGER NOT NULL,
  desktops_up INTEGER NOT NULL,
  bridge_pool_free INTEGER,
  battery_pct INTEGER,
  battery_state TEXT
);
CREATE INDEX IF NOT EXISTS idx_snapshots_taken ON inventory_snapshots(taken_unix);
-- Step 1 of S1-for-MO: one measured-usage row per lease that ended.
-- peak_rss_mb / cpu_time_s are null when neither the daemon nor the
-- agent measured the lease; source names the evidence (agent,
-- daemon, daemon-partial, none). One row per lease, ever.
CREATE TABLE IF NOT EXISTS lease_usage (
  lease_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  resource_class TEXT NOT NULL,
  peak_rss_mb INTEGER,
  cpu_time_s REAL,
  duration_s INTEGER NOT NULL,
  source TEXT NOT NULL,
  recorded_unix INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_recorded ON lease_usage(recorded_unix);
CREATE INDEX IF NOT EXISTS idx_usage_class ON lease_usage(resource_class);
";

pub struct Store {
    conn: Mutex<Connection>,
}

/// Migrate a v3 database to v4. Idempotent: CREATE TABLE IF NOT
/// EXISTS and CREATE INDEX IF NOT EXISTS. Adds the lease_usage
/// table (Step 1 of S1-for-MO). No existing table is touched and
/// no lease history is lost.
fn migrate_v3_to_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lease_usage (
          lease_id TEXT PRIMARY KEY,
          agent_id TEXT NOT NULL,
          job_id TEXT NOT NULL,
          resource_class TEXT NOT NULL,
          peak_rss_mb INTEGER,
          cpu_time_s REAL,
          duration_s INTEGER NOT NULL,
          source TEXT NOT NULL,
          recorded_unix INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_recorded ON lease_usage(recorded_unix);
        CREATE INDEX IF NOT EXISTS idx_usage_class ON lease_usage(resource_class);",
    )?;
    conn.execute(
        "UPDATE schema_meta SET value='4' WHERE key='schema_version'",
        [],
    )?;
    Ok(())
}

/// Migrate a v2 database to v3. Idempotent: every step checks first.
/// Adds `started_desktop` to leases: 1 when MO started the desktop,
/// 0 when it reused one that was already up. Existing rows default 0,
/// which is the safe value: release never stops a pre-existing desktop.
fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    let has_col = |table: &str, col: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == col {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has_col("leases", "started_desktop")? {
        conn.execute(
            "ALTER TABLE leases ADD COLUMN started_desktop INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    conn.execute(
        "UPDATE schema_meta SET value='3' WHERE key='schema_version'",
        [],
    )?;
    Ok(())
}

/// Migrate a v1 database to v2. Idempotent: every step checks first.
fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    let has_col = |table: &str, col: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == col {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has_col("leases", "last_heartbeat_unix")? {
        conn.execute(
            "ALTER TABLE leases ADD COLUMN last_heartbeat_unix INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_col("requests", "mem_mb_max")? {
        conn.execute("ALTER TABLE requests ADD COLUMN mem_mb_max INTEGER", [])?;
    }
    if !has_col("requests", "lease_id")? {
        conn.execute("ALTER TABLE requests ADD COLUMN lease_id TEXT", [])?;
    }
    conn.execute(
        "UPDATE leases SET last_heartbeat_unix =
         COALESCE((SELECT MAX(beat_unix) FROM heartbeats
                   WHERE heartbeats.lease_id = leases.lease_id), start_unix)",
        [],
    )?;
    conn.execute("DROP INDEX IF EXISTS idx_leases_desktop_active", [])?;
    conn.execute(
        "CREATE UNIQUE INDEX idx_leases_desktop_active ON leases(desktop)
         WHERE state IN ('reserving','granted','active','stale','extension_pending','interrupted')
         AND desktop != 'none'",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_requests_status ON requests(status, id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_leases_state ON leases(state)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_created ON audit(created_unix)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_hb ON heartbeats(lease_id, beat_unix DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_snapshots_taken ON inventory_snapshots(taken_unix)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS flagged_desktops (
         desktop TEXT PRIMARY KEY, reason TEXT NOT NULL, flagged_unix INTEGER NOT NULL)",
        [],
    )?;
    conn.execute(
        "UPDATE schema_meta SET value='2' WHERE key='schema_version'",
        [],
    )?;
    Ok(())
}

impl Store {
    /// Open or create the DB. Refuses to start on a newer schema version: E10.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "cache_size", -8000)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "mmap_size", 67108864)?;
        conn.execute_batch(SCHEMA)?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        match version {
            None => {
                conn.execute(
                    "INSERT INTO schema_meta(key, value) VALUES('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) => {
                let n: i64 = v
                    .parse()
                    .map_err(|_| MoError::e10(format!("bad schema version in DB: {v}")))?;
                if n > SCHEMA_VERSION {
                    return Err(MoError::e10(format!(
                        "DB schema version {n} is newer than binary {SCHEMA_VERSION}"
                    )));
                }
                if n == 1 {
                    migrate_v1_to_v2(&conn)?;
                    migrate_v2_to_v3(&conn)?;
                    migrate_v3_to_v4(&conn)?;
                } else if n == 2 {
                    migrate_v2_to_v3(&conn)?;
                    migrate_v3_to_v4(&conn)?;
                } else if n == 3 {
                    migrate_v3_to_v4(&conn)?;
                }
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Set the granted lease id on a request row. Lets cmd_request return
    /// the exact lease instead of guessing by agent_id + job_id.
    pub fn set_request_lease(&self, id: i64, lease_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "UPDATE requests SET lease_id=?1 WHERE id=?2",
            params![lease_id, id],
        )?;
        Ok(())
    }

    pub fn get_request(&self, id: i64) -> Result<Option<QueuedRequest>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, job_id, idempotency_key, resource_class, cpu_cores,
             mem_mb, desktops, duration_min, priority_class, deadline, needs_bridge,
             needs_display, mem_mb_max, lease_id, capabilities_json, created_unix, status
             FROM requests WHERE id=?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_request(row)?))
        } else {
            Ok(None)
        }
    }

    /// Latest audit row for one agent + job, newest first. Used to report
    /// the real denial reason instead of a generic E04.
    pub fn last_audit_for(&self, agent_id: &str, job_id: &str) -> Result<Option<AuditEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, event, agent_id, job_id, lease_id, desktop, detail, created_unix
             FROM audit WHERE agent_id=?1 AND job_id=?2 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![agent_id, job_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(AuditEvent {
                id: row.get(0)?,
                event: row.get(1)?,
                agent_id: row.get(2)?,
                job_id: row.get(3)?,
                lease_id: row.get(4)?,
                desktop: row.get(5)?,
                detail: row.get(6)?,
                created_unix: row.get(7)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Latest audit row for one event + desktop since `since_unix`, if any.
    /// Used to coalesce repeated bypass / skip audits.
    pub fn last_audit_event(
        &self,
        event: &str,
        desktop: &str,
        since_unix: i64,
    ) -> Result<Option<AuditEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, event, agent_id, job_id, lease_id, desktop, detail, created_unix
             FROM audit WHERE event=?1 AND desktop=?2 AND created_unix>=?3
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![event, desktop, since_unix])?;
        if let Some(row) = rows.next()? {
            Ok(Some(AuditEvent {
                id: row.get(0)?,
                event: row.get(1)?,
                agent_id: row.get(2)?,
                job_id: row.get(3)?,
                lease_id: row.get(4)?,
                desktop: row.get(5)?,
                detail: row.get(6)?,
                created_unix: row.get(7)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Retention: snapshots older than 24h, skip audits older than a day,
    /// heartbeats except the latest per lease, reservations whose lease is
    /// terminal. Returns (snapshots, audits, heartbeats, reservations) deleted.
    pub fn prune(&self, now: i64) -> Result<(i64, i64, i64, i64)> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let snaps = conn.execute(
            "DELETE FROM inventory_snapshots WHERE taken_unix < ?1",
            params![now - 86400],
        )?;
        let audits = conn.execute(
            "DELETE FROM audit WHERE event='skip' AND created_unix < ?1",
            params![now - 86400],
        )?;
        let beats = conn.execute(
            "DELETE FROM heartbeats WHERE id NOT IN
             (SELECT MAX(id) FROM heartbeats GROUP BY lease_id)",
            [],
        )?;
        let res = conn.execute(
            "DELETE FROM reservations WHERE lease_id IN
             (SELECT lease_id FROM leases
              WHERE state IN ('released','expired','failed','denied','cancelled'))",
            [],
        )?;
        Ok((snaps as i64, audits as i64, beats as i64, res as i64))
    }

    /// Flag a desktop whose stop failed. Fold refuses flagged desktops.
    pub fn flag_desktop(&self, desktop: &str, reason: &str, now: i64) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "INSERT INTO flagged_desktops(desktop, reason, flagged_unix)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(desktop) DO UPDATE SET reason=excluded.reason,
             flagged_unix=excluded.flagged_unix",
            params![desktop, reason, now],
        )?;
        Ok(())
    }

    pub fn is_flagged(&self, desktop: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM flagged_desktops WHERE desktop=?1",
            params![desktop],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn clear_flag(&self, desktop: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let n = conn.execute(
            "DELETE FROM flagged_desktops WHERE desktop=?1",
            params![desktop],
        )?;
        Ok(n > 0)
    }

    /// Checkpoint the WAL, truncating it. Run daily from the tick.
    pub fn checkpoint(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }

    /// Run `f` inside one transaction. Commit or roll back as one unit.
    pub fn with_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T>,
    {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Audit insert inside a transaction. One row per event.
    #[allow(clippy::too_many_arguments)]
    pub fn audit_tx(
        tx: &Transaction<'_>,
        event: &str,
        agent_id: Option<&str>,
        job_id: Option<&str>,
        lease_id: Option<&str>,
        desktop: Option<&str>,
        detail: &str,
        now: i64,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO audit(event, agent_id, job_id, lease_id, desktop, detail, created_unix)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![event, agent_id, job_id, lease_id, desktop, detail, now],
        )?;
        Ok(())
    }

    /// Audit insert outside a transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn audit(
        &self,
        event: &str,
        agent_id: Option<&str>,
        job_id: Option<&str>,
        lease_id: Option<&str>,
        desktop: Option<&str>,
        detail: &str,
        now: i64,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "INSERT INTO audit(event, agent_id, job_id, lease_id, desktop, detail, created_unix)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![event, agent_id, job_id, lease_id, desktop, detail, now],
        )?;
        Ok(())
    }

    pub fn insert_request(&self, req: &QueuedRequest) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "INSERT INTO requests(agent_id, job_id, idempotency_key, resource_class,
             cpu_cores, mem_mb, desktops, duration_min, priority_class, deadline,
             needs_bridge, needs_display, capabilities_json, mem_mb_max, lease_id,
             created_unix, status)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, NULL, ?15, ?16)",
            params![
                req.agent_id,
                req.job_id,
                req.idempotency_key,
                req.resource_class,
                req.cpu_cores,
                req.mem_mb,
                req.desktops,
                req.duration_min,
                req.priority_class,
                req.deadline,
                req.needs_bridge as i64,
                req.needs_display as i64,
                serde_json::to_string(&req.capabilities)
                    .map_err(|e| MoError::e10(format!("capabilities encode failed: {e}")))?,
                req.mem_mb_max,
                req.created_unix,
                req.status,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn find_request_by_idempotency(
        &self,
        agent_id: &str,
        key: &str,
    ) -> Result<Option<QueuedRequest>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, job_id, idempotency_key, resource_class, cpu_cores,
             mem_mb, desktops, duration_min, priority_class, deadline, needs_bridge,
             needs_display, mem_mb_max, lease_id, capabilities_json, created_unix, status
             FROM requests WHERE agent_id=?1 AND idempotency_key=?2",
        )?;
        let mut rows = stmt.query(params![agent_id, key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_request(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_queued_requests(&self) -> Result<Vec<QueuedRequest>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, job_id, idempotency_key, resource_class, cpu_cores,
             mem_mb, desktops, duration_min, priority_class, deadline, needs_bridge,
             needs_display, mem_mb_max, lease_id, capabilities_json, created_unix, status
             FROM requests WHERE status='queued' ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], row_to_request)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_request_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "UPDATE requests SET status=?1 WHERE id=?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn insert_lease_tx(tx: &Transaction<'_>, lease: &Lease) -> Result<()> {
        tx.execute(
            "INSERT INTO leases(lease_id, agent_id, job_id, resource_class, desktop, display,
             cdp_port, vnc_port, profile, cpu_cores, mem_mb, state, start_unix, end_unix,
             heartbeat_due_unix, heartbeat_misses, extensions, last_heartbeat_unix,
             created_unix, started_desktop)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                lease.lease_id,
                lease.agent_id,
                lease.job_id,
                lease.resource_class,
                lease.desktop,
                lease.display,
                lease.cdp_port,
                lease.vnc_port,
                lease.profile,
                lease.cpu_cores,
                lease.mem_mb,
                lease.state,
                lease.start_unix,
                lease.end_unix,
                lease.heartbeat_due_unix,
                lease.heartbeat_misses,
                lease.extensions,
                lease.last_heartbeat_unix,
                lease.created_unix,
                lease.started_desktop,
            ],
        )?;
        Ok(())
    }

    pub fn get_lease(&self, lease_id: &str) -> Result<Option<Lease>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT lease_id, agent_id, job_id, resource_class, desktop, display,
             cdp_port, vnc_port, profile, cpu_cores, mem_mb, state, start_unix, end_unix,
             heartbeat_due_unix, heartbeat_misses, extensions, last_heartbeat_unix,
             created_unix, started_desktop FROM leases WHERE lease_id=?1",
        )?;
        let mut rows = stmt.query(params![lease_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_lease(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_leases(&self, states: &[&str]) -> Result<Vec<Lease>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let placeholders = states
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT lease_id, agent_id, job_id, resource_class, desktop, display,
             cdp_port, vnc_port, profile, cpu_cores, mem_mb, state, start_unix, end_unix,
             heartbeat_due_unix, heartbeat_misses, extensions, last_heartbeat_unix,
             created_unix, started_desktop
             FROM leases WHERE state IN ({placeholders}) ORDER BY created_unix ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = states
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(&params[..], row_to_lease)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Record one measured-usage row. INSERT OR IGNORE: one row per
    /// lease, ever. Returns true when a row was inserted, false when
    /// a row for this lease already exists.
    pub fn record_lease_usage(&self, row: &LeaseUsage) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO lease_usage(lease_id, agent_id, job_id, resource_class,
             peak_rss_mb, cpu_time_s, duration_s, source, recorded_unix)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                row.lease_id,
                row.agent_id,
                row.job_id,
                row.resource_class,
                row.peak_rss_mb,
                row.cpu_time_s,
                row.duration_s,
                row.source,
                row.recorded_unix,
            ],
        )?;
        Ok(n > 0)
    }

    pub fn get_lease_usage(&self, lease_id: &str) -> Result<Option<LeaseUsage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT lease_id, agent_id, job_id, resource_class, peak_rss_mb,
             cpu_time_s, duration_s, source, recorded_unix
             FROM lease_usage WHERE lease_id=?1",
        )?;
        let mut rows = stmt.query(params![lease_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_usage(row)?))
        } else {
            Ok(None)
        }
    }

    /// Newest usage rows first, for verification and for Step 2.
    pub fn list_lease_usage(&self, limit: i64) -> Result<Vec<LeaseUsage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT lease_id, agent_id, job_id, resource_class, peak_rss_mb,
             cpu_time_s, duration_s, source, recorded_unix
             FROM lease_usage ORDER BY recorded_unix DESC, lease_id ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_usage)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_lease_state_tx(tx: &Transaction<'_>, lease_id: &str, state: &str) -> Result<()> {
        tx.execute(
            "UPDATE leases SET state=?1 WHERE lease_id=?2",
            params![state, lease_id],
        )?;
        Ok(())
    }

    /// Record whether MO started the desktop for this lease. Set in the
    /// same transaction as the phase-3 commit to active.
    pub fn update_lease_started_tx(
        tx: &Transaction<'_>,
        lease_id: &str,
        started: i64,
    ) -> Result<()> {
        tx.execute(
            "UPDATE leases SET started_desktop=?1 WHERE lease_id=?2",
            params![started, lease_id],
        )?;
        Ok(())
    }

    pub fn update_lease_tx(tx: &Transaction<'_>, lease: &Lease) -> Result<()> {
        tx.execute(
            "UPDATE leases SET state=?1, end_unix=?2, heartbeat_due_unix=?3,
             heartbeat_misses=?4, extensions=?5, last_heartbeat_unix=?6
             WHERE lease_id=?7",
            params![
                lease.state,
                lease.end_unix,
                lease.heartbeat_due_unix,
                lease.heartbeat_misses,
                lease.extensions,
                lease.last_heartbeat_unix,
                lease.lease_id,
            ],
        )?;
        Ok(())
    }

    pub fn insert_reservation_tx(
        tx: &Transaction<'_>,
        lease_id: &str,
        desktop: &str,
        cpu_cores: i64,
        mem_mb: i64,
        bridge_slot: i64,
        now: i64,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO reservations(lease_id, desktop, cpu_cores, mem_mb, bridge_slot, created_unix)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![lease_id, desktop, cpu_cores, mem_mb, bridge_slot, now],
        )?;
        Ok(())
    }

    pub fn delete_reservations_tx(tx: &Transaction<'_>, lease_id: &str) -> Result<()> {
        tx.execute(
            "DELETE FROM reservations WHERE lease_id=?1",
            params![lease_id],
        )?;
        Ok(())
    }

    // NOTE: the heartbeats table is kept for history and for the v1
    // migration backfill, but nothing writes it on the hot path any
    // more. The last beat rides on leases.last_heartbeat_unix (F14).

    pub fn list_audit(&self, limit: i64) -> Result<Vec<AuditEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, event, agent_id, job_id, lease_id, desktop, detail, created_unix
             FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(AuditEvent {
                id: row.get(0)?,
                event: row.get(1)?,
                agent_id: row.get(2)?,
                job_id: row.get(3)?,
                lease_id: row.get(4)?,
                desktop: row.get(5)?,
                detail: row.get(6)?,
                created_unix: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn insert_snapshot(&self, inv: &crate::core::Inventory) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let (pct, state) = match &inv.battery {
            Some(b) => (b.pct, Some(b.state.as_str().to_string())),
            None => (None, None),
        };
        conn.execute(
            "INSERT INTO inventory_snapshots(taken_unix, load1, mem_available_mb, mem_total_mb,
             disk_free_gb, chrome_count, desktops_up, bridge_pool_free, battery_pct, battery_state)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                inv.taken_unix,
                inv.load1,
                inv.mem_available_mb,
                inv.mem_total_mb,
                inv.disk_free_gb,
                inv.chrome_count,
                inv.desktops_up,
                inv.bridge_pool_free,
                pct,
                state,
            ],
        )?;
        Ok(())
    }

    /// Backup via the rusqlite backup API. The path never enters SQL text,
    /// so a quote in the path cannot change the statement.
    pub fn backup(&self, dst: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut dst_conn = Connection::open(dst)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst_conn)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(0), None)?;
        Ok(())
    }

    pub fn integrity_check(&self) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let v: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(v)
    }

    /// Sum of reserved CPU/RAM and held desktops across live leases.
    pub fn reserved_totals(&self) -> Result<(i64, i64, Vec<String>)> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(r.cpu_cores),0), COALESCE(SUM(r.mem_mb),0) FROM reservations r
             JOIN leases l ON l.lease_id = r.lease_id
             WHERE l.state IN ('reserving','granted','active','stale','extension_pending','interrupted')",
        )?;
        let (cpu, mem): (i64, i64) = stmt.query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut stmt2 = conn.prepare(
            "SELECT DISTINCT desktop FROM leases
             WHERE state IN ('reserving','granted','active','stale','extension_pending','interrupted')",
        )?;
        let rows = stmt2.query_map([], |r| r.get::<_, String>(0))?;
        let mut desktops = Vec::new();
        for r in rows {
            desktops.push(r?);
        }
        Ok((cpu, mem, desktops))
    }

    /// CPU/RAM held by leases still in "reserving": they have not
    /// consumed the RAM yet, so only these are subtracted from
    /// MemAvailable. Active leases already moved MemAvailable down.
    pub fn reserving_totals(&self) -> Result<(i64, i64)> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let (cpu, mem): (i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(cpu_cores),0), COALESCE(SUM(mem_mb),0)
             FROM leases WHERE state='reserving'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((cpu, mem))
    }

    /// Set a schema_meta key. Used by recovery to record the boot id.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "INSERT INTO schema_meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .ok();
        Ok(v)
    }
}

/// Crash recovery report. V3 section 13.4.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub rolled_back: Vec<String>,
    pub interrupted: Vec<String>,
    pub activated: Vec<String>,
    pub desktops_to_verify: Vec<String>,
}

/// Boot time from /proc/stat btime. None when unreadable.
pub fn boot_unix() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    for line in stat.lines() {
        if let Some(v) = line.strip_prefix("btime ") {
            return v.trim().parse().ok();
        }
    }
    None
}

/// Crash recovery on daemon start. V3 section 13.4, crash points 1-7.
/// `reboot` is true when the host booted since the last daemon run.
/// RESERVING older than 5 min rolls back. GRANTED verifies its desktop:
/// CDP alive activates, else rolls back. On reboot, ACTIVE and GRANTED
/// leases become INTERRUPTED and agents must re-heartbeat within 10 min.
/// Recovery is idempotent: a second run finds nothing to do.
pub fn recover_on_start(store: &Store, now: i64, reboot: bool) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    const STALE_SECS: i64 = 300;
    // Crash point 3/4: RESERVING older than 5 minutes.
    let reserving = store.list_leases(&["reserving"])?;
    for lease in reserving {
        if now - lease.created_unix > STALE_SECS {
            let id = lease.lease_id.clone();
            let desktop = lease.desktop.clone();
            store.with_tx(|tx| {
                Store::delete_reservations_tx(tx, &id)?;
                Store::update_lease_state_tx(tx, &id, "failed")?;
                Store::audit_tx(
                    tx,
                    "recovery_rollback",
                    Some(&lease.agent_id),
                    Some(&lease.job_id),
                    Some(&id),
                    Some(&desktop),
                    "crash during reservation: rolled back",
                    now,
                )?;
                Ok(())
            })?;
            report.rolled_back.push(id);
            if desktop != "none" {
                report.desktops_to_verify.push(desktop);
            }
        }
    }
    // Crash point 5: GRANTED, not yet ACTIVE.
    let granted = store.list_leases(&["granted"])?;
    for lease in granted {
        let id = lease.lease_id.clone();
        let desktop = lease.desktop.clone();
        if reboot {
            // Anchor the 10-minute revive window at the interruption time.
            let mut lc = lease.clone();
            lc.state = "interrupted".to_string();
            lc.last_heartbeat_unix = now;
            store.with_tx(|tx| {
                Store::update_lease_tx(tx, &lc)?;
                Store::audit_tx(
                    tx,
                    "recovery_interrupted",
                    Some(&lease.agent_id),
                    Some(&lease.job_id),
                    Some(&id),
                    Some(&desktop),
                    "host reboot: lease interrupted",
                    now,
                )?;
                Ok(())
            })?;
            report.interrupted.push(id);
            continue;
        }
        let cdp_alive = match lease.cdp_port {
            Some(p) => crate::desktops::cdp_ok(p),
            None => true,
        };
        if cdp_alive && now - lease.created_unix <= STALE_SECS {
            store.with_tx(|tx| {
                Store::update_lease_state_tx(tx, &id, "active")?;
                Store::audit_tx(
                    tx,
                    "recovery_activated",
                    Some(&lease.agent_id),
                    Some(&lease.job_id),
                    Some(&id),
                    Some(&desktop),
                    "desktop verified: lease activated",
                    now,
                )?;
                Ok(())
            })?;
            report.activated.push(id);
        } else {
            store.with_tx(|tx| {
                Store::delete_reservations_tx(tx, &id)?;
                Store::update_lease_state_tx(tx, &id, "failed")?;
                Store::audit_tx(
                    tx,
                    "recovery_rollback",
                    Some(&lease.agent_id),
                    Some(&lease.job_id),
                    Some(&id),
                    Some(&desktop),
                    "granted lease never activated: rolled back",
                    now,
                )?;
                Ok(())
            })?;
            report.rolled_back.push(id);
            if desktop != "none" {
                report.desktops_to_verify.push(desktop);
            }
        }
    }
    // Crash point 7 / reboot: ACTIVE leases become INTERRUPTED on reboot.
    if reboot {
        let active = store.list_leases(&["active", "stale", "extension_pending"])?;
        for lease in active {
            let id = lease.lease_id.clone();
            let desktop = lease.desktop.clone();
            let mut lc = lease.clone();
            lc.state = "interrupted".to_string();
            lc.last_heartbeat_unix = now;
            store.with_tx(|tx| {
                Store::update_lease_tx(tx, &lc)?;
                Store::audit_tx(
                    tx,
                    "recovery_interrupted",
                    Some(&lease.agent_id),
                    Some(&lease.job_id),
                    Some(&id),
                    Some(&desktop),
                    "host reboot: lease interrupted, agent must re-heartbeat in 10 min",
                    now,
                )?;
                Ok(())
            })?;
            report.interrupted.push(id);
        }
    }
    // Record this boot so the next start can tell reboot from restart.
    if let Some(btime) = boot_unix() {
        store.set_meta("last_boot_unix", &btime.to_string())?;
    }
    Ok(report)
}

/// True when the host booted since the last daemon run.
pub fn rebooted_since_last_run(store: &Store) -> bool {
    match (boot_unix(), store.get_meta("last_boot_unix")) {
        (Some(btime), Ok(Some(prev))) => prev != btime.to_string(),
        (Some(_), Ok(None)) => false,
        _ => false,
    }
}

fn row_to_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedRequest> {
    Ok(QueuedRequest {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        job_id: row.get(2)?,
        idempotency_key: row.get(3)?,
        resource_class: row.get(4)?,
        cpu_cores: row.get(5)?,
        mem_mb: row.get(6)?,
        desktops: row.get(7)?,
        duration_min: row.get(8)?,
        priority_class: row.get(9)?,
        deadline: row.get(10)?,
        needs_bridge: row.get::<_, i64>(11)? != 0,
        needs_display: row.get::<_, i64>(12)? != 0,
        mem_mb_max: row.get(13)?,
        lease_id: row.get(14)?,
        capabilities: {
            let caps_json: String = row.get(15)?;
            serde_json::from_str(&caps_json).unwrap_or_default()
        },
        created_unix: row.get(16)?,
        status: row.get(17)?,
    })
}

fn row_to_usage(row: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseUsage> {
    Ok(LeaseUsage {
        lease_id: row.get(0)?,
        agent_id: row.get(1)?,
        job_id: row.get(2)?,
        resource_class: row.get(3)?,
        peak_rss_mb: row.get(4)?,
        cpu_time_s: row.get(5)?,
        duration_s: row.get(6)?,
        source: row.get(7)?,
        recorded_unix: row.get(8)?,
    })
}

fn row_to_lease(row: &rusqlite::Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        lease_id: row.get(0)?,
        agent_id: row.get(1)?,
        job_id: row.get(2)?,
        resource_class: row.get(3)?,
        desktop: row.get(4)?,
        display: row.get(5)?,
        cdp_port: row.get(6)?,
        vnc_port: row.get(7)?,
        profile: row.get(8)?,
        cpu_cores: row.get(9)?,
        mem_mb: row.get(10)?,
        state: row.get(11)?,
        start_unix: row.get(12)?,
        end_unix: row.get(13)?,
        heartbeat_due_unix: row.get(14)?,
        heartbeat_misses: row.get(15)?,
        extensions: row.get(16)?,
        last_heartbeat_unix: row.get(17)?,
        created_unix: row.get(18)?,
        started_desktop: row.get(19)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{now_unix, LeaseRequest};

    fn tmp_path() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("/tmp/mo-test-{nanos}.db")
    }

    fn sample_request() -> QueuedRequest {
        QueuedRequest {
            id: 0,
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 512,
            desktops: 1,
            duration_min: 30,
            priority_class: "P1".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            mem_mb_max: None,
            lease_id: None,
            capabilities: vec![],
            created_unix: now_unix(),
            status: "queued".to_string(),
        }
    }

    fn sample_lease(id: &str, desktop: &str, state: &str) -> Lease {
        let now = now_unix();
        Lease {
            lease_id: id.to_string(),
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            resource_class: "vd-chrome".to_string(),
            desktop: desktop.to_string(),
            display: ":2".to_string(),
            cdp_port: Some(9333),
            vnc_port: Some(5903),
            profile: None,
            cpu_cores: 1,
            mem_mb: 512,
            state: state.to_string(),
            start_unix: now,
            end_unix: now + 1800,
            heartbeat_due_unix: now + 300,
            heartbeat_misses: 0,
            extensions: 0,
            last_heartbeat_unix: 0,
            created_unix: now,
            started_desktop: 0,
        }
    }

    #[test]
    fn migrate_v1_to_v2_opens_old_db_and_fixes_index() {
        let path = tmp_path();
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE schema_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta VALUES('schema_version','1');
                 CREATE TABLE requests(id INTEGER PRIMARY KEY AUTOINCREMENT,
                   agent_id TEXT, job_id TEXT, idempotency_key TEXT, resource_class TEXT,
                   cpu_cores INTEGER, mem_mb INTEGER, desktops INTEGER, duration_min INTEGER,
                   priority_class TEXT, deadline TEXT, needs_bridge INTEGER, needs_display INTEGER,
                   capabilities_json TEXT NOT NULL DEFAULT '[]', created_unix INTEGER,
                   status TEXT NOT NULL DEFAULT 'queued');
                 CREATE TABLE leases(lease_id TEXT PRIMARY KEY, agent_id TEXT, job_id TEXT,
                   resource_class TEXT, desktop TEXT, display TEXT, cdp_port INTEGER,
                   vnc_port INTEGER, profile TEXT, cpu_cores INTEGER, mem_mb INTEGER,
                   state TEXT, start_unix INTEGER, end_unix INTEGER, heartbeat_due_unix INTEGER,
                   heartbeat_misses INTEGER, extensions INTEGER, created_unix INTEGER);
                 CREATE UNIQUE INDEX idx_leases_desktop_active ON leases(desktop)
                   WHERE state IN ('reserving','granted','active','stale','extension_pending');
                 CREATE TABLE heartbeats(id INTEGER PRIMARY KEY AUTOINCREMENT,
                   lease_id TEXT, beat_unix INTEGER);
                 CREATE TABLE audit(id INTEGER PRIMARY KEY AUTOINCREMENT, event TEXT,
                   agent_id TEXT, job_id TEXT, lease_id TEXT, desktop TEXT, detail TEXT,
                   created_unix INTEGER);
                 CREATE TABLE reservations(id INTEGER PRIMARY KEY AUTOINCREMENT,
                   lease_id TEXT, desktop TEXT, cpu_cores INTEGER, mem_mb INTEGER,
                   bridge_slot INTEGER, created_unix INTEGER);
                 CREATE TABLE inventory_snapshots(id INTEGER PRIMARY KEY AUTOINCREMENT,
                   taken_unix INTEGER, mem_available_mb INTEGER, mem_total_mb INTEGER,
                   load1 REAL, disk_free_gb REAL, chrome_count INTEGER, desktops_up INTEGER,
                   bridge_pool_free INTEGER, bridge_known INTEGER, battery_pct INTEGER,
                   battery_state TEXT);
                 INSERT INTO leases VALUES('mo-old-1','a','j','cpu-ram','none',':1',NULL,
                   NULL,NULL,1,256,'active',1000,2800,1300,0,0,1000);
                 INSERT INTO heartbeats(lease_id, beat_unix) VALUES('mo-old-1',1200);",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let c = Connection::open(&path).unwrap();
        let v: String = c
            .query_row(
                "SELECT value FROM schema_meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // v1 -> v2 -> v3 -> v4 chain: version lands on 4, and the migrated
        // row gets the safe started_desktop default 0.
        assert_eq!(v, "4");
        let old = s.get_lease("mo-old-1").unwrap().unwrap();
        assert_eq!(old.last_heartbeat_unix, 1200);
        assert_eq!(old.started_desktop, 0);
        // DEFECT-001: two cpu-ram leases (desktop 'none') must coexist now.
        let now = now_unix();
        for id in ["mo-new-1", "mo-new-2"] {
            let l = sample_lease(id, "none", "active");
            let _ = now;
            s.with_tx(|tx| {
                Store::insert_lease_tx(tx, &l)?;
                Ok(())
            })
            .unwrap();
        }
        // Flagged-desktops table exists.
        s.flag_desktop("vd1", "stop failed", now_unix()).unwrap();
        assert!(s.is_flagged("vd1").unwrap());
        assert!(s.clear_flag("vd1").unwrap());
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn integrity_ok() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.integrity_check().unwrap(), "ok");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn idempotency_unique_index_blocks_duplicate() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let r = sample_request();
        s.insert_request(&r).unwrap();
        assert!(s.insert_request(&r).is_err());
        let found = s
            .find_request_by_idempotency("scout", &r.idempotency_key)
            .unwrap()
            .unwrap();
        assert_eq!(found.job_id, "j1");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn desktop_unique_index_blocks_double_grant() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let l1 = sample_lease("mo-1-a", "vd1", "active");
        let l2 = sample_lease("mo-2-b", "vd1", "active");
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &l1)?;
            Ok(())
        })
        .unwrap();
        let dup = s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &l2)?;
            Ok(())
        });
        assert!(dup.is_err(), "second active lease on vd1 must fail");
        s.with_tx(|tx| {
            Store::update_lease_state_tx(tx, "mo-1-a", "released")?;
            Store::insert_lease_tx(tx, &l2)?;
            Ok(())
        })
        .unwrap();
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn audit_rows_round_trip() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        s.audit(
            "grant",
            Some("scout"),
            Some("j1"),
            Some("mo-1-a"),
            Some("vd1"),
            "ok",
            now_unix(),
        )
        .unwrap();
        let rows = s.list_audit(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, "grant");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn newer_schema_version_refuses_start() {
        let p = tmp_path();
        {
            let s = Store::open(&p).unwrap();
            drop(s);
        }
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute(
                "UPDATE schema_meta SET value='99' WHERE key='schema_version'",
                [],
            )
            .unwrap();
        }
        let err = match Store::open(&p) {
            Ok(_) => panic!("open with newer schema must fail"),
            Err(e) => e,
        };
        assert_eq!(err.code, "E10");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn lease_usage_round_trip_and_one_row_per_lease() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let row = LeaseUsage {
            lease_id: "mo-u-1".to_string(),
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            resource_class: "vd-chrome".to_string(),
            peak_rss_mb: Some(812),
            cpu_time_s: Some(12.5),
            duration_s: 120,
            source: "daemon".to_string(),
            recorded_unix: now_unix(),
        };
        assert!(s.record_lease_usage(&row).unwrap());
        // Second insert for the same lease is a no-op.
        assert!(!s.record_lease_usage(&row).unwrap());
        let got = s.get_lease_usage("mo-u-1").unwrap().unwrap();
        assert_eq!(got.peak_rss_mb, Some(812));
        assert_eq!(got.cpu_time_s, Some(12.5));
        assert_eq!(got.source, "daemon");
        assert_eq!(got.duration_s, 120);
        // Nulls round-trip for an unmeasured lease.
        let none_row = LeaseUsage {
            lease_id: "mo-u-2".to_string(),
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            resource_class: "cpu-ram".to_string(),
            peak_rss_mb: None,
            cpu_time_s: None,
            duration_s: 60,
            source: "none".to_string(),
            recorded_unix: now_unix(),
        };
        assert!(s.record_lease_usage(&none_row).unwrap());
        let got2 = s.get_lease_usage("mo-u-2").unwrap().unwrap();
        assert_eq!(got2.peak_rss_mb, None);
        assert_eq!(got2.source, "none");
        assert_eq!(s.list_lease_usage(10).unwrap().len(), 2);
        assert!(s.get_lease_usage("mo-nope").unwrap().is_none());
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn migrate_v3_to_v4_keeps_history_and_adds_usage() {
        // Build a v3 database by hand: schema_meta says 3, the
        // leases table is the v3 shape, one lease row, no
        // lease_usage table. Open must migrate to 4, keep the
        // lease, and make lease_usage writable.
        let p = tmp_path();
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "CREATE TABLE schema_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta VALUES('schema_version','3');
                 CREATE TABLE leases(lease_id TEXT PRIMARY KEY, agent_id TEXT, job_id TEXT,
                   resource_class TEXT, desktop TEXT, display TEXT, cdp_port INTEGER,
                   vnc_port INTEGER, profile TEXT, cpu_cores INTEGER, mem_mb INTEGER,
                   state TEXT, start_unix INTEGER, end_unix INTEGER, heartbeat_due_unix INTEGER,
                   heartbeat_misses INTEGER, extensions INTEGER, last_heartbeat_unix INTEGER,
                   created_unix INTEGER, started_desktop INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO leases VALUES('mo-old-3','a','j','vd-chrome','vd1',':2',9333,
                   5903,'/tmp/chrome-vd1',1,512,'released',1000,2800,1300,0,0,0,1000,0);",
            )
            .unwrap();
        }
        let s = Store::open(&p).unwrap();
        let c = Connection::open(&p).unwrap();
        let v: String = c
            .query_row(
                "SELECT value FROM schema_meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "4");
        // The v3 lease row survived the migration.
        let old = s.get_lease("mo-old-3").unwrap().unwrap();
        assert_eq!(old.state, "released");
        assert_eq!(old.started_desktop, 0);
        // lease_usage exists and accepts a row.
        let row = LeaseUsage {
            lease_id: "mo-old-3".to_string(),
            agent_id: "a".to_string(),
            job_id: "j".to_string(),
            resource_class: "vd-chrome".to_string(),
            peak_rss_mb: Some(700),
            cpu_time_s: Some(9.0),
            duration_s: 1800,
            source: "daemon".to_string(),
            recorded_unix: now_unix(),
        };
        assert!(s.record_lease_usage(&row).unwrap());
        // Re-opening a v4 DB is a no-op: version stays 4.
        drop(s);
        let s2 = Store::open(&p).unwrap();
        assert!(s2.get_lease_usage("mo-old-3").unwrap().is_some());
        drop(s2);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn request_type_is_referenced() {
        let r = LeaseRequest::default();
        assert_eq!(r.duration_min, 30);
    }

    #[test]
    fn recovery_rolls_back_stale_reserving() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let now = now_unix();
        let old = sample_lease("mo-r-1", "vd1", "reserving");
        let mut old = old;
        old.created_unix = now - 600;
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &old)?;
            Store::insert_reservation_tx(tx, "mo-r-1", "vd1", 1, 512, 0, now - 600)?;
            Ok(())
        })
        .unwrap();
        // Fresh RESERVING (under 5 min) is left alone.
        let fresh = sample_lease("mo-r-2", "vd2", "reserving");
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &fresh)?;
            Ok(())
        })
        .unwrap();
        let report = recover_on_start(&s, now, false).unwrap();
        assert_eq!(report.rolled_back, vec!["mo-r-1".to_string()]);
        assert_eq!(s.get_lease("mo-r-1").unwrap().unwrap().state, "failed");
        assert_eq!(s.get_lease("mo-r-2").unwrap().unwrap().state, "reserving");
        // Idempotent: second run finds nothing.
        let report2 = recover_on_start(&s, now, false).unwrap();
        assert!(report2.rolled_back.is_empty());
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn recovery_marks_interrupted_on_reboot() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let now = now_unix();
        let active = sample_lease("mo-r-3", "vd1", "active");
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &active)?;
            Ok(())
        })
        .unwrap();
        let report = recover_on_start(&s, now, true).unwrap();
        assert_eq!(report.interrupted, vec!["mo-r-3".to_string()]);
        assert_eq!(s.get_lease("mo-r-3").unwrap().unwrap().state, "interrupted");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn recovery_keeps_active_on_daemon_restart() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let now = now_unix();
        let active = sample_lease("mo-r-4", "vd1", "active");
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &active)?;
            Ok(())
        })
        .unwrap();
        let report = recover_on_start(&s, now, false).unwrap();
        assert!(report.interrupted.is_empty());
        assert_eq!(s.get_lease("mo-r-4").unwrap().unwrap().state, "active");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn granted_with_dead_cdp_rolls_back() {
        let p = tmp_path();
        let s = Store::open(&p).unwrap();
        let now = now_unix();
        let mut g = sample_lease("mo-r-5", "vd1", "granted");
        g.cdp_port = Some(59999);
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &g)?;
            Ok(())
        })
        .unwrap();
        let report = recover_on_start(&s, now, false).unwrap();
        assert_eq!(report.rolled_back, vec!["mo-r-5".to_string()]);
        assert_eq!(s.get_lease("mo-r-5").unwrap().unwrap().state, "failed");
        drop(s);
        let _ = std::fs::remove_file(p);
    }
}
