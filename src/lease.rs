//! Lease manager: two-phase reservation. V3 section 5.2, ADR-005.
//! Phase 1 reserve in one transaction, phase 2 provision, phase 3 commit
//! or roll back. No partial grant survives a failure.
//! Heartbeat and reclaim land in TASK-009, in this module.

use crate::core::{
    new_lease_id, unix_to_local_iso, Config, Grant, Inventory, Lease, LeaseRequest, MoError,
    QueuedRequest, Result,
};
use crate::store::Store;

/// Pick a free desktop for a request. None when no slot is free.
/// "none" is returned for classes that need no desktop. A desktop that
/// is already up is preferred over a cold one. A cold desktop is only
/// started when the load allows it and the sustained count is not
/// reached; otherwise the request waits for a warm desktop.
pub fn pick_desktop(
    store: &Store,
    req: &QueuedRequest,
    cfg: &Config,
    caps: &crate::core::Capabilities,
    load1: f64,
    ncpu: i64,
    desktops: &[crate::core::Desktop],
) -> Result<Option<String>> {
    if req.desktops == 0 {
        return Ok(Some("none".to_string()));
    }
    if !caps.has_desktops {
        return Err(MoError::e02("this host has no desktops"));
    }
    let (_, _, held) = store.reserved_totals()?;
    let want_main = req.resource_class == "main-display" || req.needs_display;
    if want_main {
        if held.iter().any(|d| d == "main-display") {
            return Ok(None);
        }
        return Ok(Some("main-display".to_string()));
    }
    let vd_held = held
        .iter()
        .filter(|d| *d != "main-display" && *d != "none")
        .count() as i64;
    let cold_ok = load1 <= ncpu as f64 && vd_held < cfg.sustained_vd;
    for d in desktops {
        if d.id == "main-display" {
            continue;
        }
        if d.up && !held.contains(&d.id) {
            return Ok(Some(d.id.clone()));
        }
    }
    if !cold_ok {
        return Ok(None);
    }
    for d in desktops {
        if d.id == "main-display" {
            continue;
        }
        if !d.up && !held.contains(&d.id) {
            return Ok(Some(d.id.clone()));
        }
    }
    Ok(None)
}

/// Ports and profile for a desktop id. V3 section 5.2.
pub fn desktop_binding(desktop: &str) -> (String, Option<i64>, Option<i64>, Option<String>) {
    match desktop {
        "main-display" => (":1".to_string(), Some(9222), Some(5901), None),
        "none" => ("none".to_string(), None, None, None),
        _ => {
            let n: i64 = desktop.trim_start_matches("vd").parse().unwrap_or(0);
            (
                format!(":{}", n + 1),
                Some(9332 + n),
                Some(5902 + n),
                Some(format!("{}/chrome-vd{n}", crate::core::profile_root())),
            )
        }
    }
}

/// Grant a lease through two-phase reservation.
/// `provision` starts the desktop and waits for CDP (up to 10s).
/// `stop` stops a desktop after a failed provision.
/// Both are closures so tests can inject failures without a live desktop.
#[allow(clippy::too_many_arguments)]
pub fn grant(
    store: &Store,
    req: &QueuedRequest,
    desktop: &str,
    inv: &Inventory,
    cfg: &Config,
    now: i64,
    provision: &dyn Fn(&str) -> Result<()>,
    stop: &dyn Fn(&str) -> Result<()>,
) -> Result<Grant> {
    // Guards on a fresh inventory read. V3 section 9.3.
    if inv.mem_available_mb < cfg.ram_guard_mb {
        return Err(MoError::e06(format!(
            "MemAvailable {} MB under guard {} MB",
            inv.mem_available_mb, cfg.ram_guard_mb
        )));
    }
    if inv.disk_free_gb < cfg.disk_guard_gb as f64 {
        return Err(MoError::e07(format!(
            "disk free {:.2} GB under guard {} GB",
            inv.disk_free_gb, cfg.disk_guard_gb
        )));
    }
    let (display, cdp_port, vnc_port, profile) = desktop_binding(desktop);
    let lease_id = new_lease_id();
    let start = now;
    let end = now + req.duration_min * 60;
    let lease = Lease {
        lease_id: lease_id.clone(),
        agent_id: req.agent_id.clone(),
        job_id: req.job_id.clone(),
        resource_class: req.resource_class.clone(),
        desktop: desktop.to_string(),
        display: display.clone(),
        cdp_port,
        vnc_port,
        profile: profile.clone(),
        cpu_cores: req.cpu_cores,
        mem_mb: req.mem_mb,
        state: "reserving".to_string(),
        start_unix: start,
        end_unix: end,
        heartbeat_due_unix: now + cfg.heartbeat_min * 60,
        heartbeat_misses: 0,
        extensions: 0,
        last_heartbeat_unix: 0,
        created_unix: now,
    };
    // Phase 1: reserve in one transaction.
    let reserve = store.with_tx(|tx| {
        Store::insert_lease_tx(tx, &lease)?;
        Store::insert_reservation_tx(
            tx,
            &lease_id,
            desktop,
            req.cpu_cores,
            req.mem_mb,
            if req.needs_bridge { 1 } else { 0 },
            now,
        )?;
        Store::audit_tx(
            tx,
            "reserve",
            Some(&req.agent_id),
            Some(&req.job_id),
            Some(&lease_id),
            Some(desktop),
            "reservation held",
            now,
        )?;
        Ok(())
    });
    if let Err(e) = reserve {
        // Only a unique-index collision means "desktop busy" (E04).
        // A disk-full error is E07, anything else keeps its own code.
        if e.code == "E04" {
            return Err(MoError::e04(format!(
                "desktop {desktop} busy, no free slot: {}",
                e.message
            )));
        }
        return Err(e);
    }
    // Phase 2: provision. Desktop start and CDP wait happen outside the tx.
    if desktop != "none" {
        if let Err(e) = provision(desktop) {
            rollback(store, &lease, req, desktop, now, &e.message, stop);
            return Err(MoError::e04(format!(
                "provision failed for {desktop}: {}",
                e.message
            )));
        }
    }
    // Phase 3: commit. Lease becomes ACTIVE, one audit row, same transaction.
    // A failure here rolls back: the reservation is deleted, the lease is
    // marked failed, and the provisioned desktop is stopped. The lease is
    // never left in "reserving" with a desktop up.
    if let Err(e) = store.with_tx(|tx| {
        Store::update_lease_state_tx(tx, &lease_id, "active")?;
        Store::audit_tx(
            tx,
            "grant",
            Some(&req.agent_id),
            Some(&req.job_id),
            Some(&lease_id),
            Some(desktop),
            "lease granted",
            now,
        )?;
        Ok(())
    }) {
        rollback(store, &lease, req, desktop, now, &e.message, stop);
        return Err(e);
    }
    Ok(Grant {
        lease_id,
        desktop: desktop.to_string(),
        display,
        cdp_port,
        vnc_port,
        profile,
        start: unix_to_local_iso(start),
        end: unix_to_local_iso(end),
        heartbeat_due: unix_to_local_iso(now + cfg.heartbeat_min * 60),
        status: "granted".to_string(),
    })
}

