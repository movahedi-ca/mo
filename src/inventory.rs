//! Inventory: minimal probe for TASK-004. Full read lands in TASK-005.

use crate::core::{now_unix, Inventory};

/// Minimal probe: load and RAM only. TASK-005 adds disk, Chrome, bridge, cache.
pub fn read_inventory() -> Inventory {
    let mut inv = Inventory {
        taken_unix: now_unix(),
        ..Default::default()
    };
    if let Ok(load) = std::fs::read_to_string("/proc/loadavg") {
        inv.load1 = load
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
    }
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        for line in mem.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or("");
            let val: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            match key {
                "MemAvailable:" => inv.mem_available_mb = val / 1024,
                "MemTotal:" => inv.mem_total_mb = val / 1024,
                _ => {}
            }
        }
    }
    inv
}
