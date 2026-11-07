# MO fix plan: handover package

Date: 2026-11-06. Playbook: ~/workspace/your_files/mo-fix-plan-playbook.md (v1.0).
Ledger: implementation/state.yaml (fix_plan_2026_11_06). Status: all 19 tasks done.

## 1. What changed and why

Stress tests 1 and 2 (Nov 6) exposed five MO defects and three harness defects.

- D6 daemon restart under load. The daemon did NOT crash. Both restarts (17:35:53, 17:46:17 EST) were deliberate redeploys by the dashboard-app work. The socket outage came from main.rs running drain_queue() BEFORE serve() binds the socket. Fix (FIX-002): the socket binds first, then the startup drain runs on its own thread. PID logged at startup. Clean shutdown marker logged.
- D7 cold Chrome launch. A cold vd-chrome provision never produced a responding CDP, because MO never launched Chrome. Fix (FIX-004): after vd-start, MO calls vd-chrome.sh (thin wrapper over the real launcher at ~/jobs/virtual-desktops/vd-chrome.sh), then waits for CDP. Fix (FIX-005): CDP wait is 60 s. Timeout fails with distinct E15.
- D2/D8 drain retries without backoff. The drain retried every ~10 s with no cap (65 failed lease rows in ~4 min in test 1). Fix (FIX-006): backoff 30, 60, 120, 240 s, max 4 retries, then the request is marked failed. State is in memory only. Fix (FIX-007): circuit breaker at 3 consecutive identical error codes.
- D9 grant order not FIFO, cancel race, reserving row. Test 2 showed out-of-order grants, a cancel that could kill a lease, and a reserving row that seemed to vanish. Fix (FIX-008): grants follow arrival order within a priority class, fit-first kept, every non-fitting entry audited as a fit skip. Fix (FIX-009): cancel looks up the lease first; a live lease is released cleanly and recorded; a reserving lease blocks the cancel; reserve, commit, and rollback are transactions.
- D1 MO-side vd-start argument. Fix (FIX-010): MO passes the desktop NUMBER to vd-start.sh (vd1 becomes 1).
- D3 CDP port null. Fix (FIX-011): the lease row records a non-null CDP port for every Chrome lease. Also fixed in the same edit: grant() preserved the distinct E15 instead of re-wrapping every provision error as E04.
- D10 Chrome spike. FIX-012 found no stray Chrome. The spike rose with granted desktops and fell to baseline ~1 min after the last release. One extra instance during the spike is unverified. chrome_count counts ANY Chrome on the VM, so TEST-055 should count vd-profile Chrome only.
- D4/D12 harness stdout. FIX-013: parallel cb-job.sh calls shared one fixed /tmp/cb-run.out path. Now mktemp per call. TEST-057: 10/10 pass, twice.
- D11 Yahoo selector. FIX-014: canonical selector file at ~/workspace/chromebook-vm/yahoo-price-selectors.js. Primary [data-testid='qsp-price'] verified live (AAPL 342.37). Dead selectors confirmed 0.

New bugs found during the work:

- NEW-001: ReclaimProbe.lock_held was computed but never used. Fixed: reclaim keeps a desktop whose profile lock is held (Chrome starting).
- NEW-002: skip_audits_coalesce_within_a_minute assumed live MemAvailable under 4096 MB. It failed on the VM (~5900 MB). Root cause: drain_queue overwrites mem_available with the live fresh_mem_available_mb() re-read, which ignored the test override. Fixed: the re-read consults the override in test builds; the test uses a fixed 512 MB inventory.

## 2. Architecture and key decisions

ADRs in the fix-plan playbook, section 5.5: ADR-001 (vd-start number), ADR-002 (backoff + circuit breaker), ADR-003 (cold Chrome via vd-chrome.sh, 60 s CDP wait), ADR-004 (FIFO within priority, recorded fit skips), ADR-005 (restart investigation + startup hardening), ADR-006 (CDP port recording). No architecture change beyond those ADRs. No new dependencies. Error codes now run E01-E15.

## 3. Repository, branch, commit, release version

- Repo: github.com/movahedi-ca/mo. Local branch master, remote branch main. Local master and origin/main are UNRELATED histories; plain git push is impossible, no force-push used.
- Deployed commit (local): 67a000f (FIX-000..014 + NEW-001 + NEW-002). Plus 9d4d070 (FIX-017 deploy record). Working tree clean after FIX-018.
- No new git tag. v0.1.0 stays the release tag (remote main, 16e44943). The recovery point is the VM binary backup, not a tag.
- Binaries live: mo-daemon sha 443665b8, mo-cli sha 5195afb0. Daemon PID 166598 since 19:06:56 EST Nov 6.

## 4. Configuration and dependency changes

- CDP_WAIT_SECS = 60 (const in src/desktops.rs). MO_CDP_WAIT_SECS env override exists for tests only; 0 is ignored.
- Drain backoff 30/60/120/240 s, max 4 retries (in-memory state, no schema change).
- Circuit breaker: 3 consecutive identical error codes.
- New error code E15 (CDP timeout). No other config or dependency changes.

## 5. Tests and validation results