/// Roll back a failed grant: delete the reservation, mark FAILED, audit, stop.
fn rollback(
    store: &Store,
    lease: &Lease,
    req: &QueuedRequest,
    desktop: &str,
    now: i64,
    reason: &str,
    stop: &dyn Fn(&str) -> Result<()>,
) {
    let _ = store.with_tx(|tx| {
        Store::delete_reservations_tx(tx, &lease.lease_id)?;
        Store::update_lease_state_tx(tx, &lease.lease_id, "failed")?;
        Store::audit_tx(
            tx,
            "grant_failed",
            Some(&req.agent_id),
            Some(&req.job_id),
            Some(&lease.lease_id),
            Some(desktop),
            &format!("rollback: {reason}"),
            now,
        )?;
        Ok(())
    });
    // Never stop the main display or a no-desktop lease. Only vd desktops
    // are stopped, and only when one was actually provisioned.
    if desktop.starts_with("vd") {
        let _ = stop(desktop);
    }
}

/// Release a lease. Idempotent: a second release returns ok, no error.
/// Release a lease. Idempotent: a second release returns ok, no error.
/// DEFECT-002: release stops the desktop it held. A stop failure does not
/// fail the release, but it is audited as "stop_failed" and the desktop is
/// flagged so fold refuses it until an operator clears the flag.
pub fn release(
    store: &Store,
    lease_id: &str,
    now: i64,
    stop: &dyn Fn(&str) -> Result<()>,
) -> Result<bool> {
    let lease = match store.get_lease(lease_id)? {
        Some(l) => l,
        None => return Err(MoError::e08(format!("lease not found: {lease_id}"))),
    };
    if lease.state == "released" {
        return Ok(false);
    }
    store.with_tx(|tx| {
        Store::delete_reservations_tx(tx, lease_id)?;
        Store::update_lease_state_tx(tx, lease_id, "released")?;
        Store::audit_tx(
            tx,
            "release",
            Some(&lease.agent_id),
            Some(&lease.job_id),
            Some(lease_id),
            Some(&lease.desktop),
            "lease released",
            now,
        )?;
        Ok(())
    })?;
    if lease.desktop.starts_with("vd") {
        if let Err(e) = stop(&lease.desktop) {
            let reason = format!("stop failed on release: {}", e.message);
            let _ = store.audit(
                "stop_failed",
                Some(&lease.agent_id),
                Some(&lease.job_id),
                Some(lease_id),
                Some(&lease.desktop),
                &reason,
                now,
            );
            let _ = store.flag_desktop(&lease.desktop, &reason, now);
        }
    }
    Ok(true)
}

