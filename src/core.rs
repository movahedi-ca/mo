//! Core types, error codes E01-E14, config, time helpers.
//! No host facts live here. Host facts live in `host`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One MO error: a code from V3 section 6.2 plus a message and a next action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoError {
    pub code: String,
    pub message: String,
    pub next_action: String,
}

impl MoError {
    pub fn new(code: &str, message: impl Into<String>, next_action: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            next_action: next_action.into(),
        }
    }
    pub fn e01(msg: impl Into<String>) -> Self {
        Self::new(
            "E01",
            msg,
            "Fix the request, resend with the same idempotency_key",
        )
    }
    pub fn e02(msg: impl Into<String>) -> Self {
        Self::new(
            "E02",
            msg,
            "Pick a valid class, or pick a host with the capability",
        )
    }
    pub fn e03(msg: impl Into<String>) -> Self {
        Self::new("E03", msg, "Pick a duration in range 5 to 120")
    }
    pub fn e04(msg: impl Into<String>) -> Self {
        Self::new("E04", msg, "Wait for next_slot, stay queued")
    }
    pub fn e05(msg: impl Into<String>) -> Self {
        Self::new("E05", msg, "Wait for charge, retry later")
    }
    pub fn e06(msg: impl Into<String>) -> Self {
        Self::new(
            "E06",
            msg,
            "Wait, release a lease, or pick a smaller bundle",
        )
    }
    pub fn e07(msg: impl Into<String>) -> Self {
        Self::new("E07", msg, "Free disk, retry later")
    }
    pub fn e08(msg: impl Into<String>) -> Self {
        Self::new("E08", msg, "Check the lease id, do not retry blindly")
    }
    pub fn e09(msg: impl Into<String>) -> Self {
        Self::new("E09", msg, "Release on time, or request a new lease")
    }
    pub fn e10(msg: impl Into<String>) -> Self {
        Self::new("E10", msg, "Stop, run recovery, do not force a grant")
    }
    pub fn e11(msg: impl Into<String>) -> Self {
        Self::new("E11", msg, "Do not start a second daemon, check the lock")
    }
    pub fn e12(msg: impl Into<String>) -> Self {
        Self::new("E12", msg, "Check the host name, check the adapter")
    }
    pub fn e13(msg: impl Into<String>) -> Self {
        Self::new("E13", msg, "Check leases, heartbeat, Chrome, CDP")
    }
    pub fn e14(msg: impl Into<String>) -> Self {
        Self::new("E14", msg, "Never fold :1, never kill :1 Chrome")
    }
}

impl fmt::Display for MoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MoError {}

impl From<rusqlite::Error> for MoError {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::ConstraintViolation) => {
                MoError::e04(format!("SQLite constraint: {e}"))
            }
            Some(rusqlite::ffi::ErrorCode::DiskFull) => {
                MoError::e07(format!("SQLite disk full: {e}"))
            }
            _ => MoError::e10(format!("SQLite error: {e}")),
        }
    }
}

impl From<std::io::Error> for MoError {
    fn from(e: std::io::Error) -> Self {
        MoError::e10(format!("IO error: {e}"))
    }
}

impl From<serde_json::Error> for MoError {
    fn from(e: serde_json::Error) -> Self {
        MoError::e01(format!("JSON error: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, MoError>;

/// Resource class of a request. V3 section 5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceClass {
    VdChrome,
    HeadlessChrome,
    CpuRam,
    MainDisplay,
}

impl ResourceClass {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "vd-chrome" => Some(Self::VdChrome),
            "headless-chrome" => Some(Self::HeadlessChrome),
            "cpu-ram" => Some(Self::CpuRam),
            "main-display" => Some(Self::MainDisplay),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VdChrome => "vd-chrome",
            Self::HeadlessChrome => "headless-chrome",
            Self::CpuRam => "cpu-ram",
            Self::MainDisplay => "main-display",
        }
    }
}

/// Priority class. P0 is human and main display only.
// No derived Ord: the scheduler must never trust declaration
// order. Compare classes with weight(), never with cmp (F10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriorityClass {
    P0,
    P1,
    P2,
}

impl PriorityClass {
    pub fn weight(&self) -> i64 {
        match self {
            Self::P0 => 100,
            Self::P1 => 10,
            Self::P2 => 0,
        }
    }
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "P0" => Some(Self::P0),
            "P1" => Some(Self::P1),
            "P2" => Some(Self::P2),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
        }
    }
}

