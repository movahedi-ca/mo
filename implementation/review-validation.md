# MO architecture review — validation table

Date: 2026-11-06.
Review file: ~/workspace/user/files/mo-architecture-review_1_al79.html.
Review commit: e6ef028. Validated against HEAD 0efe493.
Method: read every cited file and line. No finding was fixed before confirmation.
Result: 16 of 16 findings CONFIRMED. Both footguns CONFIRMED.
Systemd note CONFIRMED. Test notes CONFIRMED. Zero findings REJECTED.

## F01 to F16

| ID | Verdict | Proof (file, line at HEAD) |
|---|---|---|
| F01 | CONFIRMED | src/server.rs drain_queue Err arm: every grant error audits "denied" and sets status "denied". E06/E07 are transient. |
| F02 | CONFIRMED | src/server.rs drain_queue battery block: any can_new_lease error audits "denied" and returns Ok(()). Warn/Restrict abort the whole drain. |
| F03 | CONFIRMED | src/server.rs serve_one: read_line with no timeout, no cap. drain_queue calls read_inventory_fresh every pass. inventory.rs read_bridge_pool: 3 tries, 200 ms sleep. lease::grant provision runs vd-start.sh plus wait_cdp 10 s inside serve_one. |
| F04 | CONFIRMED | src/lease.rs grant phase 3: store.with_tx(...)? with no rollback. Phase 1 Err arm rewrites every error as E04 "desktop busy". |
| F05 | CONFIRMED | drain_queue is called only from cmd_request. cmd_release, expire_leases, reclaim_check do not call it. src/main.rs sweep thread never calls queue::expire_stale. |
| F06 | CONFIRMED | src/server.rs free_capacity: mem_mb = mem_available_mb - mem_res. load1 is stored on Inventory and never read by sched::fits. cpu_total() re-reads /proc/cpuinfo on every call. |
| F07 | CONFIRMED | src/queue.rs validate_request allows desktops 0..=2. lease::pick_desktop returns one id. grant inserts one lease and one reservation row. |
| F08 | CONFIRMED | src/lease.rs heartbeat rejects only released/expired/failed; any other state, including interrupted, is forced to active. No 600 s expiry of interrupted exists. LeaseState::holds_desktop excludes interrupted; the partial unique index matches. |
| F09 | CONFIRMED | src/server.rs cmd_extend: conflict = queued.iter().any(|r| r.desktops > 0 && lease.desktop != "none"). Ignores which desktop, free count, CPU, RAM. |
| F10 | CONFIRMED | src/sched.rs plan sorts PriorityClass first (derived Ord, P0 < P1 < P2), then deadline, then score. Test aging_p2_reaches_p1_after_100_min asserts only the integer score. Test class_orders_before_score asserts P1 before a 5-hour P2. |
| F11 | CONFIRMED | lease::pick_desktop returns the first id not held; never reads Desktop.up. desktops::expand_one starts the first down desktop, no battery check. should_expand hardcodes 1500. list_desktops sets leased: false always. |
| F12 | CONFIRMED | src/server.rs free_capacity desktops: (10 - vd_held). Config max_vd is read only by should_expand (sustained). cmd_expand never consults the adapter or the battery. OracleAdapter refuses start_desktop, yet cmd_expand calls expand_one. |
| F13 | CONFIRMED | inventory.rs CACHE_TTL is a private const 60; cfg.inventory_cache_sec is never read. read_inventory drops the mutex before calling fresh. battery.rs read_battery_at calls Config::from_env() on every read. count_chrome and chrome_alive_for_profile read cmdline for every pid. |
| F14 | CONFIRMED | store.rs open sets synchronous=FULL. lease::heartbeat commits update plus audit, then insert_heartbeat commits a second transaction. heartbeats has no index. No index on requests(status), leases(state), audit(created_unix), inventory_snapshots(taken_unix). No retention anywhere. release, expire_leases, reclaim_check never delete reservation rows. |
| F15 | CONFIRMED | src/main.rs dashboard thread sleeps 30 s hardcoded; cfg.dashboard_refresh_sec is only the meta tag. Dashboard write is fs::write, not atomic. dashboard::render calls list_desktops (11 connects) then notify::snapshot, which re-queries queue, leases, audit. core.rs est_label and unix_to_iso(unix, -5) always print EST. |
| F16 | CONFIRMED | Cargo.toml opt-level = 2, no rust-version, no other profile keys. Cargo.lock is not tracked in the repo (git ls-files shows none) although state.yaml says pinned. clap is a lib dependency, so mo-daemon links the CLI parser. |

## Footguns

| ID | Verdict | Proof |
|---|---|---|
| Footgun 1 | CONFIRMED | src/server.rs serve_one and call(): read_line with no timeout and no cap. One quiet client pins the loop; one huge line grows the buffer. |
| Footgun 2 | CONFIRMED | src/store.rs backup(): format!("VACUUM INTO '{dst}'"). A quote in dst changes the statement. |
| Systemd note | CONFIRMED | systemd/mo-daemon.service: After=network.target, Wants=network.target. StartLimitBurst and StartLimitIntervalSec sit under [Service]; both are [Unit] keys and are ignored (DEFECT-005). |
| Test notes | CONFIRMED | inventory.rs cache_returns_same_snapshot can flake: the cache is a process-global Mutex and parallel tests call read_inventory_fresh. bridge_read_does_not_panic_when_down pays the 600 ms sleep when the port is closed. |

## Tester defects DEFECT-001 to DEFECT-005

| ID | Verdict | Proof | Maps to |
|---|---|---|---|
| DEFECT-001 | CONFIRMED | store.rs partial index idx_leases_desktop_active covers every desktop value including "none". Only one cpu-ram lease can be active. | F14 schema area; fix in patch 0 (schema v2). |
| DEFECT-002 | CONFIRMED | lease.rs release only marks the lease released. It never calls stop_desktop. | New fix; release stops the desktop, flags on failure. |
| DEFECT-003 | CONFIRMED | No bypass detection anywhere. A desktop up with no holding lease produces zero audit rows. | New fix; tick audits "bypass" once per desktop per hour. |
| DEFECT-004 | CONFIRMED | desktops::stop_desktop failure returns E04 with no audit row and no flagged state. | New fix; flagged_desktops table, fold refuses flagged, CLI clear-flag. |
| DEFECT-005 | CONFIRMED | systemd unit keys in the wrong section. See systemd note above. | Fix in patch 10 (unit file). |

## Judgments (parent brief step 4)

- (a) synchronous FULL vs NORMAL: measured on this host with python sqlite3, 200 single-row commits each. FULL 5 ms total, 0.03 ms per commit. NORMAL 5 ms total, 0.02 ms per commit. Difference is noise. The Chromebook had a battery shutdown on 2026-11-05, so power-loss durability matters. Decision: KEEP FULL. Recorded in state.yaml and in an ADR note.
- (b) wait_cdp / vd-start.sh off serve_one: full async provision (reserve, return, worker provisions, commit) is too big for this run; it changes cmd_request semantics and the two-phase grant tests. Safe subset in this run: thread-per-connection plus read timeout 2 s plus 64 KiB line cap, single-flight snapshot, bridge only when needed. Async provision stays a follow-up, recorded in state.yaml.
- (c) panic=abort: NOT taken. Keep unwind so a panic in the field leaves a backtrace in the log. Binary size cost is small (2.7 MB daemon). Recorded in state.yaml and in an ADR note.