/// Extend a lease. Max 30 minutes per extension, max total 120 minutes.
/// Denied with E09 when a queued request needs the same bundle.
pub fn extend(
    store: &Store,
    lease_id: &str,
    minutes: i64,
    cfg: &Config,
    queue_conflict: bool,
    now: i64,
) -> Result<Lease> {
    if !(1..=cfg.extend_max_min).contains(&minutes) {
        return Err(MoError::e09(format!(
            "extension {minutes} min out of range 1 to {}",
            cfg.extend_max_min
        )));
    }
    let mut lease = match store.get_lease(lease_id)? {
        Some(l) => l,
        None => return Err(MoError::e08(format!("lease not found: {lease_id}"))),
    };
    if lease.state != "active" {
        return Err(MoError::e09(format!(
            "lease {lease_id} is {}, not active",
            lease.state
        )));
    }
    if queue_conflict {
        return Err(MoError::e09(
            "extension denied: a queued request needs the same bundle",
        ));
    }
    let total_min = (lease.end_unix - lease.start_unix) / 60 + minutes;
    if total_min > cfg.lease_max_min {
        return Err(MoError::e09(format!(
            "extension denied: total {total_min} min exceeds max {} min",
            cfg.lease_max_min
        )));
    }
    lease.end_unix += minutes * 60;
    lease.extensions += 1;
    lease.state = "active".to_string();
    let lease_clone = lease.clone();
    store.with_tx(|tx| {
        Store::update_lease_tx(tx, &lease_clone)?;
        Store::audit_tx(
            tx,
            "extend",
            Some(&lease_clone.agent_id),
            Some(&lease_clone.job_id),
            Some(lease_id),
            Some(&lease_clone.desktop),
            &format!("extended by {minutes} min, total {total_min} min"),
            now,
        )?;
        Ok(())
    })?;
    Ok(lease)
}

/// Record a heartbeat. Resets misses, pushes the next due time.
/// A STALE lease revives to ACTIVE on a fresh heartbeat. E08 when unknown.
pub fn heartbeat(store: &Store, lease_id: &str, now: i64, cfg: &Config) -> Result<Lease> {
    let mut lease = match store.get_lease(lease_id)? {
        Some(l) => l,
        None => return Err(MoError::e08(format!("lease not found: {lease_id}"))),
    };
    if lease.state == "released" || lease.state == "expired" || lease.state == "failed" {
        return Err(MoError::e08(format!(
            "lease {lease_id} is {}, no heartbeat accepted",
            lease.state
        )));
    }
    // F08: an interrupted lease revives only inside its 10-minute window,
    // only while its desktop is still alive, and only when the battery
    // gate allows a new lease. Otherwise it stays interrupted until the
    // tick expires it, and the agent must re-request.
    if lease.state == "interrupted" {
        if now - lease.last_heartbeat_unix > 600 {
            return Err(MoError::e08(format!(
                "lease {lease_id} interrupted over 10 minutes ago, re-request"
            )));
        }
        if lease.desktop.starts_with("vd") {
            let (_, cdp_port, _, profile) = desktop_binding(&lease.desktop);
            let cdp_alive = cdp_port.map(crate::desktops::cdp_ok).unwrap_or(false);
            let chrome_alive = profile
                .as_deref()
                .map(crate::desktops::chrome_alive_for_profile)
                .unwrap_or(false);
            if !cdp_alive || !chrome_alive {
                return Err(MoError::e08(format!(
                    "lease {lease_id} desktop {} is dead, re-request",
                    lease.desktop
                )));
            }
        }
        let battery = crate::battery::read_battery();
        let state = crate::battery::effective_state(battery.as_ref());
        crate::battery::can_new_lease(
            state,
            crate::core::PriorityClass::P1,
            lease.desktop != "none",
        )
        .map_err(|e| MoError::e08(format!("lease {lease_id} cannot revive: {}", e.message)))?;
    }
    let revived = lease.state == "stale";
    lease.state = "active".to_string();
    lease.heartbeat_misses = 0;
    lease.heartbeat_due_unix = now + cfg.heartbeat_min * 60;
    // One transaction for the heartbeat: the last-beat time rides on
    // the lease row now. The heartbeats table is no longer written on
    // this path (F14: it cost a second commit per heartbeat).
    lease.last_heartbeat_unix = now;
    let lease_clone = lease.clone();
    store.with_tx(|tx| {
        Store::update_lease_tx(tx, &lease_clone)?;
        Store::audit_tx(
            tx,
            if revived {
                "heartbeat_revive"
            } else {
                "heartbeat"
            },
            Some(&lease_clone.agent_id),
            Some(&lease_clone.job_id),
            Some(lease_id),
            Some(&lease_clone.desktop),
            "heartbeat received",
            now,
        )?;
        Ok(())
    })?;
    Ok(lease)
}

