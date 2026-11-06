//! Queue: request intake, validation, idempotency, cancellation, expiry.
//! One queue per host. Persistent in SQLite. Ordered by scheduler, not here.

use crate::core::{Capabilities, LeaseRequest, MoError, QueuedRequest, Result};
use crate::store::Store;
use serde::Serialize;

/// Queued entries older than this are expired. 24 hours.
pub const QUEUE_TTL_SECS: i64 = 86400;

#[derive(Debug, Clone, Serialize)]
pub struct QueueOutcome {
    pub status: String,
    pub request_id: i64,
    pub queue_position: i64,
    pub duplicate: bool,
}

/// Validate a request. E01 missing field, E02 unknown class or missing
/// capability, E03 duration or size out of range. No state change on error.
pub fn validate_request(req: &LeaseRequest, caps: &Capabilities) -> Result<()> {
    if req.agent_id.trim().is_empty() {
        return Err(MoError::e01("agent_id is required"));
    }
    if req.job_id.trim().is_empty() {
        return Err(MoError::e01("job_id is required"));
    }
    if req.idempotency_key.trim().is_empty() {
        return Err(MoError::e01("idempotency_key is required"));
    }
    if uuid::Uuid::parse_str(&req.idempotency_key).is_err() {
        return Err(MoError::e01("idempotency_key must be a UUID"));
    }
    let class = match crate::core::ResourceClass::parse_str(&req.resource_class) {
        Some(c) => c,
        None => {
            return Err(MoError::e02(format!(
                "unknown resource_class: {}",
                req.resource_class
            )))
        }
    };
    if !(1..=4).contains(&req.cpu_cores) {
        return Err(MoError::e01("cpu_cores must be 1 to 4"));
    }
    if !(256..=4096).contains(&req.mem_mb) {
        return Err(MoError::e01("mem_mb must be 256 to 4096"));
    }
    if let Some(max) = req.mem_mb_max {
        if max < req.mem_mb {
            return Err(MoError::e01("mem_mb_max must be at least mem_mb"));
        }
    }
    // F07: one lease holds one desktop. A request for 2 desktops was
    // accepted and then granted a single one. Reject it at the door
    // until an N-desktop reserve exists.
    if !matches!(req.desktops, 0 | 1) {
        return Err(MoError::e01("desktops must be 0 or 1"));
    }
    if !(5..=120).contains(&req.duration_min) {
        return Err(MoError::e03(format!(
            "duration_min {} out of range 5 to 120",
            req.duration_min
        )));
    }
    if crate::core::PriorityClass::parse_str(&req.priority_class).is_none() {
        return Err(MoError::e01(format!(
            "unknown priority_class: {}",
            req.priority_class
        )));
    }
    if req.needs_bridge && !caps.has_bridge {
        return Err(MoError::e02("host has no bridge: needs_bridge denied"));
    }
    if (req.needs_display || req.desktops > 0) && !caps.has_desktops {
        return Err(MoError::e02("host has no desktops"));
    }
    if class == crate::core::ResourceClass::MainDisplay && !caps.has_desktops {
        return Err(MoError::e02("host has no main display"));
    }
    if let Some(d) = &req.deadline {
        if crate::core::iso_to_unix(d).is_none() {
            return Err(MoError::e01("deadline must be ISO 8601"));
        }
    }
    for cap in &req.capabilities {
        if cap.trim().is_empty() {
            return Err(MoError::e01("capabilities must not contain empty strings"));
        }
    }
    Ok(())
}

/// Submit a request. Idempotent: the same agent_id + idempotency_key returns
/// the first result and creates no second row. Returns queue position.
pub fn submit(
    store: &Store,
    req: &LeaseRequest,
    caps: &Capabilities,
    now: i64,
) -> Result<QueueOutcome> {
    validate_request(req, caps)?;
    if let Some(existing) =
        store.find_request_by_idempotency(&req.agent_id, &req.idempotency_key)?
    {
        let position = queue_position(store, existing.id)?;
        return Ok(QueueOutcome {
            status: existing.status,
            request_id: existing.id,
            queue_position: position,
            duplicate: true,
        });
    }
    let row = QueuedRequest {
        id: 0,
        agent_id: req.agent_id.clone(),
        job_id: req.job_id.clone(),
        idempotency_key: req.idempotency_key.clone(),
        resource_class: req.resource_class.clone(),
        cpu_cores: req.cpu_cores,
        mem_mb: req.mem_mb,
        desktops: req.desktops,
        duration_min: req.duration_min,
        priority_class: req.priority_class.clone(),
        deadline: req.deadline.clone(),
        needs_bridge: req.needs_bridge,
        needs_display: req.needs_display,
        mem_mb_max: req.mem_mb_max,
        lease_id: None,
        capabilities: req.capabilities.clone(),
        created_unix: now,
        status: "queued".to_string(),
    };
    let id = store.insert_request(&row)?;
    store.audit(
        "request",
        Some(&req.agent_id),
        Some(&req.job_id),
        None,
        None,
        &format!("queued as request {id}"),
        now,
    )?;
    let position = queue_position(store, id)?;
    Ok(QueueOutcome {
        status: "queued".to_string(),
        request_id: id,
        queue_position: position,
        duplicate: false,
    })
}

