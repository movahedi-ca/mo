//! Desktops: start, stop, fold, expand, CDP/VNC/profile checks.
//! V3 section 5.2. Fold safety checklist, 7 checks. :1 hard guard, E14.
//! vd scripts are call targets only. MO never edits them.

use crate::core::{Config, Desktop, Inventory, MoError, Result};
use crate::store::Store;
use std::net::TcpStream;
use std::time::Duration;

fn vd_start_script() -> String {
    std::env::var("MO_VD_START_SCRIPT")
        .unwrap_or_else(|_| format!("{}/vd-start.sh", crate::core::jobs_dir()))
}

fn vd_stop_script() -> String {
    std::env::var("MO_VD_STOP_SCRIPT")
        .unwrap_or_else(|_| format!("{}/vd-stop.sh", crate::core::jobs_dir()))
}

/// Static desktop list with live up status from CDP.
/// Desktop bitmap cached for 5 seconds. The probe, the dashboard,
/// and the bypass check share one read instead of each opening 11
/// CDP connections.
static DESKTOP_CACHE: std::sync::Mutex<Option<(Vec<Desktop>, std::time::Instant)>> =
    std::sync::Mutex::new(None);

pub fn list_desktops_cached() -> Vec<Desktop> {
    if let Ok(guard) = DESKTOP_CACHE.lock() {
        if let Some((ds, at)) = guard.as_ref() {
            if at.elapsed() < std::time::Duration::from_secs(5) {
                return ds.clone();
            }
        }
    }
    let ds = list_desktops();
    if let Ok(mut guard) = DESKTOP_CACHE.lock() {
        *guard = Some((ds.clone(), std::time::Instant::now()));
    }
    ds
}

pub fn list_desktops() -> Vec<Desktop> {
    let mut out = vec![Desktop {
        id: "main-display".to_string(),
        display: ":1".to_string(),
        cdp_port: Some(9222),
        vnc_port: Some(5901),
        profile: None,
        up: cdp_ok(9222),
        leased: false,
    }];
    for n in 1..=10 {
        let port = 9332 + n;
        out.push(Desktop {
            id: format!("vd{n}"),
            display: format!(":{}", n + 1),
            cdp_port: Some(port),
            vnc_port: Some(5902 + n),
            profile: Some(format!("{}/chrome-vd{n}", crate::core::profile_root())),
            up: cdp_ok(port),
            leased: false,
        });
    }
    out
}

/// CDP responds on this port right now.
pub fn cdp_ok(port: i64) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// VNC responds on this port right now.
pub fn vnc_ok(port: i64) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// Wait up to `secs` for CDP to respond. 10 seconds at grant time.
pub fn wait_cdp(port: i64, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if cdp_ok(port) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A Chrome process runs with this profile path in its cmdline.
pub fn chrome_alive_for_profile(profile: &str) -> bool {
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(cmd) = std::fs::read(format!("/proc/{name}/cmdline")) {
                if String::from_utf8_lossy(&cmd).contains(profile) {
                    return true;
                }
            }
        }
    }
    false
}

/// Chrome's SingletonLock file is absent, so the profile is free.
pub fn profile_lock_free(profile: &str) -> bool {
    !std::path::Path::new(&format!("{profile}/SingletonLock")).exists()
}

/// Start a desktop via the vd script, then wait for CDP.
/// Main display is already up and is never started here.
pub fn start_desktop(id: &str) -> Result<Desktop> {
    if id == "main-display" {
        return Ok(list_desktops().into_iter().next().unwrap());
    }
    let n: i64 = id
        .trim_start_matches("vd")
        .parse()
        .map_err(|_| MoError::e02(format!("unknown desktop: {id}")))?;
    if !(1..=10).contains(&n) {
        return Err(MoError::e02(format!("unknown desktop: {id}")));
    }
    let status = std::process::Command::new(vd_start_script())
        .arg(id)
        .status()
        .map_err(|e| MoError::e04(format!("vd-start failed to run: {e}")))?;
    if !status.success() {
        return Err(MoError::e04(format!("vd-start {id} exited {status}")));
    }
    let port = 9332 + n;
    if !wait_cdp(port, 10) {
        return Err(MoError::e04(format!(
            "CDP never responded on vd{n} port {port}"
        )));
    }
    Ok(Desktop {
        id: id.to_string(),
        display: format!(":{}", n + 1),
        cdp_port: Some(port),
        vnc_port: Some(5902 + n),
        profile: Some(format!("{}/chrome-vd{n}", crate::core::profile_root())),
        up: true,
        leased: false,
    })
}

