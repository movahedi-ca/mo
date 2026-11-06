//! Unix socket server. V3 section 5.2, ADR-003.
//! One writer thread, one SQLite connection, socket mode 600.
//! Protocol: one JSON object per line, one JSON response per line.
//! No TCP listener. No HTTP server.

use crate::core::{
    now_unix, unix_to_iso, Config, Inventory, LeaseRequest, MoError, PriorityClass, Result,
};
use crate::host::HostAdapter;
use crate::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

pub struct Daemon {
    pub store: Arc<Store>,
    pub cfg: Config,
    pub adapter: Arc<dyn HostAdapter + Send + Sync>,
}

fn cpu_total() -> i64 {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as i64)
        .unwrap_or(2)
        .max(1)
}

/// Free capacity for the scheduler, from a fresh inventory read.
pub fn free_capacity(store: &Store, inv: &Inventory) -> Result<crate::sched::FreeCapacity> {
    let (cpu_res, mem_res, held) = store.reserved_totals()?;
    let vd_held = held
        .iter()
        .filter(|d| *d != "main-display" && *d != "none")
        .count() as i64;
    Ok(crate::sched::FreeCapacity {
        cpu_cores: (cpu_total() - cpu_res).max(0),
        mem_mb: (inv.mem_available_mb - mem_res).max(0),
        desktops: (10 - vd_held).max(0),
        bridge_slots: if inv.bridge_known {
            inv.bridge_pool_free
        } else {
            None
        },
    })
}

impl Daemon {
    pub fn new(
        store: Arc<Store>,
        cfg: Config,
        adapter: Arc<dyn HostAdapter + Send + Sync>,
    ) -> Self {
        Self {
            store,
            cfg,
            adapter,
        }
    }

    /// One scheduler pass: grant the first fitting queued request, repeat
    /// while a fitting request exists. Skips are audited, head keeps position.
    pub fn drain_queue(&self) -> Result<()> {
        let now = now_unix();
        loop {
            let queued = self.store.list_queued_requests()?;
            if queued.is_empty() {
                return Ok(());
            }
            let inv = crate::inventory::read_inventory_fresh();
            let free = free_capacity(&self.store, &inv)?;
            let plan = crate::sched::plan(&queued, &free, now);
            for skipped in crate::sched::skipped_before(&plan) {
                let _ = self.store.audit(
                    "skip",
                    Some(&skipped.request.agent_id),
                    Some(&skipped.request.job_id),
                    None,
                    None,
                    &format!(
                        "head-of-line skip: {}",
                        skipped.skip_reason.clone().unwrap_or_default()
                    ),
                    now,
                );
            }
            let Some(entry) = crate::sched::first_fitting(&plan) else {
                return Ok(());
            };
            let req = entry.request.clone();
            // Battery gate before any grant.
            let state = crate::battery::effective_state(inv.battery.as_ref());
            let prio = PriorityClass::parse_str(&req.priority_class).unwrap_or(PriorityClass::P2);
            let needs_vd = req.desktops > 0;
            if let Err(e) = crate::battery::can_new_lease(state, prio, needs_vd) {
                let _ = self.store.audit(
                    "denied",
                    Some(&req.agent_id),
                    Some(&req.job_id),
                    None,
                    None,
                    &format!("battery gate: {}", e.message),
                    now,
                );
                return Ok(());
            }
            let desktop = match crate::lease::pick_desktop(&self.store, &req)? {
                Some(d) => d,
                None => return Ok(()),
            };
            let provision = |d: &str| crate::desktops::provision_desktop(d);
            let stop = |d: &str| crate::desktops::stop_desktop(d);
            match crate::lease::grant(
                &self.store,
                &req,
                &desktop,
                &inv,
                &self.cfg,
                now,
                &provision,
                &stop,
            ) {
                Ok(_) => {
                    self.store.update_request_status(req.id, "granted")?;
                }
                Err(e) => {
                    let _ = self.store.audit(
                        "denied",
                        Some(&req.agent_id),
                        Some(&req.job_id),
                        None,
                        Some(&desktop),
                        &format!("grant failed: {}", e.message),
                        now,
                    );
                    self.store.update_request_status(req.id, "denied")?;
                }
            }
        }
    }

    fn handle(&self, msg: &Value) -> Value {
        let cmd = msg.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
        let now = now_unix();
        let out: Result<Value> = match cmd {
            "request" => self.cmd_request(msg, now),
            "heartbeat" => self.cmd_heartbeat(msg, now),
            "extend" => self.cmd_extend(msg, now),
            "release" => self.cmd_release(msg, now),
            "status" => self.cmd_status(),
            "queue" => self.cmd_queue(),
            "leases" => self.cmd_leases(),
            "health" => self.cmd_health(),
            "fold" => self.cmd_fold(msg, now),
            "expand" => self.cmd_expand(now),
            _ => Err(MoError::e01(format!("unknown cmd: {cmd}"))),
        };
        match out {
            Ok(v) => v,
            Err(e) => json!({
                "status": "error",
                "error": e.code,
                "message": e.message,
                "next_action": e.next_action,
            }),
        }
    }