/// 1-based position among queued requests, by insertion order.
pub fn queue_position(store: &Store, request_id: i64) -> Result<i64> {
    let queued = store.list_queued_requests()?;
    for (i, r) in queued.iter().enumerate() {
        if r.id == request_id {
            return Ok(i as i64 + 1);
        }
    }
    Ok(0)
}

/// Cancel a queued request. Idempotent: already cancelled returns ok.
pub fn cancel(store: &Store, agent_id: &str, idempotency_key: &str, now: i64) -> Result<bool> {
    let found = store.find_request_by_idempotency(agent_id, idempotency_key)?;
    match found {
        None => Err(MoError::e01(format!(
            "no request for agent {agent_id} with that idempotency_key"
        ))),
        Some(r) => {
            if r.status == "cancelled" {
                return Ok(false);
            }
            store.update_request_status(r.id, "cancelled")?;
            store.audit(
                "cancel",
                Some(agent_id),
                Some(&r.job_id),
                None,
                None,
                &format!("request {} cancelled", r.id),
                now,
            )?;
            Ok(true)
        }
    }
}

/// Expire queued entries older than QUEUE_TTL_SECS. Returns count expired.
pub fn expire_stale(store: &Store, now: i64) -> Result<i64> {
    let queued = store.list_queued_requests()?;
    let mut n = 0;
    for r in queued {
        if now - r.created_unix > QUEUE_TTL_SECS {
            store.update_request_status(r.id, "expired")?;
            store.audit(
                "expire",
                Some(&r.agent_id),
                Some(&r.job_id),
                None,
                None,
                &format!("request {} expired after 24h", r.id),
                now,
            )?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::now_unix;

    fn caps() -> Capabilities {
        Capabilities {
            has_battery: true,
            has_desktops: true,
            has_bridge: true,
            max_vd: 10,
            max_ram_mb: 8192,
            arch: "x86_64".to_string(),
        }
    }

    fn req() -> LeaseRequest {
        LeaseRequest {
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 512,
            mem_mb_max: Some(1024),
            desktops: 1,
            duration_min: 30,
            priority_class: "P1".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            capabilities: vec![],
        }
    }

    fn tmp_store() -> (Store, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = format!("/tmp/mo-q-{nanos}.db");
        (Store::open(&p).unwrap(), p)
    }

    #[test]
    fn valid_request_passes() {
        assert!(validate_request(&req(), &caps()).is_ok());
    }

    #[test]
    fn missing_field_is_e01() {
        let mut r = req();
        r.agent_id.clear();
        assert_eq!(validate_request(&r, &caps()).unwrap_err().code, "E01");
        let mut r = req();
        r.idempotency_key = "not-a-uuid".to_string();
        assert_eq!(validate_request(&r, &caps()).unwrap_err().code, "E01");
    }

    #[test]
    fn unknown_class_is_e02() {
        let mut r = req();
        r.resource_class = "quantum".to_string();
        assert_eq!(validate_request(&r, &caps()).unwrap_err().code, "E02");
    }

    #[test]
    fn missing_capability_is_e02() {
        let mut c = caps();
        c.has_bridge = false;
        let mut r = req();
        r.needs_bridge = true;
        assert_eq!(validate_request(&r, &c).unwrap_err().code, "E02");
    }

    #[test]
    fn bad_duration_is_e03() {
        let mut r = req();
        r.duration_min = 200;
        assert_eq!(validate_request(&r, &caps()).unwrap_err().code, "E03");
    }

    #[test]
    fn submit_is_idempotent() {
        let (s, p) = tmp_store();
        let r = req();
        let a = submit(&s, &r, &caps(), now_unix()).unwrap();
        let b = submit(&s, &r, &caps(), now_unix()).unwrap();
        assert!(!a.duplicate);
        assert!(b.duplicate);
        assert_eq!(a.request_id, b.request_id);
        assert_eq!(s.list_queued_requests().unwrap().len(), 1);
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn ten_requests_queue_in_order() {
        let (s, p) = tmp_store();
        for i in 0..10 {
            let mut r = req();
            r.job_id = format!("j{i}");
            r.idempotency_key = uuid::Uuid::new_v4().to_string();
            submit(&s, &r, &caps(), now_unix()).unwrap();
        }
        let q = s.list_queued_requests().unwrap();
        assert_eq!(q.len(), 10);
        assert_eq!(q[0].job_id, "j0");
        assert_eq!(q[9].job_id, "j9");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn cancel_and_expire() {
        let (s, p) = tmp_store();
        let r = req();
        submit(&s, &r, &caps(), now_unix()).unwrap();
        assert!(cancel(&s, "scout", &r.idempotency_key, now_unix()).unwrap());
        assert!(!cancel(&s, "scout", &r.idempotency_key, now_unix()).unwrap());
        let mut r2 = req();
        r2.job_id = "old".to_string();
        r2.idempotency_key = uuid::Uuid::new_v4().to_string();
        submit(&s, &r2, &caps(), now_unix() - QUEUE_TTL_SECS - 1).unwrap();
        assert_eq!(expire_stale(&s, now_unix()).unwrap(), 1);
        drop(s);
        let _ = std::fs::remove_file(p);
    }
}