/// Lease states. V3 section 5.2. No backward move except extension denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Requested,
    Queued,
    Reserving,
    Granted,
    Active,
    ExtensionPending,
    Extended,
    Released,
    Expired,
    Stale,
    Failed,
    Denied,
    Cancelled,
    Interrupted,
}

impl LeaseState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Queued => "queued",
            Self::Reserving => "reserving",
            Self::Granted => "granted",
            Self::Active => "active",
            Self::ExtensionPending => "extension_pending",
            Self::Extended => "extended",
            Self::Released => "released",
            Self::Expired => "expired",
            Self::Stale => "stale",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "requested" => Some(Self::Requested),
            "queued" => Some(Self::Queued),
            "reserving" => Some(Self::Reserving),
            "granted" => Some(Self::Granted),
            "active" => Some(Self::Active),
            "extension_pending" => Some(Self::ExtensionPending),
            "extended" => Some(Self::Extended),
            "released" => Some(Self::Released),
            "expired" => Some(Self::Expired),
            "stale" => Some(Self::Stale),
            "failed" => Some(Self::Failed),
            "denied" => Some(Self::Denied),
            "cancelled" => Some(Self::Cancelled),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
    /// States that hold a desktop. The unique index covers these.
    pub fn holds_desktop(&self) -> bool {
        matches!(
            self,
            Self::Reserving
                | Self::Granted
                | Self::Active
                | Self::Stale
                | Self::ExtensionPending
                | Self::Interrupted
        )
    }
    /// Terminal states: no further move.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Released | Self::Expired | Self::Failed | Self::Denied | Self::Cancelled
        )
    }
}

/// Battery states. V3 section 5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatteryState {
    Normal,
    Warn,
    Restrict,
    Refuse,
    Recovery,
    Unknown,
}

impl BatteryState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Warn => "Warn",
            Self::Restrict => "Restrict",
            Self::Refuse => "Refuse",
            Self::Recovery => "Recovery",
            Self::Unknown => "Unknown",
        }
    }
}

/// A lease request from an agent. V3 section 5.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub agent_id: String,
    pub job_id: String,
    pub idempotency_key: String,
    pub resource_class: String,
    pub cpu_cores: i64,
    pub mem_mb: i64,
    pub mem_mb_max: Option<i64>,
    pub desktops: i64,
    pub duration_min: i64,
    pub priority_class: String,
    pub deadline: Option<String>,
    pub needs_bridge: bool,
    pub needs_display: bool,
    pub capabilities: Vec<String>,
}

impl Default for LeaseRequest {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            job_id: String::new(),
            idempotency_key: String::new(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 512,
            mem_mb_max: None,
            desktops: 1,
            duration_min: 30,
            priority_class: "P2".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            capabilities: Vec::new(),
        }
    }
}

/// A granted lease. V3 section 6.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub lease_id: String,
    pub desktop: String,
    pub display: String,
    pub cdp_port: Option<i64>,
    pub vnc_port: Option<i64>,
    pub profile: Option<String>,
    pub start: String,
    pub end: String,
    pub heartbeat_due: String,
    pub status: String,
}

/// One lease row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub lease_id: String,
    pub agent_id: String,
    pub job_id: String,
    pub resource_class: String,
    pub desktop: String,
    pub display: String,
    pub cdp_port: Option<i64>,
    pub vnc_port: Option<i64>,
    pub profile: Option<String>,
    pub cpu_cores: i64,
    pub mem_mb: i64,
    pub state: String,
    pub start_unix: i64,
    pub end_unix: i64,
    pub heartbeat_due_unix: i64,
    pub heartbeat_misses: i64,
    pub extensions: i64,
    pub last_heartbeat_unix: i64,
    pub created_unix: i64,
}

/// One queued request row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedRequest {
    pub id: i64,
    pub agent_id: String,
    pub job_id: String,
    pub idempotency_key: String,
    pub resource_class: String,
    pub cpu_cores: i64,
    pub mem_mb: i64,
    pub desktops: i64,
    pub duration_min: i64,
    pub priority_class: String,
    pub deadline: Option<String>,
    pub needs_bridge: bool,
    pub needs_display: bool,
    pub mem_mb_max: Option<i64>,
    pub lease_id: Option<String>,
    pub capabilities: Vec<String>,
    pub created_unix: i64,
    pub status: String,
}

/// One desktop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Desktop {
    pub id: String,
    pub display: String,
    pub cdp_port: Option<i64>,
    pub vnc_port: Option<i64>,
    pub profile: Option<String>,
    pub up: bool,
    pub leased: bool,
}