    fn cmd_request(&self, msg: &Value, now: i64) -> Result<Value> {
        let req: LeaseRequest =
            serde_json::from_value(msg.get("request").cloned().unwrap_or(Value::Null))?;
        let caps = self.adapter.capabilities();
        let outcome = crate::queue::submit(&self.store, &req, &caps, now)?;
        if outcome.duplicate {
            return Ok(json!({
                "status": "duplicate",
                "request_id": outcome.request_id,
                "queue_position": outcome.queue_position,
            }));
        }
        // Try to grant right away.
        self.drain_queue()?;
        let queued = self.store.list_queued_requests()?;
        let still_queued = queued.iter().any(|r| r.id == outcome.request_id);
        if still_queued {
            let next_slot = unix_to_iso(now + 1800, -5);
            return Ok(json!({
                "status": "queued",
                "reason": "E04",
                "message": "No free slot right now",
                "next_slot": format!("{next_slot} ESTIMATE"),
                "queue_position": outcome.queue_position,
            }));
        }
        let leases = self.store.list_leases(&["active"])?;
        let mine = leases
            .iter()
            .find(|l| l.agent_id == req.agent_id && l.job_id == req.job_id);
        match mine {
            Some(l) => Ok(json!({
                "lease_id": l.lease_id,
                "desktop": l.desktop,
                "display": l.display,
                "cdp_port": l.cdp_port,
                "vnc_port": l.vnc_port,
                "profile": l.profile,
                "start": unix_to_iso(l.start_unix, -5),
                "end": unix_to_iso(l.end_unix, -5),
                "heartbeat_due": unix_to_iso(l.heartbeat_due_unix, -5),
                "status": "granted",
            })),
            None => Ok(json!({
                "status": "queued",
                "reason": "E04",
                "message": "Grant did not complete",
                "queue_position": outcome.queue_position,
            })),
        }
    }

    fn cmd_heartbeat(&self, msg: &Value, now: i64) -> Result<Value> {
        let lease_id = msg
            .get("lease_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("lease_id is required"))?;
        let lease = crate::lease::heartbeat(&self.store, lease_id, now, &self.cfg)?;
        Ok(json!({
            "lease_id": lease.lease_id,
            "state": lease.state,
            "heartbeat_due": unix_to_iso(lease.heartbeat_due_unix, -5),
            "status": "ok",
        }))
    }

    fn cmd_extend(&self, msg: &Value, now: i64) -> Result<Value> {
        let lease_id = msg
            .get("lease_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("lease_id is required"))?;
        let minutes = msg
            .get("minutes")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| MoError::e01("minutes is required"))?;
        let inv = crate::inventory::read_inventory_fresh();
        let state = crate::battery::effective_state(inv.battery.as_ref());
        crate::battery::can_extend(state)?;
        // Compatible-queue rule: deny when a queued request needs the desktop.
        let lease = self
            .store
            .get_lease(lease_id)?
            .ok_or_else(|| MoError::e08(format!("lease not found: {lease_id}")))?;
        let conflict = self
            .store
            .list_queued_requests()?
            .iter()
            .any(|r| r.desktops > 0 && lease.desktop != "none");
        let lease = crate::lease::extend(&self.store, lease_id, minutes, &self.cfg, conflict, now)?;
        Ok(json!({
            "lease_id": lease.lease_id,
            "end": unix_to_iso(lease.end_unix, -5),
            "extensions": lease.extensions,
            "status": "extended",
        }))
    }

    fn cmd_release(&self, msg: &Value, now: i64) -> Result<Value> {
        let lease_id = msg
            .get("lease_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("lease_id is required"))?;
        let released = crate::lease::release(&self.store, lease_id, now)?;
        Ok(json!({
            "lease_id": lease_id,
            "released": released,
            "status": "ok",
        }))
    }

    fn cmd_status(&self) -> Result<Value> {
        let inv = crate::inventory::read_inventory();
        Ok(crate::notify::snapshot(&self.store, &inv, &self.cfg))
    }

    fn cmd_queue(&self) -> Result<Value> {
        let queued = self.store.list_queued_requests()?;
        Ok(json!({ "queue": queued, "depth": queued.len() }))
    }

    fn cmd_leases(&self) -> Result<Value> {
        let leases = self.store.list_leases(&[
            "active",
            "granted",
            "reserving",
            "stale",
            "extension_pending",
        ])?;
        Ok(json!({ "leases": leases, "count": leases.len() }))
    }

    fn cmd_health(&self) -> Result<Value> {
        let integrity = self.store.integrity_check()?;
        let inv = crate::inventory::read_inventory();
        let battery_state = match &inv.battery {
            Some(b) => crate::battery::effective_state(Some(b))
                .as_str()
                .to_string(),
            None => "Unknown".to_string(),
        };
        Ok(json!({
            "daemon": "up",
            "db": integrity,
            "lock": "held",
            "battery_state": battery_state,
            "queue_depth": self.store.list_queued_requests()?.len(),
            "active_leases": self.store.list_leases(&["active"])?.len(),
        }))
    }

    fn cmd_fold(&self, msg: &Value, now: i64) -> Result<Value> {
        let desktop = msg
            .get("desktop")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("desktop is required"))?;
        crate::desktops::fold_desktop(&self.store, desktop, now, &self.cfg)?;
        Ok(json!({ "desktop": desktop, "status": "folded" }))
    }

    fn cmd_expand(&self, now: i64) -> Result<Value> {
        let inv = crate::inventory::read_inventory_fresh();
        let depth = self.store.list_queued_requests()?.len() as i64;
        let (_, _, held) = self.store.reserved_totals()?;
        let active_vd = held
            .iter()
            .filter(|d| *d != "main-display" && *d != "none")
            .count() as i64;
        if !crate::desktops::should_expand(depth, &inv, &self.cfg, active_vd) {
            return Ok(json!({ "status": "no-expand", "queue_depth": depth }));
        }
        let started = crate::desktops::expand_one(&self.store, now)?;
        Ok(json!({ "status": "expanded", "desktop": started }))
    }

    /// Accept loop. One connection at a time, one writer.
    pub fn serve(&self, socket_path: &str) -> Result<()> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)?;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let _ = self.serve_one(stream);
                }
                Err(e) => {
                    eprintln!("mo-daemon: accept error: {e}");
                }
            }
        }
        Ok(())
    }

    fn serve_one(&self, stream: UnixStream) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            return Ok(());
        }
        let msg: Value = serde_json::from_str(&line)?;
        let resp = self.handle(&msg);
        let mut writer = stream;
        writer.write_all(serde_json::to_string(&resp)?.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}

