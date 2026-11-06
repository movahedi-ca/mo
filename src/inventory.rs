//! Inventory: /proc, /sys, ps, bridge pool. V3 section 5.2.
//! Snapshot is a 60 second cache. Grant reads fresh.
//! Guard uses MemAvailable, not MemFree (V3 mismatch 5).
//! Bridge pool read: HTTP GET 127.0.0.1:8899/health, 5s timeout, 2 retries.
//! On failure the pool is unknown, never guessed.

use crate::core::{now_unix, Inventory};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static CACHE: Mutex<Option<(Inventory, Instant)>> = Mutex::new(None);
const CACHE_TTL: Duration = Duration::from_secs(60);
const BRIDGE_HOST: &str = "127.0.0.1:8899";

/// Cached snapshot. Daemon status path uses this.
pub fn read_inventory() -> Inventory {
    if let Ok(cache) = CACHE.lock() {
        if let Some((inv, at)) = cache.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return inv.clone();
            }
        }
    }
    read_inventory_fresh()
}

/// Fresh read, bypasses the cache. Grant path uses this. V3 mismatch 2.
pub fn read_inventory_fresh() -> Inventory {
    let inv = probe();
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((inv.clone(), Instant::now()));
    }
    inv
}

fn probe() -> Inventory {
    let mut inv = Inventory {
        taken_unix: now_unix(),
        ..Default::default()
    };
    inv.load1 = read_load();
    let (avail, total) = read_mem();
    inv.mem_available_mb = avail;
    inv.mem_total_mb = total;
    inv.disk_free_gb = read_disk_free_gb("/");
    inv.chrome_count = count_chrome();
    inv.desktops_up = crate::desktops::list_desktops()
        .iter()
        .filter(|d| d.up)
        .count() as i64;
    let (free, known) = read_bridge_pool();
    inv.bridge_pool_free = free;
    inv.bridge_known = known;
    inv.battery = crate::battery::read_battery();
    inv
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

/// Disk free in GB via `df`. Std only, no libc crate.
fn read_disk_free_gb(path: &str) -> f64 {
    (|| -> Option<f64> {
        let out = std::process::Command::new("df")
            .arg("-k")
            .arg(path)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().nth(1)?;
        let avail_kb: f64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(avail_kb / 1024.0 / 1024.0)
    })()
    .unwrap_or(0.0)
}

/// Count processes whose cmdline mentions chrome.
fn count_chrome() -> i64 {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(cmd) = std::fs::read(format!("/proc/{name}/cmdline")) {
                let text = String::from_utf8_lossy(&cmd).to_lowercase();
                if text.contains("chrome") && !text.contains("mo-daemon") {
                    n += 1;
                }
            }
        }
    }
    n
}

/// One bridge pool read. Returns (pool_free, known).
/// GET /health is auth exempt. Pool fields are best effort; unknown stays unknown.
fn read_bridge_pool() -> (Option<i64>, bool) {
    for _ in 0..3 {
        match bridge_health_once() {
            Ok((free, known)) => return (free, known),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    (None, false)
}

fn bridge_health_once() -> std::result::Result<(Option<i64>, bool), std::io::Error> {
    let mut stream = TcpStream::connect_timeout(
        &BRIDGE_HOST.parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad bridge addr")
        })?,
        Duration::from_secs(5),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
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

    #[test]
    fn probe_returns_sane_values() {
        let inv = read_inventory_fresh();
        assert!(inv.mem_total_mb > 0, "MemTotal must be read");
        assert!(inv.mem_available_mb > 0, "MemAvailable must be read");
        assert!(inv.load1 >= 0.0);
        assert!(inv.disk_free_gb >= 0.0);
    }

    #[test]
    fn cache_returns_same_snapshot() {
        let a = read_inventory();
        let b = read_inventory();
        assert_eq!(a.taken_unix, b.taken_unix);
    }

    #[test]
    fn bridge_read_does_not_panic_when_down() {
        // Bridge may be up or down on this host. Either way no panic, no guess.
        let (free, known) = read_bridge_pool();
        if !known {
            assert!(free.is_none());
        }
    }

    #[test]
    fn battery_type_is_used() {
        let _b = crate::battery::read_battery();
    }
}