/// Battery reading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Battery {
    pub pct: Option<i64>,
    pub charging: bool,
    pub present: bool,
    pub state: BatteryState,
}

/// Host capability model. V3 section 5.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub has_battery: bool,
    pub has_desktops: bool,
    pub has_bridge: bool,
    pub max_vd: i64,
    pub max_ram_mb: i64,
    pub arch: String,
}

/// Inventory snapshot. Cache is 60 seconds; grant reads fresh.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub load1: f64,
    pub mem_available_mb: i64,
    pub mem_total_mb: i64,
    pub disk_free_gb: f64,
    pub chrome_count: i64,
    pub desktops_up: i64,
    pub bridge_pool_free: Option<i64>,
    pub bridge_known: bool,
    pub battery: Option<Battery>,
    pub taken_unix: i64,
}

/// One audit row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: i64,
    pub event: String,
    pub agent_id: Option<String>,
    pub job_id: Option<String>,
    pub lease_id: Option<String>,
    pub desktop: Option<String>,
    pub detail: String,
    pub created_unix: i64,
}

/// Config from env. V3 section 9.4. Paths are absolute, defaults under ~/jobs/mo.
#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: String,
    pub socket_path: String,
    pub lock_path: String,
    pub dashboard_path: String,
    pub host_name: String,
    pub lease_max_min: i64,
    pub extend_max_min: i64,
    pub heartbeat_min: i64,
    pub heartbeat_miss: i64,
    pub reclaim_grace_min: i64,
    pub inventory_cache_sec: i64,
    pub dashboard_refresh_sec: i64,
    pub idle_fold_min: i64,
    pub expand_queue_depth: i64,
    pub ram_guard_mb: i64,
    pub chrome_budget_mb: i64,
    pub disk_guard_gb: i64,
    pub battery_warn: i64,
    pub battery_restrict: i64,
    pub battery_refuse: i64,
    pub max_vd: i64,
    pub sustained_vd: i64,
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/home/auto".to_string())
}

/// Chrome profile root. Overridable with MO_PROFILE_ROOT; otherwise
/// $HOME/.config. No desktop path hardcodes /home/auto any more (j).
pub fn profile_root() -> String {
    std::env::var("MO_PROFILE_ROOT").unwrap_or_else(|_| format!("{}/.config", home_dir()))
}

/// Jobs dir for the vd scripts. Overridable with MO_JOBS_DIR.
pub fn jobs_dir() -> String {
    std::env::var("MO_JOBS_DIR").unwrap_or_else(|_| format!("{}/jobs", home_dir()))
}

fn env_i64(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let home = home_dir();
        let base = format!("{home}/jobs/mo");
        Self {
            db_path: std::env::var("MO_DB_PATH").unwrap_or(format!("{base}/mo.db")),
            socket_path: std::env::var("MO_SOCKET_PATH").unwrap_or(format!("{base}/mo.sock")),
            lock_path: std::env::var("MO_LOCK_PATH").unwrap_or(format!("{base}/mo.lock")),
            dashboard_path: std::env::var("MO_DASHBOARD_PATH")
                .unwrap_or(format!("{base}/dashboard.html")),
            host_name: std::env::var("MO_HOST").unwrap_or_else(|_| "chromebook-vm".to_string()),
            lease_max_min: env_i64("MO_LEASE_MAX_MIN", 120),
            extend_max_min: env_i64("MO_EXTEND_MAX_MIN", 30),
            heartbeat_min: env_i64("MO_HEARTBEAT_MIN", 5),
            heartbeat_miss: env_i64("MO_HEARTBEAT_MISS", 2),
            reclaim_grace_min: env_i64("MO_RECLAIM_GRACE_MIN", 5),
            inventory_cache_sec: env_i64("MO_INVENTORY_CACHE_SEC", 60),
            dashboard_refresh_sec: env_i64("MO_DASHBOARD_REFRESH_SEC", 30),
            idle_fold_min: env_i64("MO_IDLE_FOLD_MIN", 15),
            expand_queue_depth: env_i64("MO_EXPAND_QUEUE_DEPTH", 2),
            ram_guard_mb: env_i64("MO_RAM_GUARD_MB", 1200),
            chrome_budget_mb: env_i64("MO_CHROME_BUDGET_MB", 1024),
            disk_guard_gb: env_i64("MO_DISK_GUARD_GB", 1),
            battery_warn: env_i64("MO_BATTERY_WARN", 20),
            battery_restrict: env_i64("MO_BATTERY_RESTRICT", 10),
            battery_refuse: env_i64("MO_BATTERY_REFUSE", 5),
            max_vd: env_i64("MO_MAX_VD", 10),
            sustained_vd: env_i64("MO_SUSTAINED_VD", 6),
        }
    }
}

