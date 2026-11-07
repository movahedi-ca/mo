//! Step 1 of S1-for-MO: measure actual resource usage per lease.
//! Peak RSS (MB) and CPU time (s) per lease, stored in the
//! `lease_usage` table. Measure only: nothing here changes any
//! grant logic, budget, or guard. A later step learns from these
//! rows; this step only records them.
//!
//! Two sources:
//! - "daemon": MO samples the Chrome process tree for a lease while
//!   it is active (tick, every 10 s) and once more at its end. The
//!   tree is found by the lease's Chrome profile path in /proc.
//!   Only vd-chrome leases have a profile MO can attribute. A
//!   main-display lease shares the :1 Chrome, so it cannot be
//!   attributed; a headless-chrome or cpu-ram lease runs a process
//!   MO did not start, so MO cannot sample it.
//! - "agent": the agent reports its own peak RSS and CPU time on
//!   release (mo-cli release --peak-rss-mb --cpu-time-s). This is
//!   how cpu-ram and headless-chrome leases report usage.
//! - "none": neither source produced a number. The row is still
//!   written, with nulls, so the absence is visible, not silent.
//! - "daemon-partial": the daemon sampled the lease, but the lease
//!   started before this daemon process did (a restart mid-lease),
//!   so the peak is a lower bound from after the restart only.
//!
//! RSS is the sum of resident pages over the matched processes.
//! Shared pages are counted once per process that maps them, so the
//! sum is a conservative overestimate of unique memory. That is the
//! safe direction for a budget: it never understates. CPU time is
//! the sum of utime + stime over the tree, in seconds; processes
//! that already exited are gone from /proc, so their CPU time is
//! not counted. Both limits are documented, not hidden.

use crate::core::{Lease, LeaseUsage, Result};
use crate::store::Store;
use serde::{Deserialize, Serialize};

/// Usage reported by an agent on release. Both fields optional:
/// an agent may report only what it measured.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AgentUsage {
    pub peak_rss_mb: Option<i64>,
    pub cpu_time_s: Option<f64>,
}

/// Running peak for one lease, held in daemon memory. Peak RSS is
/// the max over samples; CPU time is the latest cumulative total
/// (it only grows while the tree lives).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunningUsage {
    pub peak_rss_mb: i64,
    pub cpu_time_s: f64,
}

/// One observation of a process tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeSample {
    /// Sum of resident memory over the matched processes, MB.
    pub rss_mb: i64,
    /// Sum of utime + stime over the matched processes, seconds.
    pub cpu_time_s: f64,
    /// Number of matched processes.
    pub procs: i64,
}

/// Merge a fresh sample into a running peak. RSS keeps the max;
/// CPU time takes the latest total (cumulative, never decreases
/// while the tree lives).
pub fn merge_peak(running: &mut RunningUsage, sample: TreeSample) {
    if sample.rss_mb > running.peak_rss_mb {
        running.peak_rss_mb = sample.rss_mb;
    }
    if sample.cpu_time_s > running.cpu_time_s {
        running.cpu_time_s = sample.cpu_time_s;
    }
}

/// Page size in bytes, from the host. Falls back to 4096.
fn page_size() -> i64 {
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as i64
    } else {
        4096
    }
}

/// Clock ticks per second, from the host. Falls back to 100.
fn clk_tck() -> f64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as f64
    } else {
        100.0
    }
}

/// Resident memory in MB from one /proc/<pid>/statm line.
/// Field 2 is resident pages. None on a short or unparsable line.
pub fn parse_statm_rss_mb(statm: &str) -> Option<i64> {
    let mut fields = statm.split_whitespace();
    let _size = fields.next()?;
    let resident: i64 = fields.next()?.parse().ok()?;
    if resident < 0 {
        return None;
    }
    Some(resident * page_size() / (1024 * 1024))
}

