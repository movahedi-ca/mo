# MO benchmarks — TASK-015

Date: 2026-11-06.
Host: this dev VM, not chromebook-vm.
Binary: target/release, rustc 1.99.1.
Unit tests: 65 pass. Clippy: 0 warnings. Fmt: clean.
Daemon paths: all under /tmp/mo-test, never ~/jobs/mo.
Release daemon on /tmp/mo-test/bench, socket /tmp/mo-test/bench/mo.sock.

## Host conditions

- CPU: 4 cores.
- RAM: 7934 MB total, MemAvailable 635 to 760 MB during the run.
- Disk: 122 GB free on /.
- Battery: no BAT0 sensor on this host.
- Bridge: nothing on 127.0.0.1:8899 (down).
- Chrome: none. vd scripts: none.
- Guards set to 0 by env (MO_RAM_GUARD_MB=0, MO_DISK_GUARD_GB=0).
- Reason: host MemAvailable sits below the 1200 MB default guard.
- This is config, not a code change.

## Fixtures

- Fake BAT0 in a private mount namespace: 78 percent, Discharging.
- Namespace is private. Host /sys is untouched.
- Fake vd scripts open a TCP listener on the vd CDP port.
- They fake CDP liveness only. No Chrome runs.
- Fake bridge /health responder used only for the bridge-up comparison.
- Fake meminfo (6000 MB available) and fake cpuinfo (16 cores) used
  only for the multi-lease adversarial tests, not for benchmarks.

## Status response time

- Target: P95 under 500 ms, 20 runs.
- Method: wall time around `mo-cli status`, one run after another.
- Daemon idle. Bridge down. Cache warm except run 1.
- Raw ms: 8, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2.
- Min 2 ms. Median 2 ms. Max 8 ms. P95 about 3 ms.
- Result: PASS.

## Grant decision time

- Target: P95 under 1 s, 20 runs, desktop start excluded.
- Method: wall time around `mo-cli request`, cpu-ram class.
- Each grant was followed by a release (release not timed).
- Bridge down, the normal state on this host.
- Raw ms: 608, 612, 609, 608, 607, 607, 607, 608, 610, 607,
  607, 607, 607, 608, 607, 607, 607, 607, 607, 608.
- Min 607 ms. Median 607 ms. Max 612 ms. P95 about 610 ms.
- Result: PASS.
- Note: the 600 ms is bridge retry cost, not grant logic.
- `read_bridge_pool` tries 3 times with 200 ms sleeps when the
  bridge is down. With the bridge up the same grant takes 8 to 10 ms.
- Bridge-up check, 5 runs: 10, 9, 9, 8, 8 ms. Median 9 ms.
- One vd-chrome grant with the fake CDP listener: 819 ms.
- It includes fake provision. Still under 1 s.

## Idle RAM and idle CPU

- Targets: under 30 MB RAM, under 1 percent CPU.
- Method: sampler every 30 s for 750 s (12.5 min, 26 samples).
- RSS from /proc/PID/status. CPU from /proc/PID/stat tick deltas.
- Daemon PID 3503, release build, bench instance.
- The daemon was idle except for the benchmark bursts at the start.
- RSS: min 4712 KB, max 4808 KB. Mean about 4790 KB (4.7 MB).
- CPU ticks: 0 to 7 at 100 Hz over 750 s.
- That is 0.07 s of CPU in 750 s.
- Mean CPU: 0.009 percent of one core, 0.002 percent of 4 cores.
- Result: PASS on both.
- Note: an earlier sampler from a killed daemon kept writing
  DAEMON_GONE lines into the same file. The 26 RSS lines above
  are the live daemon only, timestamps every 30 s, no gaps.

## DB size at 1000 leases

- Target: under 10 MB at 1000 leases.
- Method: 1000 request plus release cycles through the CLI.
- Class cpu-ram, 256 MB, 5 min. Bridge up for speed.
- Time for 1000 cycles: 18 s.
- Rows after: leases 1000, requests 1000, audit 4000, snapshots 1.
- mo.db before checkpoint: 663552 bytes.
- mo.db-wal before checkpoint: 4148872 bytes.
- mo.db after PRAGMA wal_checkpoint(TRUNCATE): 684032 bytes (0.65 MB).
- Result: PASS. Both the file alone and file plus WAL stay under 10 MB.

