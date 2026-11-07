//! Unix socket server. V3 section 5.2, ADR-003.
//! One writer thread, one SQLite connection, socket mode 600.
//! Protocol: one JSON object per line, one JSON response per line.
//! No TCP listener. No HTTP server.

use crate::core::{
    now_unix, unix_to_local_iso, Config, Inventory, LeaseRequest, MoError, PriorityClass, Result,
};
use crate::host::HostAdapter;
use crate::store::Store;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

pub struct Daemon {
    pub store: Arc<Store>,
    pub cfg: Config,
    pub adapter: Arc<dyn HostAdapter + Send + Sync>,
    ncpu: i64,
    last_bypass_unix: std::sync::Mutex<i64>,
    last_prune_unix: std::sync::Mutex<i64>,
    /// Single-flight lock for drain_queue. The tick thread, the
    /// startup drain in main, and every request/release handler all
    /// call drain_queue, and two concurrent drains granted the same
    /// queued request twice (live host, 2026-11-06: one vd-chrome
    /// request got two leases, vd1 and vd2, in the same second).
    drain_lock: std::sync::Mutex<()>,
    /// FIX-006/007: per-request drain retry state. Attempts, the last
    /// error code, consecutive identical errors, and the next retry
    /// time. Memory only: a restart resets the backoff, and the queue
    /// itself is the durable state.
    retry_state: std::sync::Mutex<HashMap<i64, DrainRetryState>>,
}

/// FIX-006: drain backoff delays in seconds. Attempt 1 waits 30 s,
/// attempt 2 waits 60 s, attempt 3 waits 120 s, attempt 4 waits 240 s.
/// After the 4th retry (5th failed attempt) the request is marked failed.
pub const DRAIN_BACKOFF_SECS: [i64; 4] = [30, 60, 120, 240];
/// FIX-006: max retries after the first failure.
pub const DRAIN_MAX_RETRIES: i64 = 4;
/// FIX-007: consecutive identical error codes that trip the breaker.
pub const CIRCUIT_BREAKER_THRESHOLD: i64 = 3;

#[derive(Debug, Clone, Default)]
struct DrainRetryState {
    attempts: i64,
    consecutive_same: i64,
    last_code: Option<String>,
    next_retry_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainRetryDecision {
    RetryAt(i64),
    FailCap,
    FailBreaker,
}

/// FIX-006/007: record one failed drain attempt for a request.
/// Returns the decision and the attempt count. The breaker trips
/// first: 3 consecutive identical error codes stop retrying. Then
/// the cap: the 5th failed attempt (initial + 4 retries) fails the
/// request. Otherwise the next retry time follows the backoff
/// schedule, 30 s, 60 s, 120 s, 240 s.
fn record_drain_failure(
    state: &mut DrainRetryState,
    code: &str,
    now: i64,
) -> (DrainRetryDecision, i64) {
    state.attempts += 1;
    if state.last_code.as_deref() == Some(code) {
        state.consecutive_same += 1;
    } else {
        state.consecutive_same = 1;
        state.last_code = Some(code.to_string());
    }
    if state.consecutive_same >= CIRCUIT_BREAKER_THRESHOLD {
        return (DrainRetryDecision::FailBreaker, state.attempts);
    }
    if state.attempts > DRAIN_MAX_RETRIES {
        return (DrainRetryDecision::FailCap, state.attempts);
    }
    let delay = DRAIN_BACKOFF_SECS[(state.attempts - 1) as usize];
    state.next_retry_unix = now + delay;
    (
        DrainRetryDecision::RetryAt(state.next_retry_unix),
        state.attempts,
    )
}

fn cpu_total() -> i64 {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as i64)
        .unwrap_or(2)
        .max(1)
}

/// Free capacity for the scheduler. F06: MemAvailable already fell
/// by the RSS of live leases, so only still-reserving leases are
/// subtracted from it. CPU is the tighter of the reservation sum and
/// the live load: free cores = ncpu - ceil(load1), floored at 0.
/// F12: desktops are min(config max_vd, adapter max_vd) minus held.
/// ncpu is cached on the Daemon at startup, not re-read per check.
pub fn free_capacity(
    store: &Store,
    inv: &Inventory,
    cfg: &Config,
    caps: &crate::core::Capabilities,
    ncpu: i64,
) -> Result<crate::sched::FreeCapacity> {
    let (cpu_res, _, held) = store.reserved_totals()?;
    let (_, mem_reserving) = store.reserving_totals()?;
    let vd_held = held
        .iter()
        .filter(|d| *d != "main-display" && *d != "none")
        .count() as i64;
    let load_cores = inv.load1.ceil() as i64;
    let cpu_by_res = (ncpu - cpu_res).max(0);
    let cpu_by_load = (ncpu - load_cores).max(0);
    Ok(crate::sched::FreeCapacity {
        cpu_cores: cpu_by_res.min(cpu_by_load),
        mem_mb: (inv.mem_available_mb - mem_reserving).max(0),
        desktops: (cfg.max_vd.min(caps.max_vd) - vd_held).max(0),
        bridge_slots: if inv.bridge_known {
            inv.bridge_pool_free
        } else {
            None
        },
    })
}

/// Reason kind for skip coalescing: "mem", "cpu", "desktops", "bridge",
/// or "battery". The full reason embeds live numbers that change per pass.
fn skip_key(reason: &str) -> &str {
    let r = reason.strip_prefix("head-of-line skip: ").unwrap_or(reason);
    let r = r.strip_prefix("battery gate: ").unwrap_or(r);
    r.split_whitespace().next().unwrap_or(r)
}

/// Parent directory of a path, for the statvfs disk guard.
fn db_parent(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string())
}

