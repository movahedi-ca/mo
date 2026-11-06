//! SQLite store. V3 section 5.2 and 6.3.
//! One file, WAL mode, synchronous FULL, busy_timeout 5000, foreign_keys ON.
//! Schema version 1. One transaction covers queue, lease, reservation, audit.
//! Store owns all durable state. No module writes state directly.

use crate::core::{AuditEvent, Lease, MoError, QueuedRequest, Result};
use rusqlite::{params, Connection, Transaction};
use std::sync::Mutex;

pub const SCHEMA_VERSION: i64 = 1;

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
  created_unix INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'queued'
);
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
  created_unix INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_leases_desktop_active
  ON leases(desktop)
  WHERE state IN ('reserving','granted','active','stale','extension_pending');
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
";

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open or create the DB. Refuses to start on a newer schema version: E10.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
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
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
             needs_bridge, needs_display, capabilities_json, created_unix, status)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                "[]",
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
             needs_display, created_unix, status FROM requests
             WHERE agent_id=?1 AND idempotency_key=?2",
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
             needs_display, created_unix, status FROM requests
             WHERE status='queued' ORDER BY id ASC",
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
             heartbeat_due_unix, heartbeat_misses, extensions, created_unix)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
                lease.created_unix,
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
             heartbeat_due_unix, heartbeat_misses, extensions, created_unix
             FROM leases WHERE lease_id=?1",
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
             heartbeat_due_unix, heartbeat_misses, extensions, created_unix
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

    pub fn update_lease_state_tx(tx: &Transaction<'_>, lease_id: &str, state: &str) -> Result<()> {
        tx.execute(
            "UPDATE leases SET state=?1 WHERE lease_id=?2",
            params![state, lease_id],
        )?;
        Ok(())
    }

    pub fn update_lease_tx(tx: &Transaction<'_>, lease: &Lease) -> Result<()> {
        tx.execute(
            "UPDATE leases SET state=?1, end_unix=?2, heartbeat_due_unix=?3,
             heartbeat_misses=?4, extensions=?5 WHERE lease_id=?6",
            params![
                lease.state,
                lease.end_unix,
                lease.heartbeat_due_unix,
                lease.heartbeat_misses,
                lease.extensions,
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

    pub fn insert_heartbeat(&self, lease_id: &str, beat_unix: i64) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        conn.execute(
            "INSERT INTO heartbeats(lease_id, beat_unix) VALUES(?1, ?2)",
            params![lease_id, beat_unix],
        )?;
        Ok(())
    }

    pub fn last_heartbeat(&self, lease_id: &str) -> Result<Option<i64>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let v: Option<i64> = conn
            .query_row(
                "SELECT MAX(beat_unix) FROM heartbeats WHERE lease_id=?1",
                params![lease_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        Ok(v)
    }

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

    /// Backup via VACUUM INTO.
    pub fn backup(&self, dst: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MoError::e10("store lock poisoned"))?;
        let sql = format!("VACUUM INTO '{dst}'");
        conn.execute_batch(&sql)?;
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
             WHERE l.state IN ('reserving','granted','active','stale','extension_pending')",
        )?;
        let (cpu, mem): (i64, i64) = stmt.query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut stmt2 = conn.prepare(
            "SELECT DISTINCT desktop FROM leases
             WHERE state IN ('reserving','granted','active','stale','extension_pending')",
        )?;
        let rows = stmt2.query_map([], |r| r.get::<_, String>(0))?;
        let mut desktops = Vec::new();
        for r in rows {
            desktops.push(r?);
        }
        Ok((cpu, mem, desktops))
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
        created_unix: row.get(13)?,
        status: row.get(14)?,
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
        created_unix: row.get(17)?,
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
            created_unix: now,
        }
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
    fn request_type_is_referenced() {
        let r = LeaseRequest::default();
        assert_eq!(r.duration_min, 30);
    }
}