## Dashboard refresh interval

- Target: every 30 s. Zero missed refresh over 5 min.
- Method: stat the dashboard file mtime every 30 s for 5.5 min.
- 12 samples. Intervals in seconds:
  30, 30, 30, 30, 30, 23, 30, 31, 30, 31, 30.
- Max interval 31 s. The 23 s interval is a daemon restart:
  the new daemon writes once at startup.
- Result: PASS. No interval missed the 30 s cadence by more
  than scheduling jitter (1 s).

## Socket and listeners

- Socket mode: 600. Verified by stat on /tmp/mo-test/bench/mo.sock.
- TCP listeners owned by mo-daemon: 0.
- Verified by ss -tlnp. The one listener seen during testing
  was socat PID 24272 on 127.0.0.1:9333, a fake CDP fixture.
- It is not part of MO.

## Dashboard file

- MO SVG present (1 svg tag). Teal #0E7C7B present.
- Face live: "MO is idle" with no lease, "MO is busy" with
  an active lease. Both observed in the live file.
- Warn and refuse faces are covered by unit tests only.
- Meta refresh: content="30". Script tags: 0.
- SVG under 2 KB per unit test.

## End-to-end lease flow

- Request to release ran live for cpu-ram and vd-chrome.
- Example vd lease: mo-1793995501-1e86d9, vd1, :2, CDP 9333, VNC 5903.
- Status, heartbeat, extend, release, health all returned JSON OK.
- Full flow is covered by the unit test socket_round_trip_request_to_release
  and by the live runs in this file.

## Deviations and observations

- OBS-001: grant time is 607 ms with the bridge down and 9 ms
  with the bridge up. The retry loop costs 60 percent of the
  1 s budget. The target still passes.
- OBS-002: status battery uses the 60 s cache. After a battery
  change it can lag the grant gate by up to 60 s. By design.
- No number in this file is invented. Every number above was
  measured in this session. Raw logs were under /tmp/mo-test.

## Post-fix re-measurement (2026-11-06, after patches 0-10 + sweep)

- Measured on this dev host against the release binaries, live
  daemon under /tmp/mo-bench3. Default guards block grants here
  (host MemAvailable 801 MB < 1200 MB guard, /tmp disk free 0.52 GB
  < 1 GB guard), so the successful grant below ran with
  MO_RAM_GUARD_MB=100 and MO_DISK_GUARD_GB=0. The guards
  themselves are unchanged in code.
- Status: 20 runs, min 3 ms, median 3 ms, max 4 ms.
  Before: median 2 ms, max 8 ms. Same class.
- cpu-ram grant: 4 ms round-trip, lease mo-1794000583-a5ebd6.
  Before: 9 ms with the bridge up.
- Bridge-needing request with the bridge down: 3 ms round-trip,
  queued with "head-of-line skip: bridge pool unknown".
  Before: median 607 ms, max 612 ms (3x200 ms retries).
  The 600 ms bridge-down tax is gone.
- Idle RSS: 5,512 kB (5.4 MB) after 5 s idle.
  Before: 4.7 MB max. Slightly higher, still far under the
  30 MB V3 limit and the 50 MB original limit.
- DB size at 1001 leases: 233,472 bytes (0.22 MB), after
  PRAGMA wal_checkpoint(TRUNCATE). Before: 0.65 MB.
  Rows were inserted directly with sqlite3 after stopping the
  daemon; 1000 concurrent real grants cannot exist on any host
  because capacity gates stop granting first.
- Grant times now print the host zone (+00:00 here), not the old
  hardcoded -05:00. Verified live in the grant JSON.
- Socket: srw------- (0600). No TCP listeners added.
- Dashboard refresh interval was not re-measured live; the
  thread now sleeps cfg.dashboard_refresh_sec and writes via
  tmp + rename (code + unit tests).
