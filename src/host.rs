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

/// Pick an adapter by host name. Unknown host is E12.
pub fn adapter_for(host: &str) -> Result<Box<dyn HostAdapter>> {
    match host {
        "chromebook-vm" | "chromebook" => Ok(Box::new(ChromebookAdapter)),
        "oracle-a1" | "oracle" => Ok(Box::new(OracleAdapter)),
        "generic" => Ok(Box::new(GenericAdapter::new(Capabilities {
            has_battery: false,
            has_desktops: false,
            has_bridge: false,
            max_vd: 0,
            max_ram_mb: 8192,
            arch: std::env::consts::ARCH.to_string(),
        }))),
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
}