// ---- Time helpers: std only, no chrono ----

/// Wall clock anchored at boot and advanced by a monotonic timer.
/// Intervals (expiry, heartbeat, deadlines) never jump when the
/// wall clock steps (NTP, manual set). The anchor is taken once per
/// process, so the value only ever moves forward.
pub fn now_unix() -> i64 {
    static BOOT: OnceLock<(i64, Instant)> = OnceLock::new();
    let (boot_unix, boot_instant) = BOOT.get_or_init(|| {
        let boot_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (boot_unix, Instant::now())
    });
    boot_unix + boot_instant.elapsed().as_secs() as i64
}

/// Unix epoch to ISO 8601 in the host's local zone. The offset and
/// zone come from the host (libc localtime_r), never a hardcoded
/// -5. On America/Toronto this prints -05:00 in winter and -04:00
/// in summer, matching what the clock on the host says (F15).
pub fn unix_to_local_iso(unix: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = unix as libc::time_t;
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return format!("{unix}");
    }
    let off = tm.tm_gmtoff;
    let sign = if off < 0 { '-' } else { '+' };
    let (ah, am) = (off.unsigned_abs() / 3600, (off.unsigned_abs() % 3600) / 60);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        sign,
        ah,
        am
    )
}

/// Local label for the dashboard: "2026-11-06 14:37 EST" on a host
/// in EST, "2026-11-06 14:37 EDT" in daylight time, "UTC" on a host
/// with no zone. The zone abbreviation is read from the host, never
/// printed as a constant (F15).
pub fn local_label(unix: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = unix as libc::time_t;
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return format!("{unix}");
    }
    let zone = if tm.tm_zone.is_null() {
        "UTC".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
            .to_string_lossy()
            .into_owned()
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} {}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        zone
    )
}

/// Parse a subset of ISO 8601 (YYYY-MM-DDTHH:MM:SS with offset) to unix epoch.
/// Returns None on bad input.
pub fn iso_to_unix(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse().ok() };
    let year = num(0, 4)?;
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    let hour = num(11, 2)?;
    let minute = num(14, 2)?;
    let second = num(17, 2)?;
    let days = days_from_civil(year, month, day);
    let mut unix = days * 86400 + hour * 3600 + minute * 60 + second;
    if s.len() >= 25 {
        let sign = match b[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return Some(unix),
        };
        let oh = num(20, 2)?;
        let om = num(23, 2)?;
        unix -= sign * (oh * 3600 + om * 60);
    }
    Some(unix)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Lease id: mo-<unix epoch>-<6 hex random>. Clock change cannot collide.
pub fn new_lease_id() -> String {
    let hex: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(6)
        .collect();
    format!("mo-{}-{hex}", now_unix())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_root_honors_env() {
        // SAFETY: no other test reads MO_PROFILE_ROOT, so setting it
        // here cannot disturb a parallel test.
        std::env::set_var("MO_PROFILE_ROOT", "/tmp/fake-profiles");
        assert_eq!(profile_root(), "/tmp/fake-profiles");
        std::env::remove_var("MO_PROFILE_ROOT");
        assert!(profile_root().ends_with("/.config"));
    }

    #[test]
    fn lease_id_format() {
        let id = new_lease_id();
        assert!(id.starts_with("mo-"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].len(), 6);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn iso_round_trip() {
        let unix = 1793992800;
        let iso = unix_to_local_iso(unix);
        assert_eq!(iso_to_unix(&iso), Some(unix));
    }

    #[test]
    fn now_never_goes_backwards() {
        let a = now_unix();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = now_unix();
        assert!(b >= a, "{a} -> {b}");
    }

    #[test]
    fn error_codes_present() {
        assert_eq!(MoError::e14("x").code, "E14");
        assert_eq!(MoError::e01("x").code, "E01");
    }

    #[test]
    fn sqlite_error_codes_map() {
        let constraint = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed".to_string()),
        );
        assert_eq!(MoError::from(constraint).code, "E04");
        let full = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        );
        assert_eq!(MoError::from(full).code, "E07");
        let other = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERNAL),
            Some("boom".to_string()),
        );
        assert_eq!(MoError::from(other).code, "E10");
    }

    #[test]
    fn lease_state_holds_desktop() {
        assert!(LeaseState::Active.holds_desktop());
        assert!(!LeaseState::Released.holds_desktop());
        assert!(LeaseState::Released.is_terminal());
    }
}