/// Sweep: count missed beats for ACTIVE leases.
/// Miss 1: lease stays ACTIVE, flag warned, audit event.
/// Miss 2: lease becomes STALE, audit event. Returns stale lease ids.
pub fn sweep_stale(store: &Store, now: i64, cfg: &Config) -> Result<Vec<String>> {
    let active = store.list_leases(&["active"])?;
    let interval = cfg.heartbeat_min * 60;
    let mut stale = Vec::new();
    for mut lease in active {
        // The last beat rides on the lease row, not the heartbeats table.
        let last = if lease.last_heartbeat_unix > 0 {
            lease.last_heartbeat_unix
        } else {
            lease.start_unix
        };
        let missed = (now - last) / interval;
        if missed >= cfg.heartbeat_miss && lease.heartbeat_misses < cfg.heartbeat_miss {
            lease.state = "stale".to_string();
            lease.heartbeat_misses = missed;
            let lc = lease.clone();
            store.with_tx(|tx| {
                Store::update_lease_tx(tx, &lc)?;
                Store::audit_tx(
                    tx,
                    "stale",
                    Some(&lc.agent_id),
                    Some(&lc.job_id),
                    Some(&lc.lease_id),
                    Some(&lc.desktop),
                    &format!("missed {missed} heartbeats"),
                    now,
                )?;
                Ok(())
            })?;
            stale.push(lease.lease_id);
        } else if missed == 1 && lease.heartbeat_misses == 0 {
            lease.heartbeat_misses = 1;
            let lc = lease.clone();
            store.with_tx(|tx| {
                Store::update_lease_tx(tx, &lc)?;
                Store::audit_tx(
                    tx,
                    "heartbeat_warned",
                    Some(&lc.agent_id),
                    Some(&lc.job_id),
                    Some(&lc.lease_id),
                    Some(&lc.desktop),
                    "missed 1 heartbeat",
                    now,
                )?;
                Ok(())
            })?;
        }
    }
    Ok(stale)
}

/// Expire INTERRUPTED leases past their 10-minute revive window. The
/// window runs from last_heartbeat_unix, set when recovery interrupts
/// the lease. Reservations go with the lease.
pub fn expire_interrupted(store: &Store, now: i64) -> Result<Vec<String>> {
    let interrupted = store.list_leases(&["interrupted"])?;
    let mut expired = Vec::new();
    for lease in interrupted {
        if now - lease.last_heartbeat_unix > 600 {
            let lc = lease.clone();
            store.with_tx(|tx| {
                Store::delete_reservations_tx(tx, &lc.lease_id)?;
                Store::update_lease_state_tx(tx, &lc.lease_id, "expired")?;
                Store::audit_tx(
                    tx,
                    "interrupted_expired",
                    Some(&lc.agent_id),
                    Some(&lc.job_id),
                    Some(&lc.lease_id),
                    Some(&lc.desktop),
                    "interrupted over 10 minutes, no re-heartbeat",
                    now,
                )?;
                Ok(())
            })?;
            expired.push(lease.lease_id);
        }
    }
    Ok(expired)
}

/// Mark ACTIVE leases past their end as EXPIRED. Reclaim frees them later.
pub fn expire_leases(store: &Store, now: i64) -> Result<Vec<String>> {
    let active = store.list_leases(&["active"])?;
    let mut expired = Vec::new();
    for lease in active {
        if now > lease.end_unix {
            let lc = lease.clone();
            store.with_tx(|tx| {
                Store::delete_reservations_tx(tx, &lc.lease_id)?;
                Store::update_lease_state_tx(tx, &lc.lease_id, "expired")?;
                Store::audit_tx(
                    tx,
                    "expired",
                    Some(&lc.agent_id),
                    Some(&lc.job_id),
                    Some(&lc.lease_id),
                    Some(&lc.desktop),
                    "lease end reached",
                    now,
                )?;
                Ok(())
            })?;
            expired.push(lease.lease_id);
        }
    }
    Ok(expired)
}