/// CPU time in seconds from one /proc/<pid>/stat line.
/// Fields 14 (utime) and 15 (stime), in clock ticks. The comm
/// field sits in parens and may contain spaces or parens, so the
/// fields after it are found from the LAST ')': the first token
/// after it is field 3 (state), the 12th is utime, the 13th stime.
pub fn parse_proc_stat_cpu(stat: &str) -> Option<f64> {
    let close = stat.rfind(')')?;
    let rest = &stat[close + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 13 {
        return None;
    }
    let utime: f64 = fields[11].parse().ok()?;
    let stime: f64 = fields[12].parse().ok()?;
    if utime < 0.0 || stime < 0.0 {
        return None;
    }
    Some((utime + stime) / clk_tck())
}

/// Sum RSS and CPU time over all processes whose cmdline contains
/// `needle` (a Chrome profile path). Comm-first: only processes
/// whose comm is exactly "chrome" get a cmdline read, matching the
/// pattern inventory.rs uses for chrome_count. Returns None when
/// no process matches. Never fails the caller: unreadable /proc
/// entries and unparsable lines are skipped, not errors.
pub fn sample_process_tree(needle: &str) -> Option<TreeSample> {
    sample_process_tree_at("/proc", needle)
}

/// Same as sample_process_tree, but over a given proc root.
/// The root parameter exists so tests can build a fake /proc tree;
/// production always passes "/proc".
pub fn sample_process_tree_at(proc_root: &str, needle: &str) -> Option<TreeSample> {
    let entries = std::fs::read_dir(proc_root).ok()?;
    let mut total = TreeSample::default();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // Comm first: skip non-Chrome processes without a cmdline read.
        let stat = match std::fs::read_to_string(format!("{proc_root}/{name}/stat")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let comm = stat
            .find('(')
            .and_then(|a| stat.rfind(')').map(|b| &stat[a + 1..b]));
        if comm != Some("chrome") {
            continue;
        }
        let cmdline = match std::fs::read(format!("{proc_root}/{name}/cmdline")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !String::from_utf8_lossy(&cmdline).contains(needle) {
            continue;
        }
        let rss = std::fs::read_to_string(format!("{proc_root}/{name}/statm"))
            .ok()
            .and_then(|s| parse_statm_rss_mb(&s))
            .unwrap_or(0);
        let cpu = parse_proc_stat_cpu(&stat).unwrap_or(0.0);
        total.rss_mb += rss;
        total.cpu_time_s += cpu;
        total.procs += 1;
    }
    if total.procs == 0 {
        None
    } else {
        Some(total)
    }
}

/// Write the usage row for a lease that just ended. One row per
/// lease, ever: the insert is OR IGNORE, so a second call for the
/// same lease is a no-op and returns false. Duration is end minus
/// start, clamped at 0. Source picks the best available evidence:
/// agent-reported wins, then the daemon-measured peak, then none.
/// A daemon peak for a lease that started before this daemon
/// process did is marked "daemon-partial": it covers only the
/// part of the lease after the restart, so it is a lower bound.
pub fn record_ended_lease(
    store: &Store,
    lease: &Lease,
    now: i64,
    agent: Option<&AgentUsage>,
    daemon: Option<&RunningUsage>,
    daemon_started_unix: i64,
) -> Result<bool> {
    let (peak_rss_mb, cpu_time_s, source) = match agent {
        Some(a) if a.peak_rss_mb.is_some() || a.cpu_time_s.is_some() => {
            (a.peak_rss_mb, a.cpu_time_s, "agent".to_string())
        }
        _ => match daemon {
            Some(d) => {
                let src = if lease.start_unix >= daemon_started_unix {
                    "daemon"
                } else {
                    "daemon-partial"
                };
                (Some(d.peak_rss_mb), Some(d.cpu_time_s), src.to_string())
            }
            None => (None, None, "none".to_string()),
        },
    };
    let row = LeaseUsage {
        lease_id: lease.lease_id.clone(),
        agent_id: lease.agent_id.clone(),
        job_id: lease.job_id.clone(),
        resource_class: lease.resource_class.clone(),
        peak_rss_mb,
        cpu_time_s,
        duration_s: (now - lease.start_unix).max(0),
        source,
        recorded_unix: now,
    };
    store.record_lease_usage(&row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::now_unix;

    fn sample_lease() -> Lease {
        let now = now_unix();
        Lease {
            lease_id: "mo-u-1".to_string(),
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            resource_class: "vd-chrome".to_string(),
            desktop: "vd1".to_string(),
            display: ":2".to_string(),
            cdp_port: Some(9333),
            vnc_port: Some(5903),
            profile: Some("/tmp/chrome-vd1".to_string()),
            cpu_cores: 1,
            mem_mb: 512,
            state: "released".to_string(),
            start_unix: now - 120,
            end_unix: now,
            heartbeat_due_unix: now,
            heartbeat_misses: 0,
            extensions: 0,
            last_heartbeat_unix: 0,
            created_unix: now - 120,
            started_desktop: 1,
        }
    }

    fn tmp_store() -> (Store, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = format!("/tmp/mo-u-{nanos}.db");
        (Store::open(&p).unwrap(), p)
    }

    #[test]
    fn parse_statm_rss_mb_reads_resident_field() {
        // size=100000 pages, resident=25000 pages, 4096 bytes/page
        // = 102400000 bytes = 97 MB (integer division).
        let mb = parse_statm_rss_mb("100000 25000 0 0 0 0 0").unwrap();
        assert_eq!(mb, 25000 * 4096 / (1024 * 1024));
        assert!(parse_statm_rss_mb("100000").is_none());
        assert!(parse_statm_rss_mb("").is_none());
        assert!(parse_statm_rss_mb("100000 -5 0 0 0 0 0").is_none());
    }

    #[test]
    fn parse_proc_stat_cpu_reads_utime_plus_stime() {
        // pid (comm) state ppid pgrp session tty_nr tpgid flags
        // minflt cminflt majflt cmajflt utime stime ...
        // utime=100, stime=50 ticks, 100 ticks/s = 1.5 s.
        let stat = "123 (chrome) R 1 2 3 4 5 6 7 8 9 10 100 50 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_proc_stat_cpu(stat).unwrap(), 1.5);
        // Comm with spaces and parens: fields are found from the
        // last ')', so the comm content cannot shift them.
        let stat2 = "123 (my (weird) chrome) R 1 2 3 4 5 6 7 8 9 10 200 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_proc_stat_cpu(stat2).unwrap(), 2.0);
        assert!(parse_proc_stat_cpu("123 (chrome) R 1").is_none());
        assert!(parse_proc_stat_cpu("no parens here").is_none());
    }

    #[test]
    fn sample_tree_matches_nothing_returns_none() {
        assert!(sample_process_tree("mo-no-such-profile-xyz").is_none());
    }

    #[test]
    fn sample_tree_skips_non_chrome_processes() {
        // exec -a sets argv[0]; the child is a real process whose
        // cmdline carries the marker, but its comm is "sleep", not
        // "chrome". The comm-first filter must skip it: only
        // comm == "chrome" is ever sampled.
        let marker = format!("mo-usage-test-{}", uuid::Uuid::new_v4());
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exec -a {marker} sleep 30"))
            .spawn()
            .expect("spawn sh");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(sample_process_tree(&marker).is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn sample_tree_sums_a_fake_proc_tree() {
        // Two chrome pids with the needle in cmdline, one chrome
        // pid with a different profile, one non-chrome pid with the
        // needle in cmdline. Only the two matching chrome pids count.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = format!("/tmp/mo-proc-{nanos}");
        let needle = "/tmp/chrome-vd1";
        let mk = |pid: &str, comm: &str, cmd: &str, statm: &str, stat: &str| {
            let dir = format!("{root}/{pid}");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(format!("{dir}/cmdline"), cmd).unwrap();
            std::fs::write(format!("{dir}/statm"), statm).unwrap();
            std::fs::write(format!("{dir}/stat"), stat).unwrap();
            let _ = comm;
        };
        // pid 100: chrome, needle, resident 25000 pages = 97 MB,
        // utime 100 + stime 50 = 1.5 s.
        mk(
            "100",
            "chrome",
            &format!("chrome --user-data-dir={needle}\0"),
            "100000 25000 0 0 0 0 0",
            "100 (chrome) R 1 2 3 4 5 6 7 8 9 10 100 50 0 0 0 0 0 0 0 0 0 0",
        );
        // pid 101: chrome, needle, resident 10000 pages = 39 MB,
        // utime 200 + stime 0 = 2.0 s.
        mk(
            "101",
            "chrome",
            &format!("chrome --user-data-dir={needle} --type=renderer\0"),
            "50000 10000 0 0 0 0 0",
            "101 (chrome) R 1 2 3 4 5 6 7 8 9 10 200 0 0 0 0 0 0 0 0 0 0 0",
        );
        // pid 102: chrome, different profile. Must not count.
        mk(
            "102",
            "chrome",
            "chrome --user-data-dir=/tmp/chrome-vd2\0",
            "50000 10000 0 0 0 0 0",
            "102 (chrome) R 1 2 3 4 5 6 7 8 9 10 200 0 0 0 0 0 0 0 0 0 0 0",
        );
        // pid 103: not chrome, needle in cmdline. Must not count.
        mk(
            "103",
            "sleep",
            &format!("sleep {needle}\0"),
            "50000 10000 0 0 0 0 0",
            "103 (sleep) R 1 2 3 4 5 6 7 8 9 10 200 0 0 0 0 0 0 0 0 0 0 0",
        );
        let s = sample_process_tree_at(&root, needle).unwrap();
        assert_eq!(s.procs, 2);
        assert_eq!(s.rss_mb, (25000 + 10000) * 4096 / (1024 * 1024));
        assert_eq!(s.cpu_time_s, 3.5);
        assert!(sample_process_tree_at(&root, "/tmp/no-such-profile").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_peak_keeps_max_rss_and_latest_cpu() {
        let mut run = RunningUsage::default();
        merge_peak(
            &mut run,
            TreeSample {
                rss_mb: 100,
                cpu_time_s: 1.5,
                procs: 3,
            },
        );
        merge_peak(
            &mut run,
            TreeSample {
                rss_mb: 80,
                cpu_time_s: 2.5,
                procs: 3,
            },
        );
        assert_eq!(run.peak_rss_mb, 100);
        assert_eq!(run.cpu_time_s, 2.5);
        merge_peak(
            &mut run,
            TreeSample {
                rss_mb: 120,
                cpu_time_s: 2.0,
                procs: 3,
            },
        );
        assert_eq!(run.peak_rss_mb, 120);
        assert_eq!(run.cpu_time_s, 2.5);
    }

    #[test]
    fn record_ended_lease_agent_wins() {
        let (s, p) = tmp_store();
        let now = now_unix();
        let lease = sample_lease();
        let agent = AgentUsage {
            peak_rss_mb: Some(165),
            cpu_time_s: Some(4.5),
        };
        let daemon = RunningUsage {
            peak_rss_mb: 900,
            cpu_time_s: 99.0,
        };
        assert!(
            record_ended_lease(&s, &lease, now, Some(&agent), Some(&daemon), now - 1000).unwrap()
        );
        let row = s.get_lease_usage("mo-u-1").unwrap().unwrap();
        assert_eq!(row.source, "agent");
        assert_eq!(row.peak_rss_mb, Some(165));
        assert_eq!(row.cpu_time_s, Some(4.5));
        assert_eq!(row.duration_s, 120);
        // Second call is a no-op: one row per lease, ever.
        assert!(
            !record_ended_lease(&s, &lease, now, Some(&agent), Some(&daemon), now - 1000).unwrap()
        );
        assert_eq!(s.list_lease_usage(10).unwrap().len(), 1);
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn record_ended_lease_daemon_and_partial() {
        let (s, p) = tmp_store();
        let now = now_unix();
        let mut lease = sample_lease();
        lease.lease_id = "mo-u-2".to_string();
        let daemon = RunningUsage {
            peak_rss_mb: 800,
            cpu_time_s: 12.0,
        };
        // Lease started after the daemon: full "daemon" source.
        assert!(record_ended_lease(&s, &lease, now, None, Some(&daemon), now - 1000).unwrap());
        assert_eq!(
            s.get_lease_usage("mo-u-2").unwrap().unwrap().source,
            "daemon"
        );
        // Same lease id cannot re-record; use a second lease that
        // started before this daemon process: "daemon-partial".
        let mut lease2 = sample_lease();
        lease2.lease_id = "mo-u-3".to_string();
        lease2.start_unix = now - 5000;
        lease2.created_unix = now - 5000;
        assert!(record_ended_lease(&s, &lease2, now, None, Some(&daemon), now - 1000).unwrap());
        let row2 = s.get_lease_usage("mo-u-3").unwrap().unwrap();
        assert_eq!(row2.source, "daemon-partial");
        assert_eq!(row2.duration_s, 5000);
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn record_ended_lease_none_when_unmeasured() {
        let (s, p) = tmp_store();
        let now = now_unix();
        let lease = sample_lease();
        assert!(record_ended_lease(&s, &lease, now, None, None, now - 1000).unwrap());
        let row = s.get_lease_usage("mo-u-1").unwrap().unwrap();
        assert_eq!(row.source, "none");
        assert_eq!(row.peak_rss_mb, None);
        assert_eq!(row.cpu_time_s, None);
        // An agent that reports nothing (both fields None) is the
        // same as no report: it does not claim the "agent" source.
        let mut lease2 = sample_lease();
        lease2.lease_id = "mo-u-4".to_string();
        let empty = AgentUsage {
            peak_rss_mb: None,
            cpu_time_s: None,
        };
        assert!(record_ended_lease(&s, &lease2, now, Some(&empty), None, now - 1000).unwrap());
        assert_eq!(s.get_lease_usage("mo-u-4").unwrap().unwrap().source, "none");
        drop(s);
        let _ = std::fs::remove_file(p);
    }
}