- Suite: 112 passed, 0 failed, 1 ignored (was 99). Clippy 0 warnings, fmt clean. Verified on the dev VM and on the chromebook-vm (twice in a row on the VM).
- New tests: TEST-043 (15-way storm), TEST-044 (CDP 60 s, incl. a literal 60.13 s run), TEST-045 (backoff), TEST-046 (breaker), TEST-047 (FIFO), TEST-048 (cancel), TEST-049 (reserving), TEST-050 (vd-start number), TEST-051 (CDP port row), TEST-054 (stale socket), plus NEW-001 and NEW-002 tests. TEST-052/053 are the regression stress tests. TEST-055 is the Chrome baseline check. TEST-056 is the Yahoo selector live check. TEST-057 is the 10-way harness stdout check.
- Regression stress test 1 (FIX-015, 10 agents, 19:10:27 EST): PASS. 4 at fire, 6 via drain, 0 never granted. 0 failed rows per request. PID stable. Socket responsive. 3 cold provisions, CDP responds. FIFO exact. Cleanup 0/0/1/24. Old run: 3/1/6.
- Regression stress test 2 (FIX-016, 15 vd-chrome agents, 19:17:58 EST): PASS. 4 at fire, 8 via drain, 3 never granted (a7, a10, a14, last 3 arrivals, 420 s poll cap). 0 failed rows per request. PID stable. Socket responsive. 12 of 12 cold provisions, CDP responds, 0 null ports. Extraction 12 of 12 granted agents, 2 of 2 each. FIFO exact. Cleanup 0/0/1/24. Old run: 0/2/13, 2 of 30 stocks.
- Smoke test (FIX-017): first cold vd-chrome grant ever (CDP 9333 responds, Protocol-Version 1.3, Chrome/156). cpu-ram grant. Clean releases. Back to baseline.

## 6. Before-and-after performance

- Status latency: 3 ms median baseline, unchanged (no regression measured).
- Grant latency: 4 ms baseline, unchanged.
- Idle RSS: 5.4 MB baseline; live daemon 4.9 MB before deploy.
- Test 1: 3 at fire / 1 drain / 6 never granted, 65 failed rows in ~4 min. After: 4 / 6 / 0, 0 failed rows.
- Test 2: 0 at fire / 2 drain / 13 never granted, 2 of 30 stocks. After: 4 / 8 / 3, 12 of 12 granted agents extracted 2 of 2.
- No formal benchmark re-run was done. The numbers above are the stress-test outcomes, not a benchmark pass.

## 7. Deployment and rollback instructions

Deploy (verified 2026-11-06, playbook section 14.3):

1. Push source to the VM build dir (~/mo-build): src/, Cargo.toml, Cargo.lock via cb-xfer.sh.
2. On the VM: cargo build --release --features cli; cargo test --release (must be green); clippy; fmt.
3. Back up the live binaries (mo-daemon.bak-YYYYMMDD-HHMM, mo-cli.bak-YYYYMMDD-HHMM).
4. Check: 0 leases, 0 queue, no GoA worker running.
5. systemctl --user stop mo-daemon. Install the new binaries. systemctl --user start mo-daemon.
6. Verify with mo-cli status. Smoke test: one cold vd-chrome provision (CDP must respond) and one cpu-ram grant. Release both. Verify cleanup (0/0/1/24).

Rollback (verified 2026-11-06): stop the daemon, restore the backup binaries, start, confirm it serves with mo-cli status, then redeploy the fix. The v0.1.0 recovery binaries are ~/.local/bin/mo-daemon.bak-20261106-1905 (sha 683b9d71) and mo-cli.bak-20261106-1905.

## 8. Monitoring and support

- mo-cli status: daemon, leases, queue, desktops, chrome_count, battery. Note: status chrome/desktops lag live ps up to 60 s (snapshot cache). Verify cleanup with live ps.
- mo-cli live: one consistent snapshot (status + queue + leases + desktops).
- Dashboard app: native GTK on :1, polls the socket every 3 s.
- Journal: journalctl --user -u mo-daemon. Startup line logs the PID. CDP wait outcome is logged. No new error codes except E15.
- Alerts in status: stop_failed, grant_failed, bypass. Stale alerts from test 2 remain in the list (old vd-stop.sh era).

## 9. Known issues and limitations

- TEST-010 real host reboot not run. No safe window yet.
- ARM64 cross build blocked (no cross linker).
- No staging host. Fixes deploy straight to the live host.
- TEST-055 should count vd-profile Chrome, not total chrome_count.
- mo-cli status lags live ps up to 60 s.
- The ~1 extra Chrome instance during the test 2 spike is unverified (owner unknown). A one-shot headless Chrome appeared during FIX-012 (owner unknown, not MO).
- The fulldayPrice Yahoo fallback selector rests on the test-2 record only, not re-verified live. The primary selector is verified live.
- In test 2 regression, 3 agents (last 3 arrivals) never got a grant within the 420 s poll cap. They were queued, not failed. The queue drains; cleanup was clean.
- mo-cli request returned E10 read timeout once while the daemon was provisioning; the request still landed.
- The regression drivers live at ~/workspace/mo-fix15/ (fix015.sh, fix016.sh, extract.py). extract.py is Python (throwaway harness, not MO code). MO itself stays Rust-only.
- The daemon journal was unreadable as user auto during the regression run (evidence gap). Grant, port, and chrome-count evidence stands.
- state.yaml parses as strict YAML now (fixed pre-existing breakage in the legacy deviations/defects lists). The duplicate `notes:` key was removed.

## 10. Outstanding tasks and next steps

- Push the fix commits to GitHub (local master 9d4d070; remote main is an unrelated history, so use the API push path, never force-push). Tag the release after the push only if a new tag is wanted; v0.1.0 stays the live tag.
- Re-run the benchmarks (status/grant latency, idle RSS) to confirm no regression from the fix code.
- TEST-010 (real host reboot) when a safe window exists.
- Tune the backoff/breaker/CDP values from live data if the drain shows new patterns.
- The 3 never-granted agents in test 2 regression: if 15-way bursts become routine, consider whether the 420 s agent poll cap or the drain pace needs tuning.
- Owner for all of the above: the implementer (AUTO). No handover to another person is needed.
