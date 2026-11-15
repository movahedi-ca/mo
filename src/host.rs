//! Host adapters. V3 section 5.2.
//! Core is host free. Adapters carry host facts.
//! A host without a capability must not pretend to support it: E02.

use crate::core::{Battery, Capabilities, Desktop, Inventory, MoError, Result};

/// HostAdapter trait, exact from V3 section 5.2.
pub trait HostAdapter {
    fn name(&self) -> &str;
    fn probe(&self) -> Inventory;
    fn list_desktops(&self) -> Vec<Desktop>;
    fn start_desktop(&self, id: &str) -> Result<Desktop>;
    fn stop_desktop(&self, id: &str) -> Result<()>;
    fn battery(&self) -> Option<Battery>;
    fn capabilities(&self) -> Capabilities;
}

/// chromebook-vm: Asus Chromebook Flip C433T, Debian 13, 8 GB RAM.
pub struct ChromebookAdapter;

impl HostAdapter for ChromebookAdapter {
    fn name(&self) -> &str {
        "chromebook-vm"
    }
    fn probe(&self) -> Inventory {
        crate::inventory::read_inventory()
    }
    fn list_desktops(&self) -> Vec<Desktop> {
        crate::desktops::list_desktops()
    }
    fn start_desktop(&self, id: &str) -> Result<Desktop> {
        crate::desktops::start_desktop(id)
    }
    fn stop_desktop(&self, id: &str) -> Result<()> {
        crate::desktops::stop_desktop(id)
    }
    fn battery(&self) -> Option<Battery> {
        crate::battery::read_battery()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_battery: true,
            has_desktops: true,
            has_bridge: true,
            max_vd: 10,
            max_ram_mb: 8192,
            arch: "x86_64".to_string(),
        }
    }
}

/// Oracle A1: Ampere ARM64, mains host, no battery, no desktops, no bridge yet.
pub struct OracleAdapter;

impl HostAdapter for OracleAdapter {
    fn name(&self) -> &str {
        "oracle-a1"
    }
    fn probe(&self) -> Inventory {
        crate::inventory::read_inventory()
    }
    fn list_desktops(&self) -> Vec<Desktop> {
        Vec::new()
    }
    fn start_desktop(&self, _id: &str) -> Result<Desktop> {
        Err(MoError::e02(
            "oracle-a1 has no desktops: capability missing",
        ))
    }
    fn stop_desktop(&self, _id: &str) -> Result<()> {
        Err(MoError::e02(
            "oracle-a1 has no desktops: capability missing",
        ))
    }
    fn battery(&self) -> Option<Battery> {
        None
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_battery: false,
            has_desktops: false,
            has_bridge: false,
            max_vd: 0,
            max_ram_mb: 12288,
            arch: "aarch64".to_string(),
        }
    }
}

/// Generic host: capabilities supplied by the caller. No capability is faked.
pub struct GenericAdapter {
    caps: Capabilities,
}

impl GenericAdapter {
    pub fn new(caps: Capabilities) -> Self {
        Self { caps }
    }
}

impl HostAdapter for GenericAdapter {
    fn name(&self) -> &str {
        "generic"
    }
    fn probe(&self) -> Inventory {
        crate::inventory::read_inventory()
    }
    fn list_desktops(&self) -> Vec<Desktop> {
        if self.caps.has_desktops {
            crate::desktops::list_desktops()
        } else {
            Vec::new()
        }
    }
    fn start_desktop(&self, id: &str) -> Result<Desktop> {
        if self.caps.has_desktops {
            crate::desktops::start_desktop(id)
        } else {
            Err(MoError::e02("generic host has no desktops"))
        }
    }
    fn stop_desktop(&self, id: &str) -> Result<()> {
        if self.caps.has_desktops {
            crate::desktops::stop_desktop(id)
        } else {
            Err(MoError::e02("generic host has no desktops"))
        }
    }
    fn battery(&self) -> Option<Battery> {
        if self.caps.has_battery {
            crate::battery::read_battery()
        } else {
            None
        }
    }
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }
}

/// Default power supply sysfs dir.
pub const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";

/// True when the power supply dir holds a battery: an entry whose
/// name starts with BAT and whose type file reads Battery.
/// A missing dir is false, never an error: containers and VMs
/// without the dir simply have no battery.
pub fn power_supply_has_battery(dir: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("BAT") {
            continue;
        }
        let ty = std::fs::read_to_string(entry.path().join("type")).unwrap_or_default();
        if ty.trim() == "Battery" {
            return true;
        }
    }
    false
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

/// Generic Linux host: facts are detected, never hardcoded.
/// Battery comes from a sysfs scan. The RAM cap comes from the
/// inventory probe of /proc/meminfo, with an 8192 MB fallback cap
/// when the probe returns 0. Desktops stay off unless the operator
/// sets MO_GENERIC_DESKTOPS=1 and the host has the vd scripts.
/// There is no bridge on a plain host.
pub struct GenericLinuxAdapter {
    caps: Capabilities,
}

impl GenericLinuxAdapter {
    pub fn detect() -> Self {
        Self::detect_at(POWER_SUPPLY_DIR, env_flag("MO_GENERIC_DESKTOPS"))
    }

    /// Detection with an explicit power supply dir and desktop
    /// switch. Tests use a fake dir and never touch process env.
    pub fn detect_at(power_dir: &str, desktops_enabled: bool) -> Self {
        let has_battery = power_supply_has_battery(power_dir);
        let total = crate::inventory::read_inventory().mem_total_mb;
        let max_ram_mb = if total > 0 { total } else { 8192 };
        let max_vd = if desktops_enabled {
            crate::core::Config::from_env().max_vd
        } else {
            0
        };
        Self {
            caps: Capabilities {
                has_battery,
                has_desktops: desktops_enabled,
                has_bridge: false,
                max_vd,
                max_ram_mb,
                arch: std::env::consts::ARCH.to_string(),
            },
        }
    }

