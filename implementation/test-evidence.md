# MO test evidence — TASK-016 (adversarial) and TASK-020 (ARM64)

Date: 2026-11-06.
Binary: target/release, rustc 1.99.1.
Unit tests: 65 pass. Clippy: 0 warnings. Fmt: clean.
All live tests ran on /tmp paths only, never ~/jobs/mo.
Daemons were stopped after the run. Host left clean.

## Test conditions

- Host: this dev VM, not chromebook-vm. 4 cores. MemAvailable 635 to 760 MB.
- No BAT0 sensor. No bridge. No vd scripts. No Chrome.
- Guards set to 0 by env unless a test says otherwise.
- Fixtures in private mount namespaces (host untouched):
  fake BAT0 78 percent Discharging; fake meminfo 6000 MB available
  plus fake cpuinfo 16 cores (multi-lease tests only); fake vd
  scripts that open a TCP listener on the vd CDP port; fake
  bridge /health responder (bridge-up checks only).
- The fake fixtures test MO logic, not real Chrome or real VDs.
- Tests that need live Chrome, a reboot, or :1 need chromebook-vm.
- They are marked COULD-NOT-RUN below with the place to run them.

## TEST-001 to TEST-042

| ID | Scenario | Result | Evidence |
|---|---|---|---|
| TEST-001 | Inventory snapshot | PASS | Bench DB: 19 snapshots, 60 s cadence |
| TEST-002 | Queue 10 at once | PASS | TEST-020: 10 request rows, zero loss |
| TEST-003 | Grant | PASS | Live vd grant mo-1793995501-1e86d9; cpu-ram grants |
| TEST-004 | CDP fails after start | PASS | TEST-025: rollback, lease FAILED, 0 reservations |
| TEST-005 | Heartbeat missed twice | PASS | Fast instance: STALE at +3 min, audit rows |
| TEST-006 | Lease expires, Chrome lives | COULD-NOT-RUN | Needs live Chrome on chromebook-vm. Unit test expiry_marks_expired passes. Reclaim released the vd lease before expiry in the live run |
| TEST-007 | Fold leased desktop | PASS | Fold vd1 refused E13, live |
| TEST-008 | Battery 5 percent, not Charging | PASS | Batflip: new lease E05, live lease kept active |
| TEST-009 | Kill daemon minus 9 during grant | PASS | TEST-021: no double, no lost committed grant |
| TEST-010 | Core builds ARM64 | BLOCKED | No aarch64 linker on either host. See TASK-020 |
| TEST-011 | No LLM, no listen | PASS | ss: 0 daemon listeners. Grep: no TcpListener, no HTTP framework, no model calls |
| TEST-012 | Dashboard refresh | PASS | 11 intervals 23 to 31 s, max 31 s |
| TEST-013 | MO character shows | PASS | Live file: idle and busy faces. Warn/refuse in unit tests only |
| TEST-014 | Audit rows | PASS | Bench: request 32, reserve 32, grant 32, release 32. Denied, extend, skip rows in adv DBs |
| TEST-015 | Idempotency | PASS | Same agent plus key twice: second returns duplicate, 1 request row, 1 lease |
| TEST-016 | Same desktop, two agents | PASS | TEST-019: one vd10 grant, other queued |
| TEST-017 | Fold :1 | PASS | Fold main-display refused E14, live |
| TEST-018 | Wake claim | PASS | Doc grep: no wake claim; playbook states MO does not wake a powered-off host |
| TEST-019 | Two agents, same desktop, same second | PASS | 9 vds held, 2 racers for vd10: 1 grant, 1 queued, 10 active, max 1 per desktop |
| TEST-020 | Ten requests at once | PASS | Clean rerun: 10 granted, 10 distinct desktops, 10 active, max 1 per desktop, 10 request rows |
| TEST-021 | Daemon crash during grant | PASS | Kill -9 mid-provision: RESERVING kept fresh, 0 active. Restart after 331 s: recovery rolled back, lease FAILED, 0 reservations |
| TEST-022 | Crash between queue and lease | PASS | Request row stayed queued after crash. Next intake drained it and granted vd1 |
| TEST-023 | Agent disconnects, no release | PASS | Fast instance: STALE, then reclaim released both leases |
| TEST-024 | Bridge unavailable | PASS with deviation | needs_bridge with bridge down stays queued, E04 in response, no lease, no partial grant. Playbook says Denied; daemon queues instead |
| TEST-025 | Desktop starts, never reaches CDP | PASS | nocdp script: lease FAILED, 0 reservations, audit grant_failed "CDP never responded on vd1 port 9333" |
| TEST-026 | RAM falls under 1200 MB | PASS | Default guard on this host (MemAvailable 706 MB): audit "grant failed: MemAvailable 706 MB under guard 1200 MB", request denied, 0 leases |
| TEST-027 | Disk falls under 1 GB | PASS | Guard 200 GB: audit "grant failed: disk free 7.45 GB under guard 200 GB", request denied, 0 leases |
| TEST-028 | Battery reaches 5 percent during a job | PASS | Batflip: live lease stayed active, extension refused E05 |
| TEST-029 | Host shuts down unexpectedly | COULD-NOT-RUN | Needs a reboot on chromebook-vm, Phase 7. Unit test recovery_marks_interrupted_on_reboot passes |
| TEST-030 | State file corrupt | PASS | Garbage mo.db: daemon exits 1, "E10: SQLite error: file is not a database" |
| TEST-031 | Full disk prevents audit writes | PASS | 2 MB tmpfs for the DB, filled to 8 KB free: grant returns E10 "SQLite error: database or disk is full", no lease |
| TEST-032 | Stale lock | PASS | Second daemon while first runs exits E11. Lock file left after kill -9: restart succeeds, OS released the lock |
| TEST-033 | High priority waits behind incompatible requests | PASS | P0 vd head stays queued, P1 cpu-ram granted, skip audit rows present |
| TEST-034 | Queue has repeated extensions | PASS | 30 min lease: 3 extensions OK, 4th denied E09 "total 125 min exceeds max 120 min" |
| TEST-035 | MO tries to fold an active desktop | PASS | Fold vd1 refused E13 "active lease" |
| TEST-036 | Second daemon starts | PASS | Second daemon exits E11 "mo-daemon already running" |
| TEST-037 | Oracle adapter reports no battery | PASS with observation | vd request denied E02 "host has no desktops". cpu-ram granted. See OBS-004: inventory reads the sensor directly, not through the adapter |
| TEST-038 | Agent tries to bypass MO | FAIL | vd5 started directly: desktops_up 1, 0 leases, 0 audit events. No detection. See DEFECT-003 |
| TEST-039 | systemd unit fails after reboot | COULD-NOT-RUN | Needs a reboot on chromebook-vm, Phase 7. systemd-analyze verify exits 0 but flags StartLimit keys. See DEFECT-005 |
| TEST-040 | State version newer than binary | PASS | schema_version 99: daemon exits 1, "E10: DB schema version 99 is newer than binary 1" |
| TEST-041 | Desktop stop fails | FAIL | Release never stops the desktop; fold with CDP dead returns E04 but writes no audit row and flags nothing. See DEFECT-002 and DEFECT-004 |
| TEST-042 | Dashboard tab closed | PASS (daemon side) | Dashboard file kept updating every 30 s with no tab involved. Tab-close behaviour itself needs a :1 visual check in Phase 7 |

