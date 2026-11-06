//! mo-daemon entry.
//! One writer, one SQLite connection, Unix socket only.
//! Threads: server loop, 60s inventory snapshot, 30s dashboard,
//! 60s heartbeat sweep. No HTTP, no LLM, no network port.

use mo::core::{now_unix, Config, Result};
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

fn main() {
    if let Err(e) = run() {
        eprintln!("mo-daemon: {}: {}", e.code, e.message);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cfg = Config::from_env();
    mo::inventory::set_cache_ttl(cfg.inventory_cache_sec);
    if let Some(parent) = std::path::Path::new(&cfg.db_path).parent() {
        mo::inventory::set_db_dir(&parent.to_string_lossy());
    }
    for path in [
        &cfg.db_path,
        &cfg.socket_path,
        &cfg.lock_path,
        &cfg.dashboard_path,
    ] {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                mo::core::MoError::e10(format!("cannot create dir {parent:?}: {e}"))
            })?;
        }
    }
    // One daemon only: std File try_lock, OS released on death. V3 ADR-010.
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&cfg.lock_path)?;
    if lock_file.try_lock().is_err() {
        return Err(mo::core::MoError::e11("mo-daemon already running"));
    }
    let _ = std::fs::set_permissions(&cfg.lock_path, std::fs::Permissions::from_mode(0o600));
    let store = Arc::new(mo::store::Store::open(&cfg.db_path)?);
    // Crash recovery on start. Reboot is detected from /proc/stat btime.
    let rebooted = mo::store::rebooted_since_last_run(&store);
    let report = mo::store::recover_on_start(&store, now_unix(), rebooted)?;
    if !report.rolled_back.is_empty() || !report.interrupted.is_empty() {
        eprintln!(
            "mo-daemon: recovery rolled back {:?}, interrupted {:?}",
            report.rolled_back, report.interrupted
        );
    }
    let adapter = mo::host::adapter_for(&cfg.host_name)?;
    let daemon = Arc::new(mo::server::Daemon::new(
        store.clone(),
        cfg.clone(),
        adapter.into(),
    ));
    // Inventory snapshot every 60 seconds, stored in the DB.
    {
        let store = store.clone();
        let cfg = cfg.clone();
        std::thread::spawn(move || loop {
            let dir = std::path::Path::new(&cfg.db_path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string());
            let inv = mo::inventory::read_inventory_fresh_with(&cfg, &dir);
            let _ = store.insert_snapshot(&inv);
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
    }
    // Dashboard on the configured interval. Data is gathered once
    // and passed in, so the renderer never re-queries the store or
    // re-probes desktops. The file is written to a temp name and
    // renamed, so a reload never observes a partial page (F15).
    {
        let store = store.clone();
        let cfg = cfg.clone();
        std::thread::spawn(move || loop {
            let inv = mo::inventory::read_inventory();
            let (queued, leases, desktops, alerts) = mo::dashboard::gather(&store, &inv, &cfg);
            let html = mo::dashboard::render_data(&queued, &leases, &desktops, &alerts, &inv, &cfg);
            let tmp = format!("{}.tmp", cfg.dashboard_path);
            if std::fs::write(&tmp, html)
                .and_then(|()| std::fs::rename(&tmp, &cfg.dashboard_path))
                .is_err()
            {
                eprintln!("mo-daemon: dashboard write failed");
            }
            std::thread::sleep(std::time::Duration::from_secs(
                cfg.dashboard_refresh_sec.max(1) as u64,
            ));
        });
    }
    // Scheduler tick every 10 seconds: expiry, stale sweep, reclaim,
    // drain, bypass check, daily prune. Runs once at once at startup too.
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || loop {
            let _ = daemon.tick();
            std::thread::sleep(std::time::Duration::from_secs(10));
        });
    }
    // Drain once at startup: capacity that came back while MO was down
    // grants queued requests without waiting for a new request.
    let _ = daemon.drain_queue();
    println!(
        "mo-daemon up: db={} socket={} host={}",
        cfg.db_path, cfg.socket_path, cfg.host_name
    );
    daemon.serve(&cfg.socket_path)
}
