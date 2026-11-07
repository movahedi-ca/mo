# MO fix plan: discovery notes (FIX-000, FIX-001)

Date: 2026-11-06 18:45 EST.
Repo: ~/workspace/mo, branch master, HEAD b8e2ee0, tree clean.
Remote main HEAD: 097dd51. Remote HEAD: 1bf41bc.
Local master and origin/main are unrelated histories.
Playbook claim "remote HEAD 49934d33" does not exist anywhere.
Tag v0.1.0 exists on remote main at 16e44943, not in the local repo.
Recovery point: VM binary backup ~/.local/bin/mo-daemon.bak-20261106-1640 (verified present).

## Baselines
- cargo test --release: 99 passed, 0 failed.
- clippy: 0 warnings. fmt: clean.
- cargo 1.99.1, rustc 1.99.1, rusqlite 0.32.1.
- Chrome 156.0.8078.73, Debian 13 trixie.
- Live daemon: PID 129277, up since 17:46:17 EST, 0 leases, 0 queue, chrome_count 24.

## Current values
- CDP wait: 10 s (desktops.rs wait_cdp(port, 10)).
- Drain tick: 10 s. No per-request backoff. No failure cap.
- Queue list: ORDER BY id ASC (arrival).
- Plan order: score (class weight + wait/10), then deadline, then created_unix.
- Cancel: sets request status cancelled, never checks for an existing lease.
- vd-start arg: MO passes the name (vd1). Script accepts names now, so live calls work.
- vd-chrome.sh: does NOT exist on the VM. Playbook mismatch. FIX-004 must create it.
- CDP port: lease row gets the computed port from desktop_binding. Warm-desktop actual port is never verified.

## FIX-001: restart cause
- The daemon did NOT crash.
- Both restarts (17:35:53, 17:46:17 EST) were deliberate redeploys by the dashboard-app work.
- Evidence: ledger dashboard_app_2026_11_06.daemon_live_cmd records redeploys at 17:35 and 17:46 EST. Binary backups -1735 and -1746 exist. Journal shows clean stop/start. No panic. No OOM. NRestarts 0.
- Socket outage cause: main.rs runs drain_queue() BEFORE serve() binds the socket. A startup drain over failing provisions blocks the bind for minutes.
- vd-stop.sh rejected vdK names through 17:52 EST. The fix landed during cleanup, not setup.
- a4 reserving row did not vanish from the DB. Rollback marked it failed. It only left the active view.

## New bug
- NEW-001: ReclaimProbe.lock_held is computed but never used. reclaim_check ignores it. Fix: wire it in as a KeptStale condition, with a test.