fn grant_json(l: &crate::core::Lease) -> Value {
    json!({
        "lease_id": l.lease_id,
        "desktop": l.desktop,
        "display": l.display,
        "cdp_port": l.cdp_port,
        "vnc_port": l.vnc_port,
        "profile": l.profile,
        "start": unix_to_local_iso(l.start_unix),
        "end": unix_to_local_iso(l.end_unix),
        "heartbeat_due": unix_to_local_iso(l.heartbeat_due_unix),
        "status": "granted",
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
            ncpu: cpu_total(),
            last_bypass_unix: std::sync::Mutex::new(0),
            last_prune_unix: std::sync::Mutex::new(0),
            drain_lock: std::sync::Mutex::new(()),
            retry_state: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// One scheduler tick, run by the daemon thread every 10 seconds.
    /// F05: capacity coming back is noticed here, not only when a new
    /// request arrives. Expired requests and leases, stale sweeps,
    /// reclaim, and the drain all run in this order, then the drain
    /// grants whatever now fits.
    pub fn tick(&self) -> Result<()> {
        let now = now_unix();
        let _ = crate::queue::expire_stale(&self.store, now);
        let _ = crate::lease::expire_leases(&self.store, now);
        let _ = crate::lease::expire_interrupted(&self.store, now);
        let _ = crate::lease::sweep_stale(&self.store, now, &self.cfg);
        let _ = crate::lease::reclaim_all(&self.store, now, &self.cfg);
        self.drain_queue()?;
        self.check_bypass(now);
        self.prune_daily(now);
        Ok(())
    }

    /// DEFECT-003: a desktop that is up with no holding lease was started
    /// outside MO. Audit it once per desktop per hour so the operator
    /// sees the bypass. Flagged desktops are already on record.
    fn check_bypass(&self, now: i64) {
        let last = *self
            .last_bypass_unix
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if now - last < 60 {
            return;
        }
        *self
            .last_bypass_unix
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = now;
        let held = match self.store.reserved_totals() {
            Ok((_, _, h)) => h,
            Err(_) => return,
        };
        for d in crate::desktops::list_desktops() {
            if !d.up || d.id == "main-display" || held.contains(&d.id) {
                continue;
            }
            if self.store.is_flagged(&d.id).unwrap_or(false) {
                continue;
            }
            let recent = self
                .store
                .last_audit_event("bypass", &d.id, now - 3600)
                .ok()
                .flatten()
                .is_some();
            if recent {
                continue;
            }
            let _ = self.store.audit(
                "bypass",
                None,
                None,
                None,
                Some(&d.id),
                "desktop up with no holding lease: started outside MO",
                now,
            );
        }
    }

    /// Daily retention: prune old rows and checkpoint the WAL.
    fn prune_daily(&self, now: i64) {
        let last = *self
            .last_prune_unix
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if now - last < 86400 {
            return;
        }
        *self
            .last_prune_unix
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = now;
        let _ = self.store.prune(now);
        let _ = self.store.checkpoint();
    }

    /// Audit a skip once per reason kind per request per minute. The same
    /// blocked head used to write one skip row on every drain pass. The
    /// reason text embeds live numbers (free MB), so the key is the first
    /// word after the prefix, not the full string.
    fn audit_skip_coalesced(&self, agent_id: &str, job_id: &str, reason: &str, now: i64) {
        if let Ok(Some(last)) = self.store.last_audit_for(agent_id, job_id) {
            if last.event == "skip"
                && skip_key(&last.detail) == skip_key(reason)
                && last.created_unix > now - 60
            {
                return;
            }
        }
        let _ = self.store.audit(
            "skip",
            Some(agent_id),
            Some(job_id),
            None,
            None,
            reason,
            now,
        );
    }

    /// One scheduler pass: grant the first fitting queued request, repeat
    /// while a fitting request exists. Skips are audited, head keeps position.
    /// F01: a transient grant failure (E04/E05/E06/E07) is a wait, not a
    /// denial. Only an E01/E02 mistake denies the request. F02: only a
    /// Refuse battery state stops the drain; Warn/Restrict skip the one
    /// blocked entry and the drain continues to the next fitting entry.
    pub fn drain_queue(&self) -> Result<()> {
        // Patch 12: single-flight. Without this, two concurrent drains
        // both list the same queued request and both grant it.
        let _guard = self
            .drain_lock
            .lock()
            .map_err(|_| MoError::e10("drain lock poisoned"))?;
        let now = now_unix();
        // Deferred in this pass only: battery-blocked or transiently
        // failed entries. Without this the loop re-grants a waiting
        // request forever, because a wait leaves it queued.
        let mut deferred: HashSet<i64> = HashSet::new();
        loop {
            let mut queued = self.store.list_queued_requests()?;
            queued.retain(|r| !deferred.contains(&r.id));
            // FIX-006: a request inside its backoff window is not
            // attempted again until its next retry time.
            {
                let retry = self
                    .retry_state
                    .lock()
                    .map_err(|_| MoError::e10("retry state lock poisoned"))?;
                queued.retain(|r| {
                    retry
                        .get(&r.id)
                        .map(|s| s.next_retry_unix <= now)
                        .unwrap_or(true)
                });
            }
            if queued.is_empty() {
                return Ok(());
            }
            // F04/F13: one single-flight snapshot for the whole pass,
            // plus a MemAvailable-only re-read for the grant gate. The
            // bridge is probed only when a queued row needs it.
            let needs_bridge = queued.iter().any(|r| r.needs_bridge);
            let db_dir = db_parent(&self.cfg.db_path);
            let mut inv = if needs_bridge {
                crate::inventory::read_inventory_fresh_bridge(&self.cfg, &db_dir)
            } else {
                crate::inventory::read_inventory_fresh_with(&self.cfg, &db_dir)
            };
            inv.mem_available_mb = crate::inventory::fresh_mem_available_mb();
            let caps = self.adapter.capabilities();
            let free = free_capacity(&self.store, &inv, &self.cfg, &caps, self.ncpu)?;
            let plan = crate::sched::plan(&queued, &free, now);
            // FIX-008: every non-fitting entry is a recorded fit skip,
            // not only the ones ahead of the first fitting entry.
            for entry in plan.iter().filter(|e| !e.fits) {
                let reason = format!(
                    "head-of-line skip: {}",
                    entry.skip_reason.clone().unwrap_or_default()
                );
                self.audit_skip_coalesced(
                    &entry.request.agent_id,
                    &entry.request.job_id,
                    &reason,
                    now,
                );
            }
            let Some(entry) = crate::sched::first_fitting(&plan) else {
                return Ok(());
            };
            let req = entry.request.clone();
            // Battery gate before any grant. A host with no battery
            // capability (adapter says so) has no battery gate; the
            // sensor-missing Unknown state must not block its desktops.
            let state = if caps.has_battery {
                crate::battery::effective_state(inv.battery.as_ref())
            } else {
                crate::core::BatteryState::Normal
            };
            let prio = PriorityClass::parse_str(&req.priority_class).unwrap_or(PriorityClass::P2);
            let needs_vd = req.desktops > 0;
            if let Err(e) = crate::battery::can_new_lease(state, prio, needs_vd) {
                let reason = format!("battery gate: {}", e.message);
                self.audit_skip_coalesced(&req.agent_id, &req.job_id, &reason, now);
                if state == crate::core::BatteryState::Refuse {
                    return Ok(());
                }
                deferred.insert(req.id);
                continue;
            }
            let desktops = crate::desktops::list_desktops_cached();
            let desktop = match crate::lease::pick_desktop(
                &self.store,
                &req,
                &self.cfg,
                &caps,
                inv.load1,
                self.ncpu,
                &desktops,
            )? {
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
                Ok(g) => {
                    self.store.update_request_status(req.id, "granted")?;
                    self.store.set_request_lease(req.id, &g.lease_id)?;
                    if let Ok(mut retry) = self.retry_state.lock() {
                        retry.remove(&req.id);
                    }
                    // FIX-008: log each grant with its queue position.
                    let pos = plan
                        .iter()
                        .position(|e| e.request.id == req.id)
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    eprintln!(
                        "mo-daemon: granted request {} queue position {pos} lease {}",
                        req.id, g.lease_id
                    );
                }
                Err(e) => {
                    if e.code == "E01" || e.code == "E02" {
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
                        if let Ok(mut retry) = self.retry_state.lock() {
                            retry.remove(&req.id);
                        }
                    } else {
                        // FIX-006/007: backoff and circuit breaker.
                        let (decision, attempt) = {
                            let mut retry = self
                                .retry_state
                                .lock()
                                .map_err(|_| MoError::e10("retry state lock poisoned"))?;
                            let st = retry.entry(req.id).or_default();
                            record_drain_failure(st, &e.code, now)
                        };
                        match decision {
                            DrainRetryDecision::RetryAt(next) => {
                                eprintln!(
                                    "mo-daemon: drain retry request {} attempt {attempt} code {} next_retry {next}",
                                    req.id, e.code
                                );
                                let _ = self.store.audit(
                                    "wait",
                                    Some(&req.agent_id),
                                    Some(&req.job_id),
                                    None,
                                    Some(&desktop),
                                    &format!(
                                        "{}: grant failed (attempt {attempt}, next retry {next}): {}",
                                        e.code, e.message
                                    ),
                                    now,
                                );
                                deferred.insert(req.id);
                            }
                            DrainRetryDecision::FailCap => {
                                eprintln!(
                                    "mo-daemon: drain cap request {} attempt {attempt} code {}: marked failed",
                                    req.id, e.code
                                );
                                let _ = self.store.audit(
                                    "drain_failed",
                                    Some(&req.agent_id),
                                    Some(&req.job_id),
                                    None,
                                    Some(&desktop),
                                    &format!(
                                        "max retries reached after {attempt} attempts, last error {}: {}",
                                        e.code, e.message
                                    ),
                                    now,
                                );
                                self.store.update_request_status(req.id, "failed")?;
                                if let Ok(mut retry) = self.retry_state.lock() {
                                    retry.remove(&req.id);
                                }
                            }
                            DrainRetryDecision::FailBreaker => {
                                eprintln!(
                                    "mo-daemon: circuit breaker request {} attempt {attempt} code {}: marked failed",
                                    req.id, e.code
                                );
                                let _ = self.store.audit(
                                    "circuit_breaker",
                                    Some(&req.agent_id),
                                    Some(&req.job_id),
                                    None,
                                    Some(&desktop),
                                    &format!(
                                        "circuit breaker tripped after {attempt} identical errors {}: {}",
                                        e.code, e.message
                                    ),
                                    now,
                                );
                                self.store.update_request_status(req.id, "failed")?;
                                if let Ok(mut retry) = self.retry_state.lock() {
                                    retry.remove(&req.id);
                                }
                            }
                        }
                    }
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
            "live" => self.cmd_live(),
            "health" => self.cmd_health(),
            "fold" => self.cmd_fold(msg, now),
            "expand" => self.cmd_expand(now),
            "cancel" => self.cmd_cancel(msg, now),
            "clear-flag" => self.cmd_clear_flag(msg, now),
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
            if let Some(row) = self.store.get_request(outcome.request_id)? {
                if let Some(lease_id) = row.lease_id {
                    if let Some(l) = self.store.get_lease(&lease_id)? {
                        return Ok(grant_json(&l));
                    }
                }
            }
            return Ok(json!({
                "status": "duplicate",
                "request_id": outcome.request_id,
                "queue_position": outcome.queue_position,
            }));
        }
        // Try to grant right away.
        self.drain_queue()?;
        let row = self.store.get_request(outcome.request_id)?;
        match row {
            Some(r) if r.status == "granted" => {
                if let Some(lease_id) = r.lease_id {
                    if let Some(l) = self.store.get_lease(&lease_id)? {
                        return Ok(grant_json(&l));
                    }
                }
                Ok(json!({
                    "status": "queued",
                    "reason": "E04",
                    "message": "Grant did not complete",
                    "queue_position": outcome.queue_position,
                }))
            }
            _ => {
                let (reason, message) = self.last_wait_reason(&req.agent_id, &req.job_id);
                let next_slot = unix_to_local_iso(now + 1800);
                Ok(json!({
                    "status": "queued",
                    "reason": reason,
                    "message": message,
                    "next_slot": format!("{next_slot} ESTIMATE"),
                    "queue_position": outcome.queue_position,
                }))
            }
        }
    }

    /// Real wait reason for a queued request, from its latest audit row.
    /// Falls back to the generic E04 when no audit row exists.
    fn last_wait_reason(&self, agent_id: &str, job_id: &str) -> (String, String) {
        if let Ok(Some(a)) = self.store.last_audit_for(agent_id, job_id) {
            let d = a.detail;
            if d.len() >= 3
                && d.starts_with('E')
                && d.as_bytes()[1].is_ascii_digit()
                && d.as_bytes()[2].is_ascii_digit()
            {
                return (d[..3].to_string(), d.get(5..).unwrap_or("").to_string());
            }
            if d.starts_with("battery gate") {
                return ("E05".to_string(), d);
            }
            if d.starts_with("head-of-line skip") {
                return ("E04".to_string(), d);
            }
            return ("E04".to_string(), d);
        }
        ("E04".to_string(), "No free slot right now".to_string())
    }

    /// OBS-006: cancel a queued request by agent + idempotency key.
    fn cmd_cancel(&self, msg: &Value, now: i64) -> Result<Value> {
        let agent_id = msg
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("agent_id is required"))?;
        let key = msg
            .get("idempotency_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("idempotency_key is required"))?;
        let cancelled = crate::queue::cancel(&self.store, agent_id, key, now, &|d: &str| {
            crate::desktops::stop_desktop(d)
        })?;
        Ok(json!({ "cancelled": cancelled, "status": "ok" }))
    }

    /// Clear a flagged desktop so fold can run on it again.
    fn cmd_clear_flag(&self, msg: &Value, now: i64) -> Result<Value> {
        let _ = now;
        let desktop = msg
            .get("desktop")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("desktop is required"))?;
        let cleared = self.store.clear_flag(desktop)?;
        Ok(json!({ "cleared": cleared, "status": "ok" }))
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
            "heartbeat_due": unix_to_local_iso(lease.heartbeat_due_unix),
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
        let mut inv = crate::inventory::read_inventory();
        inv.mem_available_mb = crate::inventory::fresh_mem_available_mb();
        let state = crate::battery::effective_state(inv.battery.as_ref());
        crate::battery::can_extend(state)?;
        // F09: an extension conflicts only when a queued request
        // cannot fit now but would fit if this lease went away. A
        // queued desktop request that fits anyway, or one blocked on
        // something else entirely, is not a conflict.
        let lease = self
            .store
            .get_lease(lease_id)?
            .ok_or_else(|| MoError::e08(format!("lease not found: {lease_id}")))?;
        let caps = self.adapter.capabilities();
        let free = free_capacity(&self.store, &inv, &self.cfg, &caps, self.ncpu)?;
        let mut freed = free;
        freed.cpu_cores += lease.cpu_cores;
        freed.mem_mb += lease.mem_mb;
        if lease.desktop != "none" && lease.desktop != "main-display" {
            freed.desktops += 1;
        }
        let conflict = self.store.list_queued_requests()?.iter().any(|r| {
            crate::sched::fits(r, &free).is_some() && crate::sched::fits(r, &freed).is_none()
        });
        let lease = crate::lease::extend(&self.store, lease_id, minutes, &self.cfg, conflict, now)?;
        Ok(json!({
            "lease_id": lease.lease_id,
            "end": unix_to_local_iso(lease.end_unix),
            "extensions": lease.extensions,
            "status": "extended",
        }))
    }

    fn cmd_release(&self, msg: &Value, now: i64) -> Result<Value> {
        let lease_id = msg
            .get("lease_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MoError::e01("lease_id is required"))?;
        let stop = |d: &str| crate::desktops::stop_desktop(d);
        let released = crate::lease::release(&self.store, lease_id, now, &stop)?;
        // F05: a release frees capacity, so drain right away. A drain
        // error must not turn a good release into an error response.
        if released {
            let _ = self.drain_queue();
        }
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

    /// One consistent snapshot for the live dashboard app: status +
    /// queue + leases + desktops in a single call, so the app never
    /// renders a half-old frame. Read only. No state changes.
    fn cmd_live(&self) -> Result<Value> {
        let inv = crate::inventory::read_inventory();
        let status = crate::notify::snapshot(&self.store, &inv, &self.cfg);
        let queued = self.store.list_queued_requests()?;
        let leases = self.store.list_leases(&[
            "active",
            "granted",
            "reserving",
            "stale",
            "extension_pending",
        ])?;
        let desktops = crate::desktops::list_desktops_cached();
        Ok(json!({
            "status": status,
            "queue": queued,
            "leases": leases,
            "desktops": desktops,
        }))
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
        // F12: expand is a desktop operation. A host without desktops
        // refuses it, and a low battery blocks a new vd start.
        let caps = self.adapter.capabilities();
        if !caps.has_desktops {
            return Err(MoError::e02("this host has no desktops"));
        }
        let mut inv = crate::inventory::read_inventory();
        inv.mem_available_mb = crate::inventory::fresh_mem_available_mb();
        if caps.has_battery {
            let state = crate::battery::effective_state(inv.battery.as_ref());
            crate::battery::can_start_vd(state)?;
        }
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

    /// Accept loop. One thread per connection: a client that connects
    /// and sends nothing times out after 2 s on its own thread and never
    /// blocks heartbeats or releases.
    pub fn serve(self: &Arc<Self>, socket_path: &str) -> Result<()> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .map_err(|e| MoError::e10(format!("cannot bind socket {socket_path}: {e}")))?;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| MoError::e10(format!("cannot chmod 0600 socket {socket_path}: {e}")))?;
        // FIX-002: the socket is bound before the startup drain runs.
        // The drain runs on its own thread, so a slow drain over
        // failing provisions never blocks the bind or the accept loop.
        // The tick thread drains every 10 s as well; the drain lock
        // keeps the two passes single-flight.
        {
            let daemon = Arc::clone(self);
            std::thread::spawn(move || {
                let _ = daemon.drain_queue();
            });
        }
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let daemon = Arc::clone(self);
                    std::thread::spawn(move || {
                        let _ = daemon.serve_one(stream);
                    });
                }
                Err(e) => {
                    eprintln!("mo-daemon: accept error: {e}");
                }
            }
        }
        Ok(())
    }

    fn serve_one(&self, stream: UnixStream) -> Result<()> {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let line = read_capped_line(&mut reader, 65_536)?;
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

/// Read one line, capped at `max` bytes, with the stream read timeout
/// doing the waiting. A client that sends nothing times out; a client
/// that sends a huge line gets an error instead of growing the buffer
/// without bound. Footgun 1.
fn read_capped_line(reader: &mut impl BufRead, max: usize) -> Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() > max {
                    return Err(MoError::e01(format!("request line over {max} bytes")));
                }
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(MoError::e10("read timeout waiting for request"));
            }
            Err(e) => {
                return Err(MoError::e10(format!("socket read failed: {e}")));
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// One-shot client call used by mo-cli. Sends one JSON line, reads one line.
pub fn call(socket_path: &str, msg: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| MoError::e10(format!("cannot connect to mo-daemon socket: {e}")))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.write_all(serde_json::to_string(msg)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let line = read_capped_line(&mut reader, 65_536)?;
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

    /// Healthy-inventory guard for grant tests. A grant asserts on
    /// capacity, and live load1 spikes while cargo builds in parallel
    /// (8 grant tests flaked this way). The guard fixes the snapshot
    /// and holds the inventory test lock, so no probe test runs
    /// concurrently and sees the override. Clears on drop.
    struct HealthyInv {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HealthyInv {
        fn set() -> Self {
            let lock = crate::inventory::TEST_LOCK.lock().unwrap();
            crate::inventory::set_test_inventory(Some(crate::core::Inventory {
                mem_available_mb: 4096,
                mem_total_mb: 8192,
                load1: 0.5,
                disk_free_gb: 50.0,
                ..Default::default()
            }));
            HealthyInv { _lock: lock }
        }

        /// Low-memory variant. NEW-002: the skip-coalescing test needs
        /// a request that never fits, but a max-size 4096 MB request
        /// fits on any host with more than 4096 MB available (the VM
        /// has ~5900 MB). A fixed 512 MB inventory makes the non-fit
        /// host-independent. Same lock and drop-clear as set().
        fn set_low_mem() -> Self {
            let lock = crate::inventory::TEST_LOCK.lock().unwrap();
            crate::inventory::set_test_inventory(Some(crate::core::Inventory {
                mem_available_mb: 512,
                mem_total_mb: 8192,
                load1: 0.5,
                disk_free_gb: 50.0,
                ..Default::default()
            }));
            HealthyInv { _lock: lock }
        }
    }

    impl Drop for HealthyInv {
        fn drop(&mut self) {
            crate::inventory::set_test_inventory(None);
        }
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
        let _inv = HealthyInv::set();
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

    /// FIX-003/TEST-043: 15 concurrent requests leave the daemon up
    /// and the socket responsive. Every call gets a response, and a
    /// health check after the storm still answers.
    #[test]
    fn fifteen_concurrent_requests_keep_daemon_up() {
        let _inv = HealthyInv::set();
        let (cfg, dir, sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Arc::new(Daemon::new(store, cfg.clone(), Arc::new(ChromebookAdapter)));
        let sock2 = sock.clone();
        std::thread::spawn(move || {
            let _ = daemon.serve(&sock2);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let sock = Arc::new(sock);
        let mut handles = Vec::new();
        for i in 0..15 {
            let s = Arc::clone(&sock);
            handles.push(std::thread::spawn(move || {
                let mut msg = request_json();
                msg["request"]["job_id"] = json!(format!("storm-{i}"));
                call(s.as_str(), &msg)
            }));
        }
        let mut granted = 0;
        for h in handles {
            let resp = h
                .join()
                .expect("request thread panicked")
                .expect("request call failed");
            assert!(resp.get("status").is_some(), "no status in {resp}");
            if resp["status"] == "granted" {
                granted += 1;
            }
        }
        let health = call(sock.as_str(), &json!({ "cmd": "health" })).unwrap();
        assert_eq!(health["db"], "ok", "health: {health}");
        let status = call(sock.as_str(), &json!({ "cmd": "status" })).unwrap();
        assert_eq!(status["active_leases"], granted);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// FIX-002/TEST-054: a stale socket file is removed on startup and
    /// the daemon binds and serves.
    #[test]
    fn stale_socket_is_removed_on_startup() {
        let (cfg, dir, sock) = tmp_cfg();
        std::fs::write(&sock, b"stale").unwrap();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Arc::new(Daemon::new(store, cfg.clone(), Arc::new(ChromebookAdapter)));
        let sock2 = sock.clone();
        std::thread::spawn(move || {
            let _ = daemon.serve(&sock2);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let health = call(&sock, &json!({ "cmd": "health" })).unwrap();
        assert_eq!(health["db"], "ok", "health: {health}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// FIX-006/TEST-045: backoff delays are 30, 60, 120, 240 s and
    /// there is no 5th retry. Distinct error codes are used so the
    /// circuit breaker does not trip first.
    #[test]
    fn drain_backoff_schedule() {
        let now = 1_700_000_000;
        let mut st = DrainRetryState::default();
        let mut delays = Vec::new();
        for code in ["E04", "E05", "E06", "E07"] {
            let (d, _) = record_drain_failure(&mut st, code, now);
            match d {
                DrainRetryDecision::RetryAt(t) => delays.push(t - now),
                other => panic!("expected RetryAt, got {other:?}"),
            }
        }
        assert_eq!(delays, vec![30, 60, 120, 240]);
        let (d, attempts) = record_drain_failure(&mut st, "E04", now);
        assert_eq!(d, DrainRetryDecision::FailCap);
        assert_eq!(attempts, 5);
    }

    /// FIX-007/TEST-046: 3 consecutive identical errors trip the
    /// breaker and mark the request failed.
    #[test]
    fn circuit_breaker_trips_on_three_identical_errors() {
        let now = 1_700_000_000;
        let mut st = DrainRetryState::default();
        let (d, _) = record_drain_failure(&mut st, "E04", now);
        assert!(matches!(d, DrainRetryDecision::RetryAt(_)));
        let (d, _) = record_drain_failure(&mut st, "E04", now);
        assert!(matches!(d, DrainRetryDecision::RetryAt(_)));
        let (d, attempts) = record_drain_failure(&mut st, "E04", now);
        assert_eq!(d, DrainRetryDecision::FailBreaker);
        assert_eq!(attempts, 3);
        // A different code resets the identical-error run.
        let mut st2 = DrainRetryState::default();
        record_drain_failure(&mut st2, "E04", now);
        record_drain_failure(&mut st2, "E05", now);
        assert_eq!(st2.consecutive_same, 1);
    }

    /// Footgun 1: a client that connects and sends nothing must not
    /// block a second client. The slow connection times out on its own
    /// thread after 2 s; the grant lands in milliseconds.
    #[test]
    fn slow_client_does_not_block_daemon() {
        let _inv = HealthyInv::set();
        use std::io::Write;
        use std::os::unix::net::UnixStream as RawStream;
        let (cfg, dir, sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Arc::new(Daemon::new(store, cfg.clone(), Arc::new(ChromebookAdapter)));
        let sock2 = sock.clone();
        std::thread::spawn(move || {
            let _ = daemon.serve(&sock2);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut slow = RawStream::connect(&sock).unwrap();
        slow.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let start = std::time::Instant::now();
        let grant = call(&sock, &request_json()).unwrap();
        assert_eq!(grant["status"], "granted", "grant: {grant}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "grant waited for the slow client: {:?}",
            start.elapsed()
        );
        let _ = slow.write_all(b"");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Footgun 1: a line over 64 KiB is rejected and the connection closes
    /// without a response, instead of growing the buffer without bound.
    #[test]
    fn oversized_line_is_rejected() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream as RawStream;
        let (cfg, dir, sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Arc::new(Daemon::new(store, cfg.clone(), Arc::new(ChromebookAdapter)));
        let sock2 = sock.clone();
        std::thread::spawn(move || {
            let _ = daemon.serve(&sock2);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut raw = RawStream::connect(&sock).unwrap();
        raw.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let big = vec![b'x'; 70_000];
        raw.write_all(&big).unwrap();
        raw.write_all(b"\n").unwrap();
        let mut out = Vec::new();
        let _ = raw.read_to_end(&mut out);
        assert!(out.is_empty(), "oversized line got a response");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F06/F12: MemAvailable is not double-subtracted, CPU is gated
    /// on load, and desktops honor min(config max_vd, adapter max_vd).
    #[test]
    fn free_capacity_budget() {
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let now = now_unix();
        let mut active = crate::lease::lease_from_request(
            &crate::core::LeaseRequest {
                agent_id: "a".to_string(),
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
                capabilities: vec![],
                mem_mb_max: None,
            },
            "vd1",
            now,
        );
        active.state = "active".to_string();
        let mut reserving = active.clone();
        reserving.lease_id = format!("{}-r", active.lease_id);
        reserving.desktop = "vd2".to_string();
        reserving.mem_mb = 256;
        reserving.state = "reserving".to_string();
        store
            .with_tx(|tx| {
                Store::insert_lease_tx(tx, &active)?;
                Store::insert_lease_tx(tx, &reserving)?;
                Ok(())
            })
            .unwrap();
        let inv = crate::core::Inventory {
            mem_available_mb: 4000,
            mem_total_mb: 8000,
            load1: 2.5,
            bridge_pool_free: Some(2),
            bridge_known: true,
            ..Default::default()
        };
        let caps = crate::core::Capabilities {
            has_battery: true,
            has_desktops: true,
            has_bridge: true,
            max_vd: 10,
            max_ram_mb: 8192,
            arch: "x86_64".to_string(),
        };
        let free = free_capacity(&store, &inv, &cfg, &caps, 4).unwrap();
        // CPU: min(4 - 2 reserved, 4 - ceil(2.5)) = min(2, 1) = 1.
        assert_eq!(free.cpu_cores, 1);
        // RAM: only the reserving lease subtracts: 4000 - 256.
        assert_eq!(free.mem_mb, 3744);
        assert_eq!(free.desktops, 8);
        // Adapter cap binds when it is lower than config.
        let mut small = caps.clone();
        small.max_vd = 2;
        let free2 = free_capacity(&store, &inv, &cfg, &small, 4).unwrap();
        assert_eq!(free2.desktops, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F07: a request for 2 desktops is rejected at submit with E01.
    #[test]
    fn desktops_two_is_rejected() {
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store, cfg, Arc::new(ChromebookAdapter));
        let mut req = request_json();
        req["request"]["desktops"] = json!(2);
        let resp = daemon.handle(&req);
        assert_eq!(resp["error"], "E01", "resp: {resp}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F09: an extension is blocked only when a queued request fits
    /// solely because this lease would go away. A queued desktop
    /// request that fits anyway is not a conflict.
    #[test]
    fn extend_allows_when_queued_request_fits_anyway() {
        let _inv = HealthyInv::set();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let now = now_unix();
        let lease = crate::lease::lease_from_request(
            &crate::core::LeaseRequest {
                agent_id: "a".to_string(),
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
                capabilities: vec![],
                mem_mb_max: None,
            },
            "vd1",
            now,
        );
        store
            .with_tx(|tx| {
                Store::insert_lease_tx(tx, &lease)?;
                Ok(())
            })
            .unwrap();
        let queued = crate::core::QueuedRequest {
            id: 0,
            agent_id: "b".to_string(),
            job_id: "j2".to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 256,
            desktops: 1,
            duration_min: 30,
            priority_class: "P1".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            mem_mb_max: None,
            lease_id: None,
            capabilities: vec![],
            created_unix: now,
            status: "queued".to_string(),
        };
        store.insert_request(&queued).unwrap();
        let resp = daemon.handle(&json!({
            "cmd": "extend",
            "lease_id": lease.lease_id,
            "minutes": 10,
        }));
        assert_eq!(resp["status"], "extended", "resp: {resp}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F09: when every desktop is held, the queued desktop request
    /// fits only if this lease goes away, so the extension is denied.
    #[test]
    fn extend_denies_when_queued_request_needs_this_desktop() {
        let _inv = HealthyInv::set();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let now = now_unix();
        let mut first_id = String::new();
        for n in 1..=10 {
            let lease = crate::lease::lease_from_request(
                &crate::core::LeaseRequest {
                    agent_id: "a".to_string(),
                    job_id: format!("j{n}"),
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                    resource_class: "vd-chrome".to_string(),
                    cpu_cores: 1,
                    mem_mb: 256,
                    desktops: 1,
                    duration_min: 30,
                    priority_class: "P1".to_string(),
                    deadline: None,
                    needs_bridge: false,
                    needs_display: false,
                    capabilities: vec![],
                    mem_mb_max: None,
                },
                &format!("vd{n}"),
                now,
            );
            if n == 1 {
                first_id = lease.lease_id.clone();
            }
            store
                .with_tx(|tx| {
                    Store::insert_lease_tx(tx, &lease)?;
                    Ok(())
                })
                .unwrap();
        }
        let queued = crate::core::QueuedRequest {
            id: 0,
            agent_id: "b".to_string(),
            job_id: "jwait".to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 256,
            desktops: 1,
            duration_min: 30,
            priority_class: "P1".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            mem_mb_max: None,
            lease_id: None,
            capabilities: vec![],
            created_unix: now,
            status: "queued".to_string(),
        };
        store.insert_request(&queued).unwrap();
        let resp = daemon.handle(&json!({
            "cmd": "extend",
            "lease_id": first_id,
            "minutes": 10,
        }));
        assert_eq!(resp["error"], "E09", "resp: {resp}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F12: expand on a host without desktops is E02.
    #[test]
    fn expand_on_oracle_is_e02() {
        use crate::host::OracleAdapter;
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store, cfg, Arc::new(OracleAdapter));
        let resp = daemon.handle(&json!({ "cmd": "expand" }));
        assert_eq!(resp["error"], "E02", "resp: {resp}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F05: the tick expires a request older than 24h.
    #[test]
    fn tick_expires_stale_requests() {
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let mut row = crate::core::QueuedRequest {
            id: 0,
            agent_id: "old".to_string(),
            job_id: "old1".to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "cpu-ram".to_string(),
            cpu_cores: 1,
            mem_mb: 256,
            desktops: 0,
            duration_min: 30,
            priority_class: "P1".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            mem_mb_max: None,
            lease_id: None,
            capabilities: vec![],
            created_unix: now_unix() - 90_000,
            status: "queued".to_string(),
        };
        row.id = store.insert_request(&row).unwrap();
        daemon.tick().unwrap();
        let got = store.get_request(row.id).unwrap().unwrap();
        assert_eq!(got.status, "expired");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// DEFECT-003: a desktop up with no holding lease audits "bypass".
    /// A listener on vd1's CDP port (9333) fakes the desktop being up.
    #[test]
    fn tick_audits_bypass_desktop() {
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let listener = std::net::TcpListener::bind("127.0.0.1:9333").expect("bind fake CDP");
        daemon.tick().unwrap();
        drop(listener);
        let hits = store
            .list_audit(50)
            .unwrap()
            .into_iter()
            .filter(|a| a.event == "bypass" && a.desktop.as_deref() == Some("vd1"))
            .count();
        assert_eq!(hits, 1, "bypass audit missing");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// OBS-006 + DEFECT-004: cancel and clear-flag go through the socket.
    #[test]
    fn cancel_and_clear_flag_commands() {
        let _inv = HealthyInv::set();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let req = request_json();
        let key = req["request"]["idempotency_key"]
            .as_str()
            .unwrap()
            .to_string();
        let first = daemon.handle(&req);
        assert_eq!(first["status"], "granted", "first: {first}");
        let cancelled = daemon.handle(&json!({
            "cmd": "cancel",
            "agent_id": "tester",
            "idempotency_key": key,
        }));
        assert_eq!(cancelled["cancelled"], true, "cancel: {cancelled}");
        store.flag_desktop("vd1", "test flag", now_unix()).unwrap();
        let cleared = daemon.handle(&json!({ "cmd": "clear-flag", "desktop": "vd1" }));
        assert_eq!(cleared["cleared"], true, "clear: {cleared}");
        assert!(!store.is_flagged("vd1").unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F03/OBS-002: with the bridge down, a cpu-ram grant no longer pays
    /// 600 ms of bridge retries. It must land well under the 1 s budget.
    #[test]
    fn grant_with_bridge_down_is_fast() {
        let _inv = HealthyInv::set();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let start = std::time::Instant::now();
        let resp = daemon.handle(&request_json());
        assert_eq!(resp["status"], "granted", "resp: {resp}");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "grant took {:?} with bridge down",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F01: a transient grant failure is a wait, not a denial. The RAM
    /// guard is set above any real host, so the grant fails E06.
    #[test]
    fn transient_grant_failure_stays_queued() {
        let _inv = HealthyInv::set();
        let (mut cfg, dir, _sock) = tmp_cfg();
        cfg.ram_guard_mb = 10_000_000;
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let resp = daemon.handle(&request_json());
        assert_eq!(resp["status"], "queued", "resp: {resp}");
        assert_eq!(resp["reason"], "E06", "resp: {resp}");
        let queued = store.list_queued_requests().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, "queued");
        let last = store.last_audit_for("tester", "t1").unwrap().unwrap();
        assert_eq!(last.event, "wait");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F02: Warn blocks a vd start but the drain continues and grants the
    /// cpu-ram request behind it.
    #[test]
    fn battery_warn_skips_vd_but_grants_cpu_ram() {
        let _inv = HealthyInv::set();
        use crate::core::{Battery, BatteryState};
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        crate::battery::set_test_battery(Some(Battery {
            pct: Some(15),
            charging: false,
            present: true,
            state: BatteryState::Warn,
        }));
        let vd = json!({
            "cmd": "request",
            "request": {
                "agent_id": "tester",
                "job_id": "vd1",
                "idempotency_key": uuid::Uuid::new_v4().to_string(),
                "resource_class": "vd-chrome",
                "cpu_cores": 1,
                "mem_mb": 256,
                "desktops": 1,
                "duration_min": 30,
                "priority_class": "P1",
                "needs_bridge": false,
                "needs_display": true,
                "capabilities": []
            }
        });
        let r1 = daemon.handle(&vd);
        assert_eq!(r1["status"], "queued", "vd resp: {r1}");
        let r2 = daemon.handle(&request_json());
        assert_eq!(r2["status"], "granted", "cpu resp: {r2}");
        let queued = store.list_queued_requests().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].job_id, "vd1");
        crate::battery::set_test_battery(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Skip audits coalesce: two drain passes write one skip row.
    #[test]
    fn skip_audits_coalesce_within_a_minute() {
        // NEW-002: the inventory is fixed low, so the 4096 MB request
        // never fits on any host. The old premise (MemAvailable ~600 MB
        // on the dev VM) failed on the chromebook-vm (~5900 MB), where
        // the request fit, no skip was audited, and the test failed.
        let _inv = HealthyInv::set_low_mem();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let mut big = request_json();
        big["request"]["job_id"] = json!("big");
        // 4096 MB validates but never fits against the fixed 512 MB inventory.
        big["request"]["mem_mb"] = json!(4096);
        daemon.handle(&big);
        daemon.drain_queue().unwrap();
        daemon.drain_queue().unwrap();
        let skips = store
            .list_audit(200)
            .unwrap()
            .into_iter()
            .filter(|a| a.event == "skip" && a.job_id.as_deref() == Some("big"))
            .count();
        assert_eq!(skips, 1, "skip rows written twice");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Patch 12: two concurrent drains grant one queued request once.
    /// At startup the tick thread and the main thread both drain; without
    /// a single-flight lock both listed the same queued request and each
    /// granted it (live host, 2026-11-06: one vd-chrome request got two
    /// leases, vd1 and vd2, in the same second).
    #[test]
    fn concurrent_drains_grant_once() {
        // Healthy inventory: without the guard this test flakes when
        // parallel tests spike live load1 and the grant stops fitting.
        let _inv = HealthyInv::set();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Arc::new(Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter)));
        let req: LeaseRequest = serde_json::from_value(request_json()["request"].clone()).unwrap();
        let caps = daemon.adapter.capabilities();
        let outcome = crate::queue::submit(&store, &req, &caps, now_unix()).unwrap();
        assert!(!outcome.duplicate);
        assert_eq!(store.list_queued_requests().unwrap().len(), 1);
        std::thread::scope(|s| {
            let d1 = daemon.clone();
            let d2 = daemon.clone();
            let t1 = s.spawn(move || d1.drain_queue());
            let t2 = s.spawn(move || d2.drain_queue());
            t1.join().unwrap().unwrap();
            t2.join().unwrap().unwrap();
        });
        let active = store.list_leases(&["active"]).unwrap();
        assert_eq!(active.len(), 1, "one request granted twice");
        let row = store.get_request(outcome.request_id).unwrap().unwrap();
        assert_eq!(row.status, "granted");
        assert_eq!(row.lease_id.as_deref(), Some(active[0].lease_id.as_str()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F16: an idempotent replay returns the grant, not a bare duplicate.
    #[test]
    fn duplicate_replay_returns_grant() {
        let _inv = HealthyInv::set();
        let (cfg, dir, _sock) = tmp_cfg();
        let store = Arc::new(Store::open(&cfg.db_path).unwrap());
        let daemon = Daemon::new(store.clone(), cfg, Arc::new(ChromebookAdapter));
        let req = request_json();
        let first = daemon.handle(&req);
        assert_eq!(first["status"], "granted", "first: {first}");
        let second = daemon.handle(&req);
        assert_eq!(second["status"], "granted", "second: {second}");
        assert_eq!(second["lease_id"], first["lease_id"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