    pub fn new(caps: Capabilities) -> Self {
        Self { caps }
    }
}

impl HostAdapter for GenericLinuxAdapter {
    fn name(&self) -> &str {
        "generic-linux"
    }
    fn probe(&self) -> Inventory {
        crate::inventory::read_inventory()
    }
    fn list_desktops(&self) -> Vec<Desktop> {
        if self.caps.has_desktops {
            crate::desktops::list_desktops()
        } else {
            Vec::new()
        }
    }
    fn start_desktop(&self, id: &str) -> Result<Desktop> {
        if self.caps.has_desktops {
            crate::desktops::start_desktop(id)
        } else {
            Err(MoError::e02("generic-linux host has no desktops"))
        }
    }
    fn stop_desktop(&self, id: &str) -> Result<()> {
        if self.caps.has_desktops {
            crate::desktops::stop_desktop(id)
        } else {
            Err(MoError::e02("generic-linux host has no desktops"))
        }
    }
    fn battery(&self) -> Option<Battery> {
        if self.caps.has_battery {
            crate::battery::read_battery()
        } else {
            None
        }
    }
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }
}

/// Pick an adapter by host name. Unknown host is E12.
pub fn adapter_for(host: &str) -> Result<Box<dyn HostAdapter + Send + Sync>> {
    match host {
        "chromebook-vm" | "chromebook" => Ok(Box::new(ChromebookAdapter)),
        "oracle-a1" | "oracle" => Ok(Box::new(OracleAdapter)),
        "generic-linux" | "generic" | "linux" => Ok(Box::new(GenericLinuxAdapter::detect())),
        other => Err(MoError::e12(format!("unknown host: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromebook_capabilities() {
        let a = ChromebookAdapter;
        let c = a.capabilities();
        assert!(c.has_battery && c.has_desktops && c.has_bridge);
        assert_eq!(c.max_vd, 10);
        assert_eq!(c.arch, "x86_64");
    }

    #[test]
    fn oracle_has_no_battery_no_desktops() {
        let a = OracleAdapter;
        let c = a.capabilities();
        assert!(!c.has_battery && !c.has_desktops && !c.has_bridge);
        assert_eq!(c.max_vd, 0);
        assert!(a.battery().is_none());
        assert!(a.list_desktops().is_empty());
        assert!(a.start_desktop("vd1").is_err());
        assert_eq!(a.start_desktop("vd1").unwrap_err().code, "E02");
    }

    #[test]
    fn unknown_host_is_e12() {
        assert!(adapter_for("nope").is_err());
        assert_eq!(adapter_for("nope").err().unwrap().code, "E12");
    }

    #[test]
    fn adapter_for_known_hosts() {
        assert_eq!(
            adapter_for("chromebook-vm").unwrap().name(),
            "chromebook-vm"
        );
        assert_eq!(adapter_for("oracle-a1").unwrap().name(), "oracle-a1");
    }

    fn fake_power_supply(entries: &[(&str, &str)]) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = format!("/tmp/mo-ps-{nanos}");
        for (name, ty) in entries {
            let sub = format!("{dir}/{name}");
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(format!("{sub}/type"), ty).unwrap();
        }
        if entries.is_empty() {
            std::fs::create_dir_all(&dir).unwrap();
        }
        dir
    }

    #[test]
    fn power_supply_scan_finds_battery() {
        let dir = fake_power_supply(&[("AC", "Mains"), ("BAT0", "Battery")]);
        assert!(power_supply_has_battery(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn power_supply_scan_ac_only_is_false() {
        let dir = fake_power_supply(&[("AC", "Mains"), ("ADP1", "Mains")]);
        assert!(!power_supply_has_battery(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn power_supply_scan_missing_dir_is_false() {
        assert!(!power_supply_has_battery("/tmp/mo-no-such-ps-dir-xyz"));
    }

    #[test]
    fn generic_linux_aliases() {
        for host in ["generic-linux", "generic", "linux"] {
            let a = adapter_for(host).unwrap();
            assert_eq!(a.name(), "generic-linux", "host {host}");
        }
    }

    #[test]
    fn generic_linux_detect_matches_host() {
        let a = GenericLinuxAdapter::detect();
        let c = a.capabilities();
        let inv = crate::inventory::read_inventory();
        if inv.mem_total_mb > 0 {
            assert!(
                (c.max_ram_mb - inv.mem_total_mb).abs() <= 64,
                "cap {} vs total {}",
                c.max_ram_mb,
                inv.mem_total_mb
            );
        }
        assert_eq!(c.has_battery, power_supply_has_battery(POWER_SUPPLY_DIR));
        assert!(!c.has_bridge);
        assert_eq!(c.arch, std::env::consts::ARCH);
    }

    #[test]
    fn generic_linux_desktops_off_by_default() {
        let a = GenericLinuxAdapter::detect_at("/tmp/mo-no-such-ps-dir-xyz", false);
        let c = a.capabilities();
        assert!(!c.has_battery);
        assert!(!c.has_desktops);
        assert_eq!(c.max_vd, 0);
        assert!(a.list_desktops().is_empty());
        assert_eq!(a.start_desktop("vd1").unwrap_err().code, "E02");
        assert_eq!(a.stop_desktop("vd1").unwrap_err().code, "E02");
        assert!(a.battery().is_none());
    }
}
