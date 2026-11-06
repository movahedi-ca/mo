//! Battery: BAT0 sensor read, state machine, gates. V3 section 5.2.
//! battery-watch.sh owns the screen alert. MO reads the sensor and applies
//! gates only. Unknown sensor is Warn, never Normal. Missing sensor means
//! the host has no battery capability and gates do not apply.

use crate::core::{Battery, BatteryState, Config, MoError, PriorityClass, Result};

/// Read /sys/class/power_supply/BAT0. None when the sensor is missing.
pub fn read_battery() -> Option<Battery> {
    read_battery_at("/sys/class/power_supply/BAT0")
}

/// Read a BAT0 directory. Exposed for tests with a fake sysfs tree.
pub fn read_battery_at(base: &str) -> Option<Battery> {
    let cap_path = format!("{base}/capacity");
    let status_path = format!("{base}/status");
    if !std::path::Path::new(&cap_path).exists() && !std::path::Path::new(&status_path).exists() {
        return None;
    }
    let pct = std::fs::read_to_string(&cap_path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    let status = std::fs::read_to_string(&status_path)
        .ok()
        .unwrap_or_default();
    let charging = status.trim() == "Charging" || status.trim() == "Full";
    let cfg = Config::from_env();
    let state = state_for(pct, charging, &cfg);
    Some(Battery {
        pct,
        charging,
        present: true,
        state,
    })
}

/// State machine. Thresholds from V3 section 9.4.
pub fn state_for(pct: Option<i64>, charging: bool, cfg: &Config) -> BatteryState {
    if charging {
        return BatteryState::Recovery;
    }
    match pct {
        None => BatteryState::Unknown,
        Some(p) if p > cfg.battery_warn => BatteryState::Normal,
        Some(p) if p > cfg.battery_restrict => BatteryState::Warn,
        Some(p) if p > cfg.battery_refuse => BatteryState::Restrict,
        Some(_) => BatteryState::Refuse,
    }
}

/// Unknown sensor is Warn, never Normal. V3 section 5.2.
pub fn effective_state(b: Option<&Battery>) -> BatteryState {
    match b {
        None => BatteryState::Unknown,
        Some(b) => match b.state {
            BatteryState::Unknown => BatteryState::Warn,
            s => s,
        },
    }
}

/// Can a new lease be granted at this battery state?
/// Refuse blocks all. Restrict blocks P2. Warn blocks vd starts only.
pub fn can_new_lease(state: BatteryState, priority: PriorityClass, needs_vd: bool) -> Result<()> {
    match state {
        BatteryState::Refuse => Err(MoError::e05("battery refuse: 5% or less, not charging")),
        BatteryState::Restrict => {
            if priority == PriorityClass::P2 {
                return Err(MoError::e05("battery restrict: P2 leases blocked"));
            }
            if needs_vd {
                return Err(MoError::e05("battery restrict: new vd start blocked"));
            }
            Ok(())
        }
        BatteryState::Warn | BatteryState::Unknown => {
            if needs_vd {
                return Err(MoError::e05("battery warn: new vd start blocked"));
            }
            Ok(())
        }
        BatteryState::Normal | BatteryState::Recovery => Ok(()),
    }
}

/// Can a lease be extended at this battery state? Refuse blocks extensions.
pub fn can_extend(state: BatteryState) -> Result<()> {
    match state {
        BatteryState::Refuse => Err(MoError::e05("battery refuse: extensions blocked")),
        _ => Ok(()),
    }
}

/// Can a vd desktop be started at this battery state?
pub fn can_start_vd(state: BatteryState) -> Result<()> {
    match state {
        BatteryState::Normal | BatteryState::Recovery => Ok(()),
        BatteryState::Warn => Err(MoError::e05("battery warn: new vd start blocked")),
        BatteryState::Restrict => Err(MoError::e05("battery restrict: new vd start blocked")),
        BatteryState::Refuse => Err(MoError::e05("battery refuse: new vd start blocked")),
        BatteryState::Unknown => Err(MoError::e05("battery unknown: new vd start blocked")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bat(pct: &str, status: &str) -> (String, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = format!("/tmp/mo-bat-{nanos}");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/capacity"), pct).unwrap();
        std::fs::write(format!("{dir}/status"), status).unwrap();
        (dir.clone(), dir)
    }

    #[test]
    fn state_machine_thresholds() {
        let cfg = Config::from_env();
        assert_eq!(state_for(Some(78), false, &cfg), BatteryState::Normal);
        assert_eq!(state_for(Some(20), false, &cfg), BatteryState::Warn);
        assert_eq!(state_for(Some(11), false, &cfg), BatteryState::Warn);
        assert_eq!(state_for(Some(10), false, &cfg), BatteryState::Restrict);
        assert_eq!(state_for(Some(6), false, &cfg), BatteryState::Restrict);
        assert_eq!(state_for(Some(5), false, &cfg), BatteryState::Refuse);
        assert_eq!(state_for(Some(0), false, &cfg), BatteryState::Refuse);
        assert_eq!(state_for(Some(3), true, &cfg), BatteryState::Recovery);
        assert_eq!(state_for(None, false, &cfg), BatteryState::Unknown);
    }

    #[test]
    fn unknown_sensor_is_warn_never_normal() {
        let b = Battery {
            pct: None,
            charging: false,
            present: true,
            state: BatteryState::Unknown,
        };
        assert_eq!(effective_state(Some(&b)), BatteryState::Warn);
    }

    #[test]
    fn missing_sensor_is_none() {
        assert!(read_battery_at("/tmp/mo-no-such-bat0-xyz").is_none());
    }

    #[test]
    fn fake_sysfs_reads() {
        let (dir, base) = fake_bat("42", "Discharging");
        let b = read_battery_at(&base).unwrap();
        assert_eq!(b.pct, Some(42));
        assert!(!b.charging);
        assert_eq!(b.state, BatteryState::Normal);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gates() {
        assert!(can_new_lease(BatteryState::Normal, PriorityClass::P2, true).is_ok());
        assert!(can_new_lease(BatteryState::Refuse, PriorityClass::P0, false).is_err());
        assert_eq!(
            can_new_lease(BatteryState::Refuse, PriorityClass::P0, false)
                .unwrap_err()
                .code,
            "E05"
        );
        assert!(can_new_lease(BatteryState::Warn, PriorityClass::P1, false).is_ok());
        assert!(can_new_lease(BatteryState::Warn, PriorityClass::P1, true).is_err());
        assert!(can_new_lease(BatteryState::Restrict, PriorityClass::P2, false).is_err());
        assert!(can_new_lease(BatteryState::Restrict, PriorityClass::P1, false).is_ok());
        assert!(can_extend(BatteryState::Refuse).is_err());
        assert!(can_extend(BatteryState::Warn).is_ok());
        assert!(can_start_vd(BatteryState::Unknown).is_err());
        assert!(can_start_vd(BatteryState::Recovery).is_ok());
    }
}