## 28 adversarial scenarios (Step 33)

| # | Scenario | Maps to | Result |
|---|---|---|---|
| 1 | Two agents request the same desktop | TEST-019 | PASS |
| 2 | Ten requests arrive at once | TEST-020 | PASS |
| 3 | Daemon crashes during a grant | TEST-021 | PASS |
| 4 | Crash between queue and lease updates | TEST-022 | PASS |
| 5 | Lease expires while an agent remains active | TEST-006 | COULD-NOT-RUN (live Chrome needed, chromebook-vm) |
| 6 | Agent disconnects without releasing | TEST-023 | PASS |
| 7 | Two agents use the same job ID | TEST-015 | PASS with note: two independent request rows; both denied by the DEFECT-001 "none" collision, not by job_id |
| 8 | Bridge becomes unavailable | TEST-024 | PASS (queued, no partial grant) |
| 9 | Desktop starts but never reaches CDP | TEST-025 | PASS |
| 10 | Desktop becomes unresponsive | TEST-006 | COULD-NOT-RUN (live Chrome needed, chromebook-vm) |
| 11 | RAM falls below 1200 MB during allocation | TEST-026 | PASS |
| 12 | Disk falls below 1 GB | TEST-027 | PASS |
| 13 | CPU load exceeds the agreed limit | — | GAP: no CPU-load admission gate exists. Load is only observed. See OBS-010 |
| 14 | Battery reaches 5 percent during a job | TEST-028 | PASS |
| 15 | Battery sensor returns unknown | TEST-008 | PASS live: no BAT0 here, battery Unknown, vd blocked E05, audit "battery gate: battery warn: new vd start blocked" |
| 16 | Host shuts down unexpectedly | TEST-029 | COULD-NOT-RUN (reboot needed, chromebook-vm) |
| 17 | State file becomes corrupt | TEST-030 | PASS |
| 18 | Full disk prevents audit writes | TEST-031 | PASS |
| 19 | Lock is stale or abandoned | TEST-032 | PASS |
| 20 | High-priority job waits behind incompatible requests | TEST-033 | PASS |
| 21 | Queue has repeated extensions | TEST-034 | PASS |
| 22 | MO attempts to fold an active desktop | TEST-035 | PASS |
| 23 | Second daemon attempts to start | TEST-036 | PASS |
| 24 | Oracle adapter reports unavailable capabilities | TEST-037 | PASS |
| 25 | Agent attempts to bypass MO | TEST-038 | FAIL (DEFECT-003) |
| 26 | systemd unit fails after reboot | TEST-039 | COULD-NOT-RUN (reboot needed, chromebook-vm) |
| 27 | State version is newer than running binary | TEST-040 | PASS |
| 28 | Desktop stop operation fails | TEST-041 | FAIL (DEFECT-002, DEFECT-004) |

