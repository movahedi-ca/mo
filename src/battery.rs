//! Battery: minimal read for TASK-004. State machine lands in TASK-011.

use crate::core::Battery;

/// Battery read. Full sensor read lands in TASK-011.
pub fn read_battery() -> Option<Battery> {
    let _ = std::fs::read_to_string("/sys/class/power_supply/BAT0/capacity").ok()?;
    Some(Battery {
        pct: None,
        charging: false,
        present: true,
        state: crate::core::BatteryState::Unknown,
    })
}