/// One-shot client call used by mo-cli. Sends one JSON line, reads one line.
pub fn call(socket_path: &str, msg: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| MoError::e10(format!("cannot connect to mo-daemon socket: {e}")))?;
    stream.write_all(serde_json::to_string(msg)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(MoError::e10("empty response from mo-daemon"));
    }
    Ok(serde_json::from_str(&line)?)
}

/// Default socket path from env, same default as Config.
pub fn default_socket_path() -> String {
    std::env::var("MO_SOCKET_PATH").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/auto".to_string());
        format!("{home}/jobs/mo/mo.sock")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ChromebookAdapter;

    fn tmp_cfg() -> (Config, String, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = format!("/tmp/mo-srv-{nanos}");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::from_env();
        cfg.db_path = format!("{dir}/mo.db");
        cfg.socket_path = format!("{dir}/mo.sock");
        cfg.lock_path = format!("{dir}/mo.lock");
        cfg.dashboard_path = format!("{dir}/dashboard.html");
        // This host runs near the 1200 MB RAM guard. Guards are unit
        // tested in lease.rs; the socket test must not flake on them.
        cfg.ram_guard_mb = 0;
        cfg.disk_guard_gb = 0;
        let sock = format!("{dir}/mo.sock");
        (cfg, dir, sock)
    }

    fn request_json() -> Value {
        json!({
            "cmd": "request",
            "request": {
                "agent_id": "tester",
                "job_id": "t1",
                "idempotency_key": uuid::Uuid::new_v4().to_string(),
                "resource_class": "cpu-ram",
                "cpu_cores": 1,
                "mem_mb": 256,
                "desktops": 0,
                "duration_min": 30,
                "priority_class": "P1",
                "needs_bridge": false,
                "needs_display": false,
                "capabilities": []
            }
        })
    }

    #[test]
    fn socket_round_trip_request_to_release() {
        let (cfg, dir, sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Arc::new(Daemon::new(store, cfg.clone(), Arc::new(ChromebookAdapter)));
        let sock2 = sock.clone();
        std::thread::spawn(move || {
            let _ = daemon.serve(&sock2);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let grant = call(&sock, &request_json()).unwrap();
        assert_eq!(grant["status"], "granted", "grant: {grant}");
        let lease_id = grant["lease_id"].as_str().unwrap().to_string();
        let status = call(&sock, &json!({ "cmd": "status" })).unwrap();
        assert_eq!(status["active_leases"], 1);
        let hb = call(&sock, &json!({ "cmd": "heartbeat", "lease_id": lease_id })).unwrap();
        assert_eq!(hb["status"], "ok");
        let ext = call(
            &sock,
            &json!({ "cmd": "extend", "lease_id": lease_id, "minutes": 10 }),
        )
        .unwrap();
        assert_eq!(ext["status"], "extended");
        let rel = call(&sock, &json!({ "cmd": "release", "lease_id": lease_id })).unwrap();
        assert_eq!(rel["released"], true);
        let health = call(&sock, &json!({ "cmd": "health" })).unwrap();
        assert_eq!(health["db"], "ok");
        let fold = call(&sock, &json!({ "cmd": "fold", "desktop": "main-display" })).unwrap();
        assert_eq!(fold["error"], "E14");
        let bad = call(&sock, &json!({ "cmd": "nope" })).unwrap();
        assert_eq!(bad["error"], "E01");
        let _ = std::fs::remove_dir_all(dir);
    }
}