## Defects found

### DEFECT-001 (major): cpu-ram leases collide on desktop "none"

- The unique index idx_leases_desktop_active covers every desktop
  value, including "none".
- Only one cpu-ram lease can be active at a time on a daemon.
- Proof: two agents, same job_id, cpu-ram, live adv instance.
- Both requests were marked denied. Audit shows the second grant
  failing on the desktop unique index.
- Fix: exclude "none" from the unique index, or store NULL
  desktop for no-desktop leases.

### DEFECT-002 (major): release does not stop the desktop

- `lease::release` only marks the lease released.
- It never calls stop_desktop.
- Proof: stopfail instance. After release, CDP 9333 still
  responded. The vd listener kept running.
- Consequence: the released desktop leaks. Fold then refuses
  it with E13 because CDP responds. Release leaks it, fold
  refuses it. The desktop can never be reclaimed through MO
  while its listener lives.

### DEFECT-003 (major): no bypass detection

- A desktop started without a lease produces no audit event.
- Proof: vd5 listener started directly. Status showed
  desktops_up 1 with 0 active leases. Audit had 0 matching rows.
- Playbook TEST-038 expects an audit event and an operator flag.
- The playbook also records this as LIMIT-001 (cooperative
  leases). The test expectation is still unmet.

### DEFECT-004 (minor): failed desktop stop is not flagged or audited

- With CDP dead and a failing stop script, fold returns E04
  "vd-stop vd1 exited exit status: 1". The retry-once inside
  stop_desktop ran. No audit row records the failure.
- No flagged-desktop state exists anywhere.
- Playbook TEST-041 expects the desktop to be flagged.

### DEFECT-005 (minor): systemd rate-limit keys in the wrong section

- mo-daemon.service puts StartLimitBurst and
  StartLimitIntervalSec under [Service].
- Both are [Unit] directives. systemd-analyze verify warns:
  "Unknown key name 'StartLimitIntervalSec' in section 'Service'".
- The settings are ignored. Restart rate limiting is inactive.
- Fix: move both keys to [Unit].

## Observations (not defects)

- OBS-001: CLI responses mask the real denial reason. Guard,
  battery, and provision failures inside drain_queue reach the
  client as generic "queued / E04 / Grant did not complete"
  while the request row is marked denied. The audit row holds
  the real code (E05, E06, E07). The client cannot tell
  "queued, retry later" from "denied, never".
- OBS-002: every grant pays about 600 ms of bridge retries
  (3 times 200 ms) when the bridge is down. With the bridge up
  a grant takes 8 to 10 ms.
- OBS-003: no startup drain. A queued request that survives a
  crash waits until the next request arrives. Verified live
  in TEST-022.
- OBS-004: inventory reads the battery sensor directly. It
  never calls adapter.battery(). The Oracle adapter says
  has_battery false, but the gate uses the sensor read anyway.
- OBS-005: skip audit rows repeat on every drain pass for the
  same blocked head. Noise, not a fault.
- OBS-006: no CLI cancel command exists, although
  queue::cancel is implemented. Queued requests clear only by
  24 h expiry or a DB edit.
- OBS-007: the reclaim table in V3 section 5.2 does not list
  the CDP-alive plus Chrome-dead case. The code releases it.
- OBS-008: the disk guard measures df "/" free space, not the
  filesystem that holds mo.db.
- OBS-009: status battery uses the 60 s cache. It can lag the
  grant gate by up to 60 s after a battery change. By design.
- OBS-010: no CPU-load admission gate exists anywhere in the
  code. Load is tracked and shown, never enforced.

## TASK-020 ARM64

- Status: BLOCKED on both hosts. No pass claimed.
- This host: rustup target aarch64-unknown-linux-gnu is
  installed. `cargo check --target aarch64-unknown-linux-gnu`
  fails in libsqlite3-sys with:
  "cc-rs: failed to find tool aarch64-linux-gnu-gcc:
  No such file or directory (os error 2)".
- Unblock here: install gcc-aarch64-linux-gnu on the build host.
- chromebook-vm via cb-ssh.sh: rustc 1.99.1 present.
- The aarch64 target is NOT installed there.
- No aarch64-linux-gnu-gcc there. No aarch64 dpkg packages.
- Unblock there: rustup target add aarch64-unknown-linux-gnu
  plus install gcc-aarch64-linux-gnu (or an equivalent cross
  toolchain) on the VM.
- TEST-010 stays BLOCKED until a host with a cross linker exists.

## Rig notes (for the coordinator)

- One early bench daemon was killed by my own pkill -f
  matching its own shell command line. The bench run was
  redone cleanly with setsid-detached daemons. Numbers in
  benchmarks.md are from the clean rerun.
- One filler file briefly filled the host /tmp to 100 percent
  during the first full-disk attempt. It was removed at once.
  /tmp is back to 3 percent used. The test was redone with the
  DB alone on a 2 MB tmpfs inside a private namespace.
- A stale mo2.sock file from an early aborted run sits in
  /tmp/mo-test/adv. It is test scratch, not MO state.
