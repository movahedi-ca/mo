//! mo-daemon entry. TASK-003 skeleton: config load, lock, store open.
//! Wiring of server, inventory, dashboard, heartbeat lands in later tasks.

use mo::core::{Config, Result};
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;

fn main() {
    if let Err(e) = run() {
        eprintln!("mo-daemon: {}: {}", e.code, e.message);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cfg = Config::from_env();
    for path in [
        &cfg.db_path,
        &cfg.socket_path,
        &cfg.lock_path,
        &cfg.dashboard_path,
    ] {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
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
    let _store = (); // Store open lands in TASK-008.
                     // Socket mode 600: set on the socket file after bind in server.rs.
    let _ = std::fs::set_permissions(&cfg.lock_path, std::fs::Permissions::from_mode(0o600));
    println!(
        "mo-daemon up: db={} socket={}",
        cfg.db_path, cfg.socket_path
    );
    // Server loop lands in TASK-013. Skeleton parks here.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