/// Provision step for a lease grant: start and wait for CDP.
/// Start a desktop for a grant. Battery-gated like expand: no new
/// vd start at Warn or worse. A host with no battery sensor has no
/// battery gate, so a battery-less host with desktops can still start one.
pub fn provision_desktop(id: &str) -> Result<()> {
    if let Some(b) = crate::battery::read_battery() {
        let state = crate::battery::effective_state(Some(&b));
        crate::battery::can_start_vd(state)?;
    }
    start_desktop(id).map(|_| ())
}

/// Stop a desktop via the vd script. One retry, then the desktop is flagged.
/// :1 is never stopped: E14.
pub fn stop_desktop(id: &str) -> Result<()> {
    if id == "main-display" {
        return Err(MoError::e14("main display :1 is protected"));
    }
    for attempt in 0..2 {
        match std::process::Command::new(vd_stop_script())
            .arg(id)
            .status()
        {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => {
                if attempt == 1 {
                    return Err(MoError::e04(format!("vd-stop {id} exited {s}")));
                }
            }
            Err(e) => {
                if attempt == 1 {
                    return Err(MoError::e04(format!("vd-stop failed to run: {e}")));
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(MoError::e04(format!("vd-stop {id} failed")))
}

/// Fold safety checklist. All 7 must pass. V3 section 5.2.
#[derive(Debug, Clone)]
pub struct FoldReport {
    pub desktop: String,
    pub no_active_lease: bool,
    pub no_stale_lease: bool,
    pub no_recent_heartbeat: bool,
    pub no_chrome_process: bool,
    pub cdp_dead: bool,
    pub profile_lock_free: bool,
    pub not_main_display: bool,
}

impl FoldReport {
    pub fn all_pass(&self) -> bool {
        self.no_active_lease
            && self.no_stale_lease
            && self.no_recent_heartbeat
            && self.no_chrome_process
            && self.cdp_dead
            && self.profile_lock_free
            && self.not_main_display
    }
    pub fn failed_checks(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.no_active_lease {
            out.push("active lease");
        }
        if !self.no_stale_lease {
            out.push("stale lease");
        }
        if !self.no_recent_heartbeat {
            out.push("recent heartbeat");
        }
        if !self.no_chrome_process {
            out.push("chrome process lives");
        }
        if !self.cdp_dead {
            out.push("CDP responds");
        }
        if !self.profile_lock_free {
            out.push("profile lock held");
        }
        if !self.not_main_display {
            out.push("main display");
        }
        out
    }
}

pub fn fold_check(store: &Store, desktop: &str, now: i64, cfg: &Config) -> Result<FoldReport> {
    let (_, _, _, profile) = crate::lease::desktop_binding(desktop);
    let active = store.list_leases(&["active", "granted", "reserving", "extension_pending"])?;
    let stale = store.list_leases(&["stale"])?;
    let has_active = active.iter().any(|l| l.desktop == desktop);
    let has_stale = stale.iter().any(|l| l.desktop == desktop);
    let mut recent_heartbeat = false;
    for l in active.iter().chain(stale.iter()) {
        if l.desktop == desktop {
            let last = if l.last_heartbeat_unix > 0 {
                l.last_heartbeat_unix
            } else {
                l.start_unix
            };
            if now - last < cfg.idle_fold_min * 60 {
                recent_heartbeat = true;
            }
        }
    }
    let profile_str = profile.clone().unwrap_or_default();
    let chrome_alive = if profile_str.is_empty() {
        false
    } else {
        chrome_alive_for_profile(&profile_str)
    };
    let lock_free = if profile_str.is_empty() {
        true
    } else {
        profile_lock_free(&profile_str)
    };
    let (_, cdp_port, _, _) = crate::lease::desktop_binding(desktop);
    let cdp_dead = match cdp_port {
        Some(p) => !cdp_ok(p),
        None => true,
    };
    Ok(FoldReport {
        desktop: desktop.to_string(),
        no_active_lease: !has_active,
        no_stale_lease: !has_stale,
        no_recent_heartbeat: !recent_heartbeat,
        no_chrome_process: !chrome_alive,
        cdp_dead,
        profile_lock_free: lock_free,
        not_main_display: desktop != "main-display",
    })
}

/// Fold a desktop. Refused with E13 when any check fails, E14 for :1.
pub fn fold_desktop(store: &Store, desktop: &str, now: i64, cfg: &Config) -> Result<()> {
    if desktop == "main-display" {
        return Err(MoError::e14("main display :1 is protected"));
    }
    // DEFECT-004: a desktop whose stop failed is flagged. Fold refuses
    // it until an operator clears the flag with `mo-cli clear-flag`.
    if store.is_flagged(desktop)? {
        return Err(MoError::e13(format!(
            "fold refused for {desktop}: desktop is flagged, clear the flag first"
        )));
    }
    let report = fold_check(store, desktop, now, cfg)?;
    if !report.all_pass() {
        return Err(MoError::e13(format!(
            "fold refused for {desktop}: {}",
            report.failed_checks().join(", ")
        )));
    }
    if let Err(e) = stop_desktop(desktop) {
        let reason = format!("stop failed on fold: {}", e.message);
        let _ = store.audit("stop_failed", None, None, None, Some(desktop), &reason, now);
        let _ = store.flag_desktop(desktop, &reason, now);
        return Err(e);
    }
    store.audit(
        "fold",
        None,
        None,
        None,
        Some(desktop),
        "desktop folded, all 7 checks passed",
        now,
    )?;
    Ok(())
}

/// Expand rule: queue depth, MemAvailable over guard plus one Chrome
/// budget, active vd under sustained. The 1500 MB literal is gone.
pub fn should_expand(queue_depth: i64, inv: &Inventory, cfg: &Config, active_vd: i64) -> bool {
    queue_depth >= cfg.expand_queue_depth
        && inv.mem_available_mb > cfg.ram_guard_mb + cfg.chrome_budget_mb
        && active_vd < cfg.sustained_vd
}

/// Expand one desktop: the first vd that is down. None when all are up.
/// Battery-gated: no new vd start at Warn or worse. A host with no
/// battery sensor has no battery gate.
pub fn expand_one(store: &Store, now: i64) -> Result<Option<String>> {
    let _ = store;
    let _ = now;
    if let Some(b) = crate::battery::read_battery() {
        let state = crate::battery::effective_state(Some(&b));
        crate::battery::can_start_vd(state)?;
    }
    for d in list_desktops() {
        if d.id != "main-display" && !d.up {
            start_desktop(&d.id)?;
            return Ok(Some(d.id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::now_unix;

    #[test]
    fn list_has_main_plus_ten_vd() {
        let ds = list_desktops();
        assert_eq!(ds.len(), 11);
        assert_eq!(ds[0].id, "main-display");
        assert_eq!(ds[1].id, "vd1");
        assert_eq!(ds[10].id, "vd10");
        assert_eq!(ds[1].cdp_port, Some(9333));
        assert_eq!(ds[1].vnc_port, Some(5903));
    }

    /// DEFECT-004: a flagged desktop is refused by fold with E13, and a
    /// stop failure on fold audits "stop_failed" and flags the desktop.
    #[test]
    fn fold_refuses_flagged_desktop() {
        let now = now_unix();
        let p = format!(
            "/tmp/mo-d-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let s = Store::open(&p).unwrap();
        let cfg = Config::from_env();
        s.flag_desktop("vd1", "stop failed on release", now)
            .unwrap();
        let err = fold_desktop(&s, "vd1", now, &cfg).unwrap_err();
        assert_eq!(err.code, "E13");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    /// F11: provision refuses to start a desktop at battery Warn.
    #[test]
    fn provision_blocked_at_battery_warn() {
        use crate::core::{Battery, BatteryState};
        crate::battery::set_test_battery(Some(Battery {
            pct: Some(15),
            charging: false,
            present: true,
            state: BatteryState::Warn,
        }));
        let err = provision_desktop("vd1").unwrap_err();
        assert_eq!(err.code, "E05");
        crate::battery::set_test_battery(None);
    }

    #[test]
    fn fold_main_display_is_e14() {
        let p = format!(
            "/tmp/mo-d-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let s = Store::open(&p).unwrap();
        let cfg = Config::from_env();
        let err = fold_desktop(&s, "main-display", now_unix(), &cfg).unwrap_err();
        assert_eq!(err.code, "E14");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn fold_check_passes_on_idle_desktop() {
        let p = format!(
            "/tmp/mo-d2-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let s = Store::open(&p).unwrap();
        let cfg = Config::from_env();
        // vd9 has no lease, no Chrome, CDP dead on this host, lock free.
        let r = fold_check(&s, "vd9", now_unix(), &cfg).unwrap();
        assert!(r.no_active_lease);
        assert!(r.no_stale_lease);
        assert!(r.no_recent_heartbeat);
        assert!(r.profile_lock_free);
        assert!(r.not_main_display);
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn fold_refused_when_active_lease() {
        let p = format!(
            "/tmp/mo-d3-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let s = Store::open(&p).unwrap();
        let cfg = Config::from_env();
        let now = now_unix();
        let lease = crate::core::Lease {
            lease_id: "mo-t-1".to_string(),
            agent_id: "a".to_string(),
            job_id: "j".to_string(),
            resource_class: "vd-chrome".to_string(),
            desktop: "vd1".to_string(),
            display: ":2".to_string(),
            cdp_port: Some(9333),
            vnc_port: Some(5903),
            profile: None,
            cpu_cores: 1,
            mem_mb: 512,
            state: "active".to_string(),
            start_unix: now,
            end_unix: now + 1800,
            heartbeat_due_unix: now + 300,
            heartbeat_misses: 0,
            extensions: 0,
            last_heartbeat_unix: 0,
            created_unix: now,
        };
        s.with_tx(|tx| {
            Store::insert_lease_tx(tx, &lease)?;
            Ok(())
        })
        .unwrap();
        let r = fold_check(&s, "vd1", now, &cfg).unwrap();
        assert!(!r.no_active_lease);
        assert!(!r.all_pass());
        assert_eq!(fold_desktop(&s, "vd1", now, &cfg).unwrap_err().code, "E13");
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn expand_rule() {
        let cfg = Config::from_env();
        // Threshold is guard + one Chrome budget: 1200 + 1024 = 2224.
        assert_eq!(cfg.ram_guard_mb + cfg.chrome_budget_mb, 2224);
        let inv = Inventory {
            mem_available_mb: 3000,
            ..Default::default()
        };
        assert!(should_expand(2, &inv, &cfg, 3));
        assert!(!should_expand(1, &inv, &cfg, 3));
        let low = Inventory {
            mem_available_mb: 2000,
            ..Default::default()
        };
        assert!(!should_expand(5, &low, &cfg, 3));
        assert!(!should_expand(5, &inv, &cfg, 6));
    }

    #[test]
    fn start_unknown_desktop_is_e02() {
        assert_eq!(start_desktop("vd99").unwrap_err().code, "E02");
    }

    #[test]
    fn cdp_dead_on_unused_port() {
        assert!(!cdp_ok(59999));
    }
}