/// Reclaim probe: what the desktop actually looks like right now.
#[derive(Debug, Clone, Copy)]
pub struct ReclaimProbe {
    pub cdp_ok: bool,
    pub chrome_alive: bool,
    pub lock_held: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimOutcome {
    /// Lease stays STALE/EXPIRED. MO never kills a live Chrome blindly.
    KeptStale(String),
    /// Desktop was dead. Lease released, desktop freed.
    Released,
}

/// Reclaim check for a STALE or EXPIRED lease, after the grace period.
/// Grace runs from the last heartbeat plus 2 missed intervals.
pub fn reclaim_check(
    store: &Store,
    lease_id: &str,
    now: i64,
    cfg: &Config,
    probe: &ReclaimProbe,
) -> Result<ReclaimOutcome> {
    let lease = match store.get_lease(lease_id)? {
        Some(l) => l,
        None => return Err(MoError::e08(format!("lease not found: {lease_id}"))),
    };
    if lease.state != "stale" && lease.state != "expired" {
        return Err(MoError::e08(format!(
            "lease {lease_id} is {}, reclaim needs stale or expired",
            lease.state
        )));
    }
    let last = if lease.last_heartbeat_unix > 0 {
        lease.last_heartbeat_unix
    } else {
        lease.start_unix
    };
    let stale_at = last + cfg.heartbeat_miss * cfg.heartbeat_min * 60;
    if now < stale_at + cfg.reclaim_grace_min * 60 {
        return Ok(ReclaimOutcome::KeptStale(
            "grace period not over".to_string(),
        ));
    }
    if probe.cdp_ok && probe.chrome_alive {
        store.audit(
            "reclaim_kept",
            Some(&lease.agent_id),
            Some(&lease.job_id),
            Some(lease_id),
            Some(&lease.desktop),
            "CDP responds and Chrome lives: no kill",
            now,
        )?;
        return Ok(ReclaimOutcome::KeptStale(
            "CDP responds and Chrome lives".to_string(),
        ));
    }
    if !probe.cdp_ok && probe.chrome_alive {
        store.audit(
            "reclaim_flagged",
            Some(&lease.agent_id),
            Some(&lease.job_id),
            Some(lease_id),
            Some(&lease.desktop),
            "CDP dead but Chrome lives: operator flagged, no kill",
            now,
        )?;
        return Ok(ReclaimOutcome::KeptStale(
            "CDP dead but Chrome lives: operator flagged".to_string(),
        ));
    }
    // OBS-007: CDP responds but no Chrome process exists. The desktop
    // is dead even though the port answers, so it is freed like the
    // CDP-dead case, with an audit row that says so.
    if probe.cdp_ok && !probe.chrome_alive {
        store.with_tx(|tx| {
            Store::delete_reservations_tx(tx, lease_id)?;
            Store::update_lease_state_tx(tx, lease_id, "released")?;
            Store::audit_tx(
                tx,
                "reclaim",
                Some(&lease.agent_id),
                Some(&lease.job_id),
                Some(lease_id),
                Some(&lease.desktop),
                "CDP responds but no Chrome process: desktop freed",
                now,
            )?;
            Ok(())
        })?;
        return Ok(ReclaimOutcome::Released);
    }
    // CDP dead and Chrome dead: free the desktop.
    store.with_tx(|tx| {
        Store::delete_reservations_tx(tx, lease_id)?;
        Store::update_lease_state_tx(tx, lease_id, "released")?;
        Store::audit_tx(
            tx,
            "reclaim",
            Some(&lease.agent_id),
            Some(&lease.job_id),
            Some(lease_id),
            Some(&lease.desktop),
            "CDP dead and Chrome dead: desktop freed",
            now,
        )?;
        Ok(())
    })?;
    Ok(ReclaimOutcome::Released)
}

/// Reclaim every STALE or EXPIRED lease: build the probe from live
/// desktop state and run reclaim_check on each. Used by the tick.
pub fn reclaim_all(store: &Store, now: i64, cfg: &Config) -> Result<()> {
    let stale = store.list_leases(&["stale", "expired"])?;
    for lease in stale {
        let probe = ReclaimProbe {
            cdp_ok: lease.cdp_port.map(crate::desktops::cdp_ok).unwrap_or(false),
            chrome_alive: lease
                .profile
                .as_deref()
                .map(crate::desktops::chrome_alive_for_profile)
                .unwrap_or(false),
            lock_held: lease
                .profile
                .as_deref()
                .map(|p| !crate::desktops::profile_lock_free(p))
                .unwrap_or(false),
        };
        let _ = reclaim_check(store, &lease.lease_id, now, cfg, &probe);
    }
    Ok(())
}

/// Build a Lease row from a LeaseRequest for direct use in tests.
pub fn lease_from_request(req: &LeaseRequest, desktop: &str, now: i64) -> Lease {
    let (display, cdp_port, vnc_port, profile) = desktop_binding(desktop);
    Lease {
        lease_id: new_lease_id(),
        agent_id: req.agent_id.clone(),
        job_id: req.job_id.clone(),
        resource_class: req.resource_class.clone(),
        desktop: desktop.to_string(),
        display,
        cdp_port,
        vnc_port,
        profile,
        cpu_cores: req.cpu_cores,
        mem_mb: req.mem_mb,
        state: "active".to_string(),
        start_unix: now,
        end_unix: now + req.duration_min * 60,
        heartbeat_due_unix: now + 300,
        heartbeat_misses: 0,
        extensions: 0,
        last_heartbeat_unix: 0,
        created_unix: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{now_unix, LeaseRequest};

    fn tmp_store() -> (Store, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = format!("/tmp/mo-l-{nanos}.db");
        (Store::open(&p).unwrap(), p)
    }

    fn queued_req() -> QueuedRequest {
        QueuedRequest {
            id: 1,
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

    fn inv_ok() -> Inventory {
        Inventory {
            mem_available_mb: 3895,
            disk_free_gb: 50.0,
            ..Default::default()
        }
    }

    fn ok_provision(_d: &str) -> Result<()> {
        Ok(())
    }
    fn ok_stop(_d: &str) -> Result<()> {
        Ok(())
    }
    fn fail_provision(_d: &str) -> Result<()> {
        Err(MoError::e04("CDP never responded"))
    }

    #[test]
    fn grant_happy_path() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        assert_eq!(g.desktop, "vd1");
        assert_eq!(g.display, ":2");
        assert_eq!(g.cdp_port, Some(9333));
        assert_eq!(g.vnc_port, Some(5903));
        assert_eq!(g.status, "granted");
        let lease = s.get_lease(&g.lease_id).unwrap().unwrap();
        assert_eq!(lease.state, "active");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn grant_rolls_back_when_cdp_fails() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let err = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &fail_provision,
            &ok_stop,
        )
        .unwrap_err();
        assert_eq!(err.code, "E04");
        // No active lease, no reservation, lease FAILED, desktop free again.
        assert!(s.list_leases(&["active"]).unwrap().is_empty());
        let (cpu, mem, held) = s.reserved_totals().unwrap();
        assert_eq!((cpu, mem), (0, 0));
        assert!(held.is_empty());
        let failed = s.list_leases(&["failed"]).unwrap();
        assert_eq!(failed.len(), 1);
        // Desktop can be granted again after rollback.
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        assert_eq!(g.desktop, "vd1");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn double_grant_same_desktop_blocked() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let err = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap_err();
        assert_eq!(err.code, "E04");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn ram_guard_denies_grant() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let low = Inventory {
            mem_available_mb: 800,
            disk_free_gb: 50.0,
            ..Default::default()
        };
        let err = grant(
            &s,
            &req,
            "vd1",
            &low,
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap_err();
        assert_eq!(err.code, "E06");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// F04: a phase-3 failure rolls back. The trigger fails any move to
    /// active, so the commit fails and rollback must run.
    #[test]
    fn phase3_failure_rolls_back() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        s.with_tx(|tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_commit BEFORE UPDATE ON leases
                 WHEN NEW.state='active' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )?;
            Ok(())
        })
        .unwrap();
        let err = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap_err();
        let _ = err;
        assert_eq!(s.list_leases(&["failed"]).unwrap().len(), 1);
        assert!(s.list_leases(&["reserving"]).unwrap().is_empty());
        let (cpu, mem, held) = s.reserved_totals().unwrap();
        assert_eq!((cpu, mem), (0, 0));
        assert!(held.is_empty());
        s.with_tx(|tx| {
            tx.execute_batch("DROP TRIGGER fail_commit")?;
            Ok(())
        })
        .unwrap();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        assert_eq!(g.desktop, "vd1");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// DEFECT-002/004: release stops the desktop. A stop failure does not
    /// fail the release, but it audits "stop_failed" and flags the desktop.
    #[test]
    fn release_stops_desktop_and_flags_on_stop_failure() {
        use std::sync::Mutex;
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let stopped = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stopped2 = stopped.clone();
        let record_stop = move |d: &str| {
            stopped2.lock().unwrap().push(d.to_string());
            Ok(())
        };
        assert!(release(&s, &g.lease_id, now_unix(), &record_stop).unwrap());
        assert_eq!(stopped.lock().unwrap().as_slice(), ["vd1"]);
        let (cpu, mem, held) = s.reserved_totals().unwrap();
        assert_eq!((cpu, mem), (0, 0));
        assert!(held.is_empty());

        let g2 = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let fail_stop = |_d: &str| Err(MoError::e04("vd-stop vd1 exited exit status: 1"));
        assert!(release(&s, &g2.lease_id, now_unix(), &fail_stop).unwrap());
        assert!(s.is_flagged("vd1").unwrap());
        let stops = s
            .list_audit(20)
            .unwrap()
            .into_iter()
            .filter(|a| a.event == "stop_failed")
            .count();
        assert_eq!(stops, 1);
        // Second release is a no-op and never calls stop again.
        let never = |_d: &str| panic!("stop must not run on a second release");
        assert!(!release(&s, &g2.lease_id, now_unix(), &never).unwrap());
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// F08: an interrupted lease revives inside its window when it
    /// holds no desktop, and refuses after the window or with a dead desktop.
    fn desktop(id: &str, up: bool) -> crate::core::Desktop {
        crate::core::Desktop {
            id: id.to_string(),
            display: ":2".to_string(),
            cdp_port: Some(9333),
            vnc_port: Some(5903),
            profile: None,
            up,
            leased: false,
        }
    }

    fn lease_req() -> crate::core::LeaseRequest {
        crate::core::LeaseRequest {
            agent_id: "a".to_string(),
            job_id: "j".to_string(),
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
        }
    }

    fn caps() -> crate::core::Capabilities {
        crate::core::Capabilities {
            has_battery: true,
            has_desktops: true,
            has_bridge: true,
            max_vd: 10,
            max_ram_mb: 8192,
            arch: "x86_64".to_string(),
        }
    }

    /// F11/F12: a warm desktop is picked before a cold one. A cold
    /// desktop is not started when the load is over the core count or
    /// the sustained count is reached.
    #[test]
    fn pick_desktop_prefers_warm_and_gates_cold() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let ds = vec![
            desktop("main-display", true),
            desktop("vd1", true),
            desktop("vd2", false),
        ];
        assert_eq!(
            pick_desktop(&s, &req, &cfg, &caps(), 0.5, 4, &ds).unwrap(),
            Some("vd1".to_string())
        );
        // No warm desktop and load over cores: no cold start.
        let cold = vec![desktop("main-display", true), desktop("vd1", false)];
        assert_eq!(
            pick_desktop(&s, &req, &cfg, &caps(), 99.0, 4, &cold).unwrap(),
            None
        );
        // No warm desktop and sustained count reached: no cold start.
        let now = now_unix();
        for n in 1..=6 {
            let mut l = lease_from_request(&lease_req(), &format!("vd{n}"), now);
            l.state = "active".to_string();
            s.with_tx(|tx| {
                Store::insert_lease_tx(tx, &l)?;
                Ok(())
            })
            .unwrap();
        }
        let held_warm: Vec<crate::core::Desktop> =
            (1..=7).map(|n| desktop(&format!("vd{n}"), true)).collect();
        // vd7 is up but the sustained gate only blocks cold starts;
        // a warm free desktop still grants.
        assert_eq!(
            pick_desktop(&s, &req, &cfg, &caps(), 0.5, 4, &held_warm).unwrap(),
            Some("vd7".to_string())
        );
        let all_down: Vec<crate::core::Desktop> =
            (1..=7).map(|n| desktop(&format!("vd{n}"), false)).collect();
        assert_eq!(
            pick_desktop(&s, &req, &cfg, &caps(), 0.5, 4, &all_down).unwrap(),
            None
        );
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn interrupted_heartbeat_revive_rules() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let now = now_unix();
        // CPU-RAM lease, no desktop: revives inside the window.
        let mut l = lease_from_request(
            &crate::core::LeaseRequest {
                agent_id: "a".to_string(),
                job_id: "j".to_string(),
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
                capabilities: vec![],
                mem_mb_max: None,
            },
            "none",
            now,
        );
        l.state = "interrupted".to_string();
        l.last_heartbeat_unix = now;
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &l)?;
            Ok(())
        })
        .unwrap();
        let revived = heartbeat(&s, &l.lease_id, now + 60, &cfg).unwrap();
        assert_eq!(revived.state, "active");
        // Same lease, interrupted again, past the window: refused.
        let mut l2 = l.clone();
        l2.lease_id = format!("{}-old", l.lease_id);
        l2.state = "interrupted".to_string();
        l2.last_heartbeat_unix = now - 601;
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &l2)?;
            Ok(())
        })
        .unwrap();
        let err = heartbeat(&s, &l2.lease_id, now, &cfg).unwrap_err();
        assert_eq!(err.code, "E08");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// F08: interrupted leases past 600 s expire on the tick.
    #[test]
    fn expire_interrupted_after_window() {
        let (s, p) = tmp_store();
        let now = now_unix();
        let mut l = lease_from_request(
            &crate::core::LeaseRequest {
                agent_id: "a".to_string(),
                job_id: "j".to_string(),
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
                capabilities: vec![],
                mem_mb_max: None,
            },
            "none",
            now - 1000,
        );
        l.state = "interrupted".to_string();
        l.last_heartbeat_unix = now - 601;
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &l)?;
            Store::insert_reservation_tx(tx, &l.lease_id, "none", 1, 256, 0, now - 1000)?;
            Ok(())
        })
        .unwrap();
        let expired = expire_interrupted(&s, now).unwrap();
        assert_eq!(expired, vec![l.lease_id.clone()]);
        let (cpu, mem, _) = s.reserved_totals().unwrap();
        assert_eq!((cpu, mem), (0, 0));
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// F14: a heartbeat writes the last-beat time on the lease row in
    /// the same transaction, and the sweep reads it from there.
    #[test]
    fn heartbeat_updates_last_beat_and_sweep_uses_it() {
        let (s, p) = tmp_store();
        let mut cfg = Config::from_env();
        cfg.heartbeat_min = 5;
        cfg.heartbeat_miss = 2;
        let req = queued_req();
        let start = now_unix();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            start,
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let hb = heartbeat(&s, &g.lease_id, start, &cfg).unwrap();
        assert_eq!(hb.last_heartbeat_unix, start);
        let stale = sweep_stale(&s, start + 601, &cfg).unwrap();
        assert_eq!(stale, vec![g.lease_id.clone()]);
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// F14: expiring a lease deletes its reservation rows too.
    #[test]
    fn expire_leases_deletes_reservations() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let start = now_unix();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            start,
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let (cpu, mem, _) = s.reserved_totals().unwrap();
        assert_eq!((cpu, mem), (1, 512));
        let expired = expire_leases(&s, start + 1801).unwrap();
        assert_eq!(expired, vec![g.lease_id.clone()]);
        let (cpu, mem, _) = s.reserved_totals().unwrap();
        assert_eq!((cpu, mem), (0, 0));
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn release_is_idempotent() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let no_stop = |_d: &str| Ok(());
        assert!(release(&s, &g.lease_id, now_unix(), &no_stop).unwrap());
        assert!(!release(&s, &g.lease_id, now_unix(), &no_stop).unwrap());
        assert_eq!(
            release(&s, "mo-nope-000000", now_unix(), &|_d: &str| Ok(()))
                .unwrap_err()
                .code,
            "E08"
        );
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn extend_rules() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now_unix(),
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        let l = extend(&s, &g.lease_id, 30, &cfg, false, now_unix()).unwrap();
        assert_eq!(l.extensions, 1);
        assert_eq!(
            extend(&s, &g.lease_id, 31, &cfg, false, now_unix())
                .unwrap_err()
                .code,
            "E09"
        );
        assert_eq!(
            extend(&s, &g.lease_id, 30, &cfg, true, now_unix())
                .unwrap_err()
                .code,
            "E09"
        );
        // 30 + 30 + 30 = 90, then 30 more = 120 ok, then over.
        extend(&s, &g.lease_id, 30, &cfg, false, now_unix()).unwrap();
        extend(&s, &g.lease_id, 30, &cfg, false, now_unix()).unwrap();
        assert_eq!(
            extend(&s, &g.lease_id, 30, &cfg, false, now_unix())
                .unwrap_err()
                .code,
            "E09"
        );
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn desktop_binding_ports() {
        assert_eq!(desktop_binding("vd1").1, Some(9333));
        assert_eq!(desktop_binding("vd10").1, Some(9342));
        assert_eq!(desktop_binding("main-display").1, Some(9222));
        assert_eq!(desktop_binding("none").1, None);
    }

    #[test]
    fn request_type_used() {
        let r = LeaseRequest::default();
        assert_eq!(r.cpu_cores, 1);
    }

    fn granted_lease() -> (Store, String, String, Config, i64) {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let req = queued_req();
        let now = now_unix();
        let g = grant(
            &s,
            &req,
            "vd1",
            &inv_ok(),
            &cfg,
            now,
            &ok_provision,
            &ok_stop,
        )
        .unwrap();
        (s, p, g.lease_id, cfg, now)
    }

    #[test]
    fn heartbeat_resets_misses_and_due() {
        let (s, p, id, cfg, now) = granted_lease();
        let l = heartbeat(&s, &id, now + 60, &cfg).unwrap();
        assert_eq!(l.heartbeat_misses, 0);
        assert_eq!(l.heartbeat_due_unix, now + 60 + 300);
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn two_missed_beats_go_stale() {
        let (s, p, id, cfg, now) = granted_lease();
        // One miss: warned, still active.
        let stale = sweep_stale(&s, now + 301, &cfg).unwrap();
        assert!(stale.is_empty());
        let l = s.get_lease(&id).unwrap().unwrap();
        assert_eq!(l.state, "active");
        assert_eq!(l.heartbeat_misses, 1);
        // Two misses: STALE.
        let stale = sweep_stale(&s, now + 601, &cfg).unwrap();
        assert_eq!(stale, vec![id.clone()]);
        assert_eq!(s.get_lease(&id).unwrap().unwrap().state, "stale");
        // A fresh heartbeat revives it.
        let l = heartbeat(&s, &id, now + 602, &cfg).unwrap();
        assert_eq!(l.state, "active");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn reclaim_releases_only_when_dead() {
        let (s, p, id, cfg, now) = granted_lease();
        let _ = sweep_stale(&s, now + 601, &cfg).unwrap();
        let after_grace = now + 601 + 5 * 60 + 1;
        // CDP alive + Chrome alive: kept.
        let probe = ReclaimProbe {
            cdp_ok: true,
            chrome_alive: true,
            lock_held: true,
        };
        assert_eq!(
            reclaim_check(&s, &id, after_grace, &cfg, &probe).unwrap(),
            ReclaimOutcome::KeptStale("CDP responds and Chrome lives".to_string())
        );
        // CDP dead + Chrome alive: kept, flagged.
        let probe = ReclaimProbe {
            cdp_ok: false,
            chrome_alive: true,
            lock_held: true,
        };
        assert!(matches!(
            reclaim_check(&s, &id, after_grace, &cfg, &probe).unwrap(),
            ReclaimOutcome::KeptStale(_)
        ));
        // CDP dead + Chrome dead: released.
        let probe = ReclaimProbe {
            cdp_ok: false,
            chrome_alive: false,
            lock_held: false,
        };
        assert_eq!(
            reclaim_check(&s, &id, after_grace, &cfg, &probe).unwrap(),
            ReclaimOutcome::Released
        );
        assert_eq!(s.get_lease(&id).unwrap().unwrap().state, "released");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn reclaim_respects_grace() {
        let (s, p, id, cfg, now) = granted_lease();
        let _ = sweep_stale(&s, now + 601, &cfg).unwrap();
        let probe = ReclaimProbe {
            cdp_ok: false,
            chrome_alive: false,
            lock_held: false,
        };
        assert!(matches!(
            reclaim_check(&s, &id, now + 602, &cfg, &probe).unwrap(),
            ReclaimOutcome::KeptStale(_)
        ));
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn expiry_marks_expired() {
        let (s, p, id, _cfg, now) = granted_lease();
        let _ = _cfg;
        let expired = expire_leases(&s, now + 1801).unwrap();
        assert_eq!(expired, vec![id.clone()]);
        assert_eq!(s.get_lease(&id).unwrap().unwrap().state, "expired");
        drop(s);
        let _ = std::fs::remove_file(p);
    }
}
