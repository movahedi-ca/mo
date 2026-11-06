//! Scheduler: fit-first with head-of-line skip. V3 section 5.2, ADR-006.
//! Order: class, then deadline, then score with aging.
//! Score = class weight + wait minutes / 10. P0 100, P1 10, P2 0.
//! P2 reaches P1 after 100 minutes. Head keeps its position when skipped.
//! A skip is audited by the caller.

use crate::core::{iso_to_unix, PriorityClass, QueuedRequest};

/// Free capacity the scheduler may allocate from.
#[derive(Debug, Clone, Copy)]
pub struct FreeCapacity {
    pub cpu_cores: i64,
    pub mem_mb: i64,
    pub desktops: i64,
    /// Bridge slots free. None means the pool state is unknown.
    pub bridge_slots: Option<i64>,
}

/// One entry in the planned order.
#[derive(Debug, Clone)]
pub struct ScheduleEntry {
    pub request: QueuedRequest,
    pub score: i64,
    pub fits: bool,
    pub skip_reason: Option<String>,
}

/// Scheduler score for one request at `now`.
pub fn score(req: &QueuedRequest, now: i64) -> i64 {
    let weight = PriorityClass::parse_str(&req.priority_class)
        .map(|p| p.weight())
        .unwrap_or(0);
    let wait_min = (now - req.created_unix).max(0) / 60;
    weight + wait_min / 10
}

/// Fit check: the bundle must fit free capacity.
pub fn fits(req: &QueuedRequest, free: &FreeCapacity) -> Option<String> {
    if req.cpu_cores > free.cpu_cores {
        return Some(format!(
            "cpu {} over free {}",
            req.cpu_cores, free.cpu_cores
        ));
    }
    if req.mem_mb > free.mem_mb {
        return Some(format!("mem {} over free {}", req.mem_mb, free.mem_mb));
    }
    if req.desktops > free.desktops {
        return Some(format!(
            "desktops {} over free {}",
            req.desktops, free.desktops
        ));
    }
    if req.needs_bridge {
        match free.bridge_slots {
            Some(n) if n > 0 => {}
            Some(_) => return Some("bridge pool full".to_string()),
            None => return Some("bridge pool unknown".to_string()),
        }
    }
    None
}

/// Plan the queue: sorted by class, deadline, score; each entry marked fit.
/// The caller grants the first fitting entry and audits the skips before it.
pub fn plan(requests: &[QueuedRequest], free: &FreeCapacity, now: i64) -> Vec<ScheduleEntry> {
    let mut reqs: Vec<&QueuedRequest> = requests.iter().collect();
    reqs.sort_by(|a, b| {
        let ca = PriorityClass::parse_str(&a.priority_class).unwrap_or(PriorityClass::P2);
        let cb = PriorityClass::parse_str(&b.priority_class).unwrap_or(PriorityClass::P2);
        let class_ord = ca.cmp(&cb);
        if class_ord != std::cmp::Ordering::Equal {
            return class_ord;
        }
        let da = a.deadline.as_deref().and_then(iso_to_unix);
        let db = b.deadline.as_deref().and_then(iso_to_unix);
        let deadline_ord = match (da, db) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if deadline_ord != std::cmp::Ordering::Equal {
            return deadline_ord;
        }
        let score_ord = score(b, now).cmp(&score(a, now));
        if score_ord != std::cmp::Ordering::Equal {
            return score_ord;
        }
        a.created_unix.cmp(&b.created_unix)
    });
    reqs.into_iter()
        .map(|r| {
            let reason = fits(r, free);
            ScheduleEntry {
                score: score(r, now),
                fits: reason.is_none(),
                skip_reason: reason,
                request: r.clone(),
            }
        })
        .collect()
}

/// First fitting entry in the plan, if any. Entries before it were skipped.
pub fn first_fitting(plan: &[ScheduleEntry]) -> Option<&ScheduleEntry> {
    plan.iter().find(|e| e.fits)
}

/// Skipped entries before the first fitting one. Caller audits each.
pub fn skipped_before(plan: &[ScheduleEntry]) -> Vec<&ScheduleEntry> {
    let mut out = Vec::new();
    for e in plan {
        if e.fits {
            break;
        }
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::now_unix;

    fn req(job: &str, class: &str, created: i64, desktops: i64) -> QueuedRequest {
        QueuedRequest {
            id: 0,
            agent_id: "a".to_string(),
            job_id: job.to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 512,
            desktops,
            duration_min: 30,
            priority_class: class.to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            created_unix: created,
            status: "queued".to_string(),
        }
    }

    fn free() -> FreeCapacity {
        FreeCapacity {
            cpu_cores: 4,
            mem_mb: 4096,
            desktops: 2,
            bridge_slots: Some(2),
        }
    }

    #[test]
    fn class_orders_before_score() {
        let now = now_unix();
        let p2_old = req("old-p2", "P2", now - 3600 * 5, 1);
        let p1_new = req("new-p1", "P1", now, 1);
        let plan = plan(&[p2_old, p1_new], &free(), now);
        assert_eq!(plan[0].request.job_id, "new-p1");
        assert_eq!(plan[1].request.job_id, "old-p2");
    }

    #[test]
    fn deadline_orders_within_class() {
        let now = now_unix();
        let late = req("late", "P1", now, 1);
        let mut early = req("early", "P1", now, 1);
        early.deadline = Some("2026-11-06T15:00:00-05:00".to_string());
        let mut later = req("later", "P1", now, 1);
        later.deadline = Some("2026-11-06T16:00:00-05:00".to_string());
        let plan = plan(&[later.clone(), late, early], &free(), now);
        assert_eq!(plan[0].request.job_id, "early");
        assert_eq!(plan[1].request.job_id, "later");
    }

    #[test]
    fn aging_p2_reaches_p1_after_100_min() {
        let now = now_unix();
        let fresh_p2 = req("fresh", "P2", now, 1);
        assert_eq!(score(&fresh_p2, now), 0);
        let aged = req("aged", "P2", now - 100 * 60, 1);
        assert_eq!(score(&aged, now), 10, "P2 after 100 min equals P1 weight");
        let p0 = req("p0", "P0", now, 1);
        assert_eq!(score(&p0, now), 100);
    }

    #[test]
    fn head_of_line_skip_grants_fitting_job() {
        let now = now_unix();
        let head = req("head-needs-2-vd", "P0", now, 2);
        let next = req("next-needs-0", "P1", now, 0);
        let tight = FreeCapacity {
            desktops: 1,
            ..free()
        };
        let plan = plan(&[head, next], &tight, now);
        assert!(!plan[0].fits);
        assert!(plan[0].skip_reason.is_some());
        assert!(plan[1].fits);
        let skipped = skipped_before(&plan);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].request.job_id, "head-needs-2-vd");
        assert_eq!(first_fitting(&plan).unwrap().request.job_id, "next-needs-0");
    }

    #[test]
    fn bridge_unknown_blocks_bridge_job() {
        let now = now_unix();
        let mut r = req("bridge", "P1", now, 0);
        r.needs_bridge = true;
        let unknown = FreeCapacity {
            bridge_slots: None,
            ..free()
        };
        assert!(fits(&r, &unknown).is_some());
        let known = FreeCapacity {
            bridge_slots: Some(1),
            ..free()
        };
        assert!(fits(&r, &known).is_none());
    }
}
