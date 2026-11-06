//! Inventory: /proc, /sys, ps, bridge pool. V3 section 5.2.
//! One probe task, single-flight: concurrent callers share one probe
//! instead of each paying for df, a /proc walk, and 11 CDP connects.
//! The snapshot cache honors MO_INVENTORY_CACHE_SEC (default 60).
//! Grant reads the snapshot plus a MemAvailable-only re-read.
//! The bridge is probed only when a queued request needs it, one
//! attempt, with a 60 s negative cache. Disk free comes from statvfs
//! on the filesystem that holds mo.db, not from df on "/".

use crate::core::{now_unix, Config, Inventory};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static SNAP: Mutex<Option<(Inventory, Instant)>> = Mutex::new(None);

/// Test-only fixed snapshot. Grant tests assert on capacity, so they
/// run against this instead of live load, which spikes while cargo
/// builds in parallel and makes grants flake. The override is
/// checked before the cache, so it always wins while set.
#[cfg(test)]
static TEST_INVENTORY: Mutex<Option<Option<Inventory>>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_test_inventory(inv: Option<Inventory>) {
    *TEST_INVENTORY.lock().unwrap() = Some(inv);
}

/// One lock for every test that touches the snapshot or sets the
/// override. Hoisted to module level so server grant tests can
/// hold it too.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());
static PROBE_LOCK: Mutex<()> = Mutex::new(());
static CACHE_TTL_SECS: OnceLock<i64> = OnceLock::new();
static DB_DIR: OnceLock<String> = OnceLock::new();
static BRIDGE_NEG: Mutex<Option<Instant>> = Mutex::new(None);
const BRIDGE_HOST: &str = "127.0.0.1:8899";

fn cache_ttl() -> Duration {
    Duration::from_secs((*CACHE_TTL_SECS.get().unwrap_or(&60)).max(1) as u64)
}

fn db_dir() -> String {
    DB_DIR.get().cloned().unwrap_or_else(|| "/".to_string())
}

/// Set the snapshot cache TTL in seconds. Call once at daemon start.
/// After the first set, later calls are ignored.
pub fn set_cache_ttl(secs: i64) {
    let _ = CACHE_TTL_SECS.set(secs);
}

/// Set the directory whose filesystem the disk guard measures.
/// Call once at daemon start with the parent of the DB path.
pub fn set_db_dir(dir: &str) {
    let _ = DB_DIR.set(dir.to_string());
}

/// Cached snapshot. Daemon status path uses this.
pub fn read_inventory() -> Inventory {
    read_single_flight(false, false, &Config::from_env())
}

/// Fresh read, bypasses the cache. Bridge is not probed.
pub fn read_inventory_fresh() -> Inventory {
    read_single_flight(true, false, &Config::from_env())
}

/// Fresh read with the daemon config and DB directory. Bridge not probed.
pub fn read_inventory_fresh_with(cfg: &Config, dir: &str) -> Inventory {
    read_single_flight(true, false, cfg_with_dir(cfg, dir))
}

/// Fresh read that also attempts one bridge probe.
pub fn read_inventory_fresh_bridge(cfg: &Config, dir: &str) -> Inventory {
    read_single_flight(true, true, cfg_with_dir(cfg, dir))
}

fn cfg_with_dir<'a>(cfg: &'a Config, dir: &str) -> &'a Config {
    set_db_dir(dir);
    cfg
}

/// Single-flight probe: the first caller probes, the rest wait on the
/// lock and then read the fresh snapshot. No two probes run at once.
fn read_single_flight(force: bool, need_bridge: bool, cfg: &Config) -> Inventory {
    #[cfg(test)]
    if let Ok(guard) = TEST_INVENTORY.lock() {
        if let Some(Some(inv)) = &*guard {
            return inv.clone();
        }
    }
    if !force {
        if let Ok(guard) = SNAP.lock() {
            if let Some((inv, at)) = guard.as_ref() {
                if at.elapsed() < cache_ttl() {
                    return inv.clone();
                }
            }
        }
    }
    let _probe = PROBE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !force {
        if let Ok(guard) = SNAP.lock() {
            if let Some((inv, at)) = guard.as_ref() {
                if at.elapsed() < cache_ttl() {
                    return inv.clone();
                }
            }
        }
    }
    let inv = probe(need_bridge, cfg);
    if let Ok(mut guard) = SNAP.lock() {
        *guard = Some((inv.clone(), Instant::now()));
    }
    inv
}

fn probe(need_bridge: bool, cfg: &Config) -> Inventory {
    let mut inv = Inventory {
        taken_unix: now_unix(),
        ..Default::default()
    };
    inv.load1 = read_load();
    let (avail, total) = read_mem();
    inv.mem_available_mb = avail;
    inv.mem_total_mb = total;
    inv.disk_free_gb = read_disk_free_gb(&db_dir());
    inv.chrome_count = count_chrome();
    inv.desktops_up = crate::desktops::list_desktops_cached()
        .iter()
        .filter(|d| d.up)
        .count() as i64;
    if need_bridge {
        let (free, known) = read_bridge_pool();
        inv.bridge_pool_free = free;
        inv.bridge_known = known;
    } else if let Ok(guard) = SNAP.lock() {
        // Keep the last known bridge state instead of clearing it.
        if let Some((old, _)) = guard.as_ref() {
            inv.bridge_pool_free = old.bridge_pool_free;
            inv.bridge_known = old.bridge_known;
        }
    }
    inv.battery = crate::battery::read_battery_at("/sys/class/power_supply/BAT0", cfg);
    inv
}

