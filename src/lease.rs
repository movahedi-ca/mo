//! Lease manager: two-phase reservation. V3 section 5.2, ADR-005.
//! Phase 1 reserve in one transaction, phase 2 provision, phase 3 commit
//! or roll back. No partial grant survives a failure.
//! Heartbeat and reclaim land in TASK-009, in this module.

use crate::core::{
    new_lease_id, now_unix, unix_to_iso, Config, Grant, Inventory, Lease, LeaseRequest, MoError,
    QueuedRequest, Result,
};
use crate::store::Store;

/// Pick a free desktop for a request. None when no slot is free.
/// "none" is returned for classes that need no desktop.
pub fn pick_desktop(store: &Store, req: &QueuedRequest) -> Result<Option<String>> {
    if req.desktops == 0 {
        return Ok(Some("none".to_string()));
    }
    let (_, _, held) = store.reserved_totals()?;
    let want_main = req.resource_class == "main-display" || req.needs_display;
    if want_main {
        if held.iter().any(|d| d == "main-display") {
            return Ok(None);
        }
        return Ok(Some("main-display".to_string()));
    }
    for d in crate::desktops::list_desktops() {
        if d.id == "main-display" {
            continue;
        }
        if !held.contains(&d.id) {
            return Ok(Some(d.id));
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
                Some(format!("/home/auto/.config/chrome-vd{n}")),
            )
        }
    }
}

/// Grant a lease through two-phase reservation.
/// `provision` starts the desktop and waits for CDP (up to 10s).
/// `stop` stops a desktop after a failed provision.
/// Both are closures so tests can inject failures without a live desktop.
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
        // Unique index on desktop + active lease turns a race into E04.
        return Err(MoError::e04(format!(
            "desktop {desktop} busy, no free slot: {}",
            e.message
        )));
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
    store.with_tx(|tx| {
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
    })?;
    Ok(Grant {
        lease_id,
        desktop: desktop.to_string(),
        display,
        cdp_port,
        vnc_port,
        profile,
        start: unix_to_iso(start, -5),
        end: unix_to_iso(end, -5),
        heartbeat_due: unix_to_iso(now + cfg.heartbeat_min * 60, -5),
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
    let _ = stop(desktop);
}

/// Release a lease. Idempotent: a second release returns ok, no error.
pub fn release(store: &Store, lease_id: &str, now: i64) -> Result<bool> {
    let lease = match store.get_lease(lease_id)? {
        Some(l) => l,
        None => return Err(MoError::e08(format!("lease not found: {lease_id}"))),
    };
    if lease.state == "released" {
        return Ok(false);
    }
    store.with_tx(|tx| {
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
        created_unix: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::LeaseRequest;

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
        assert!(release(&s, &g.lease_id, now_unix()).unwrap());
        assert!(!release(&s, &g.lease_id, now_unix()).unwrap());
        assert_eq!(
            release(&s, "mo-nope-000000", now_unix()).unwrap_err().code,
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
}
