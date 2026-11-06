//! Audit events and status snapshots. One audit source: the store.
//! The dashboard and the CLI status command both read from here.

use crate::core::{now_unix, Config, Inventory};
use crate::store::Store;
use serde_json::{json, Value};

/// Status snapshot: queue, leases, desktops, battery, bridge, load, RAM, disk.
/// Used by `mo-cli status` and by the dashboard generator.
pub fn snapshot(store: &Store, inv: &Inventory, _cfg: &Config) -> Value {
    let queued = store.list_queued_requests().unwrap_or_default();
    let active = store
        .list_leases(&[
            "active",
            "granted",
            "reserving",
            "stale",
            "extension_pending",
        ])
        .unwrap_or_default();
    let (battery_pct, battery_state) = match &inv.battery {
        Some(b) => (
            b.pct,
            crate::battery::effective_state(Some(b))
                .as_str()
                .to_string(),
        ),
        None => (None, "Unknown".to_string()),
    };
    let alerts: Vec<String> = store
        .list_audit(20)
        .unwrap_or_default()
        .into_iter()
        .filter(|a| {
            a.event == "stale"
                || a.event == "reclaim_flagged"
                || a.event == "grant_failed"
                || a.event == "recovery_rollback"
        })
        .map(|a| format!("{}: {}", a.event, a.detail))
        .collect();
    json!({
        "daemon": "up",
        "queue_depth": queued.len(),
        "active_leases": active.len(),
        "desktops_up": inv.desktops_up,
        "mem_available_mb": inv.mem_available_mb,
        "mem_total_mb": inv.mem_total_mb,
        "load1": inv.load1,
        "disk_free_gb": inv.disk_free_gb,
        "chrome_count": inv.chrome_count,
        "battery_pct": battery_pct,
        "battery_state": battery_state,
        "bridge_pool_free": inv.bridge_pool_free,
        "bridge_known": inv.bridge_known,
        "alerts": alerts,
        "taken_unix": inv.taken_unix,
        "now_unix": now_unix(),
    })
}