/// MemAvailable only. The grant gate re-reads this instead of a full probe.
pub fn fresh_mem_available_mb() -> i64 {
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        for line in mem.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                return rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0)
                    / 1024;
            }
        }
    }
    0
}

fn read_load() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

fn read_mem() -> (i64, i64) {
    let mut avail = 0;
    let mut total = 0;
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        for line in mem.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or("");
            let val: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            match key {
                "MemAvailable:" => avail = val / 1024,
                "MemTotal:" => total = val / 1024,
                _ => {}
            }
        }
    }
    (avail, total)
}

/// Disk free in GB via statvfs on the DB filesystem. No df subprocess.
fn read_disk_free_gb(path: &str) -> f64 {
    let cpath = match std::ffi::CString::new(path) {
        Ok(c) => c,
        Err(_) => return 0.0,
    };
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr(), &mut st) != 0 {
            return 0.0;
        }
        st.f_bavail as f64 * st.f_frsize as f64 / 1e9
    }
}

/// Pids whose /proc/PID/stat comm is exactly "chrome". Comm is read
/// first; cmdline is never opened for non-Chrome processes.
fn chrome_pids() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) {
                let comm = stat
                    .find('(')
                    .and_then(|a| stat.rfind(')').map(|b| &stat[a + 1..b]));
                if comm == Some("chrome") {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// Count Chrome processes. Comm-first, no cmdline walk.
fn count_chrome() -> i64 {
    chrome_pids().len() as i64
}

/// One bridge pool read. One attempt only, with a 60 s negative cache,
/// so a down bridge costs one fast refused connection, not 600 ms of
/// retries. GET /health is auth exempt. Unknown stays unknown.
fn read_bridge_pool() -> (Option<i64>, bool) {
    if let Ok(guard) = BRIDGE_NEG.lock() {
        if let Some(at) = *guard {
            if at.elapsed() < Duration::from_secs(60) {
                return (None, false);
            }
        }
    }
    match bridge_health_once() {
        Ok((free, known)) => {
            if let Ok(mut guard) = BRIDGE_NEG.lock() {
                *guard = None;
            }
            (free, known)
        }
        Err(_) => {
            if let Ok(mut guard) = BRIDGE_NEG.lock() {
                *guard = Some(Instant::now());
            }
            (None, false)
        }
    }
}

fn bridge_health_once() -> std::result::Result<(Option<i64>, bool), std::io::Error> {
    let mut stream = TcpStream::connect_timeout(
        &BRIDGE_HOST.parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad bridge addr")
        })?,
        Duration::from_secs(1),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;
    // Capped read: a body over 64 KiB is an error, never an unbounded buffer.
    let mut buf = Vec::new();
    stream.take(65_537).read_to_end(&mut buf)?;
    if buf.len() > 65_536 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bridge body too large",
        ));
    }
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let free = v
        .get("pool_free")
        .or_else(|| v.get("bridge_pool_free"))
        .and_then(|x| x.as_i64());
    Ok((free, true))
}

/// Free capacity check used by the scheduler: RAM guard and disk guard.
pub fn capacity_ok(inv: &Inventory, ram_guard_mb: i64, disk_guard_gb: i64) -> bool {
    inv.mem_available_mb >= ram_guard_mb && inv.disk_free_gb >= disk_guard_gb as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Inventory tests share one process-global snapshot cache, so they
    // run under one lock. Otherwise a parallel fresh probe can replace
    // the snapshot between two reads and the cache test flakes.
    // The lock lives at module level (super::TEST_LOCK) so server
    // grant tests can hold it too.

    #[test]
    fn probe_returns_sane_values() {
        let _t = TEST_LOCK.lock().unwrap();
        let inv = read_inventory_fresh();
        assert!(inv.mem_total_mb > 0, "MemTotal must be read");
        assert!(inv.mem_available_mb > 0, "MemAvailable must be read");
        assert!(inv.load1 >= 0.0);
        assert!(inv.disk_free_gb >= 0.0);
    }

    #[test]
    fn cache_returns_same_snapshot() {
        let _t = TEST_LOCK.lock().unwrap();
        let a = read_inventory();
        let b = read_inventory();
        assert_eq!(a.taken_unix, b.taken_unix);
    }

    #[test]
    fn bridge_read_is_fast_when_down() {
        // One attempt, no retries: a down bridge must not cost 600 ms.
        let _t = TEST_LOCK.lock().unwrap();
        let start = Instant::now();
        let (free, known) = read_bridge_pool();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "bridge read took {:?}",
            start.elapsed()
        );
        if !known {
            assert!(free.is_none());
        }
    }

    #[test]
    fn battery_type_is_used() {
        let _b = crate::battery::read_battery();
    }

    #[test]
    fn fresh_mem_matches_probe() {
        let _t = TEST_LOCK.lock().unwrap();
        let fresh = fresh_mem_available_mb();
        assert!(fresh > 0, "MemAvailable must be read");
        let inv = read_inventory_fresh();
        // Both read the same /proc file within a second; allow 64 MB drift.
        assert!((fresh - inv.mem_available_mb).abs() <= 64);
    }
}
