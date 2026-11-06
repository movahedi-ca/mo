# MO — Local Resource Assistant

**One queue. One SQLite database. Unix socket only.**  
No HTTP server. No network port. No LLM.

MO is a small, focused Rust daemon that manages scarce local resources (CPU cores, RAM, virtual desktops / Chrome instances) for cooperative agents and jobs on a single host. Agents request leases; MO queues, grants, monitors heartbeats, enforces battery and capacity gates, and reclaims resources when leases expire or agents disappear.

```
cargo build --release
cargo test
```

Version: **0.1.0** · License: **MIT** · MSRV: **1.89**

---

## Overview

MO solves the problem of multiple agents (or jobs) contending for limited local capacity on a host — especially a Chromebook-style VM that hosts multiple virtual desktops running Chrome with CDP/VNC endpoints.

- Agents submit **lease requests** (JSON) describing resource class, CPU, memory, duration, priority, and optional desktop needs.
- MO keeps a single ordered **queue** and a set of **active leases** in one SQLite file.
- Grants are capacity-aware (CPU load, MemAvailable, desktop count, optional bridge pool, battery state).
- Active leases require periodic **heartbeats**; missed heartbeats lead to stale → reclaim.
- A live **HTML dashboard** is written periodically for operator visibility.
- All client communication is over a **Unix domain socket** (mode 0600). There is no TCP listener and no HTTP server.

Intended users are developers and operators who run multiple cooperative workloads on the same machine and need deterministic, auditable resource allocation without a distributed scheduler.

---

## Key Features

- **Single-writer SQLite state** — one connection, WAL mode, durable (`synchronous=FULL`).
- **Lease lifecycle** — request → queue → reserve → grant → active → (extend) → release / expire / stale / reclaim.
- **Idempotent requests** — same `agent_id` + `idempotency_key` (UUID) returns the original outcome.
- **Priority & aging** — P0 / P1 / P2 with score-based aging so long-waiting lower-priority work is not starved.
- **Battery gates** — Normal / Warn / Restrict / Refuse / Recovery (configurable thresholds). Hosts without a battery capability skip the gate.
- **Capacity gates** — free CPU (reservation + load1), MemAvailable, desktop slots, optional bridge pool, disk free on the DB filesystem, Chrome budget.
- **Virtual-desktop management** — start/stop via external scripts, CDP liveness, fold/expand with safety checks (never folds `:1`).
- **Crash recovery** — on start, recovers interrupted / reserving state; detects reboot via `/proc/stat` btime.
- **Dashboard** — static HTML written atomically every N seconds (default 30).
- **Audit trail** — every significant event is recorded.
- **CLI** — `mo-cli` issues JSON commands over the socket and prints JSON responses.
- **systemd unit** — simple service with lock-file single-instance guard.

Explicit non-goals: no network exposure, no multi-host coordination, no LLM, no automatic wake of a powered-off host.

---

## Project Status

| Aspect | Status |
|--------|--------|
| Version | 0.1.0 |
| Maturity | Functional core complete; used on chromebook-vm and generic hosts |
| Platforms | Linux (x86_64 primary; aarch64 cross-build currently blocked on missing linker) |
| Tests | 94 unit tests passing (as of 2026-11-06 fix run) |
| Clippy / fmt | Zero warnings, clean |
| Known limitations | See [Limitations](#limitations) |

Development is driven by a private V3 playbook. Architecture-review findings (F01–F16) and defects found in adversarial testing have been addressed; see `implementation/` for the ledger and ADRs.

---

## Requirements

- **Rust** 1.89 or newer (edition 2021).
- **Linux** with `/proc`, Unix domain sockets, and standard file locking.
- Optional but typical for full desktop features:
  - `vd-start.sh` / `vd-stop.sh` scripts (paths configurable).
  - Chrome / Chromium instances exposing CDP ports.
  - Battery sysfs (`/sys/class/power_supply/BAT0`) when battery gates are desired.
- No external runtime services beyond the OS. SQLite is bundled via `rusqlite`.

Hardware: any Linux host; the reference adapter targets an 8 GB Chromebook-style VM. Memory and disk guards are configurable.

---

## Installation

### From source

```bash
git clone https://github.com/movahedi-ca/mo.git
cd mo
cargo build --release
```

Binaries appear in `target/release/`:

| Binary | Feature | Purpose |
|--------|---------|---------|
| `mo-daemon` | (default) | Long-running daemon |
| `mo-cli` | `--features cli` | Client CLI |

Install the CLI (optional):

```bash
cargo build --release --features cli
# or install both
cp target/release/mo-daemon ~/.local/bin/
cp target/release/mo-cli ~/.local/bin/   # after building with --features cli
```

### systemd (user unit)

A sample unit is provided:

```bash
mkdir -p ~/.config/systemd/user
cp systemd/mo-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now mo-daemon.service
```

The unit sets the standard environment variables and runs `%h/.local/bin/mo-daemon`. Adjust `ExecStart` if your binary lives elsewhere.

---

## Quick Start

1. **Start the daemon** (paths default under `~/jobs/mo`):

   ```bash
   mkdir -p ~/jobs/mo
   ./target/release/mo-daemon
   # → mo-daemon up: db=.../mo.db socket=.../mo.sock host=chromebook-vm
   ```

2. **Build the CLI** (if not already):

   ```bash
   cargo build --release --features cli
   ```

3. **Request a simple CPU/RAM lease**:

   ```bash
   cat > /tmp/req.json << 'EOF'
   {
     "agent_id": "demo-agent",
     "job_id": "job-1",
     "idempotency_key": "550e8400-e29b-41d4-a716-446655440000",
     "resource_class": "cpu-ram",
     "cpu_cores": 1,
     "mem_mb": 512,
     "desktops": 0,
     "duration_min": 15,
     "priority_class": "P2",
     "needs_bridge": false,
     "needs_display": false,
     "capabilities": []
   }
   EOF

   ./target/release/mo-cli request --file /tmp/req.json
   ```

   On success you receive a grant JSON containing `lease_id`, start/end times, etc. On capacity shortage you receive a queued response with reason and approximate next slot.

4. **Inspect state**:

   ```bash
   ./target/release/mo-cli status
   ./target/release/mo-cli leases
   ./target/release/mo-cli queue
   ./target/release/mo-cli health
   ```

5. **Heartbeat / release**:

   ```bash
   ./target/release/mo-cli heartbeat --lease <lease_id>
   ./target/release/mo-cli release --lease <lease_id>
   ```

6. **Dashboard** — open the file written at `$MO_DASHBOARD_PATH` (default `~/jobs/mo/dashboard.html`) in a browser. It auto-refreshes.

---

## Configuration

All runtime paths and tunables come from environment variables. Defaults live under `$HOME/jobs/mo`.

### Paths

| Variable | Default | Purpose |
|----------|---------|---------|
| `MO_DB_PATH` | `$HOME/jobs/mo/mo.db` | SQLite database |
| `MO_SOCKET_PATH` | `$HOME/jobs/mo/mo.sock` | Unix socket |
| `MO_LOCK_PATH` | `$HOME/jobs/mo/mo.lock` | Exclusive lock file |
| `MO_DASHBOARD_PATH` | `$HOME/jobs/mo/dashboard.html` | HTML dashboard output |
| `MO_HOST` | `chromebook-vm` | Host adapter name |
| `MO_PROFILE_ROOT` | `$HOME/.config` | Chrome profile root |
| `MO_JOBS_DIR` | `$HOME/jobs` | Directory containing vd-start/stop scripts |
| `MO_VD_START_SCRIPT` | `$MO_JOBS_DIR/vd-start.sh` | Desktop start script |
| `MO_VD_STOP_SCRIPT` | `$MO_JOBS_DIR/vd-stop.sh` | Desktop stop script |

### Tunables (selected)

| Variable | Default | Meaning |
|----------|---------|---------|
| `MO_LEASE_MAX_MIN` | 120 | Maximum lease duration (minutes) |
| `MO_EXTEND_MAX_MIN` | 30 | Maximum single extension |
| `MO_HEARTBEAT_MIN` | 5 | Heartbeat interval expectation |
| `MO_HEARTBEAT_MISS` | 2 | Misses before stale |
| `MO_RECLAIM_GRACE_MIN` | 5 | Grace before reclaim |
| `MO_INVENTORY_CACHE_SEC` | 60 | Inventory cache TTL |
| `MO_DASHBOARD_REFRESH_SEC` | 30 | Dashboard write interval |
| `MO_IDLE_FOLD_MIN` | 15 | Idle time before fold consideration |
| `MO_EXPAND_QUEUE_DEPTH` | 2 | Queue depth that triggers expand |
| `MO_RAM_GUARD_MB` | 1200 | Refuse grant if MemAvailable below this |
| `MO_CHROME_BUDGET_MB` | 1024 | Chrome memory budget |
| `MO_DISK_GUARD_GB` | 1 | Refuse if free space on DB filesystem below this |
| `MO_BATTERY_WARN` | 20 | % → Warn |
| `MO_BATTERY_RESTRICT` | 10 | % → Restrict |
| `MO_BATTERY_REFUSE` | 5 | % → Refuse |
| `MO_MAX_VD` | 10 | Configured maximum virtual desktops |
| `MO_SUSTAINED_VD` | 6 | Sustained desktop target for expand logic |

Host adapters currently recognized by `MO_HOST`:

- `chromebook-vm` / `chromebook` — full desktop + battery + bridge capabilities.
- `oracle-a1` / `oracle` — ARM64, no battery, no desktops, no bridge.
- `generic` — minimal capabilities (no battery/desktops/bridge).

Unknown host names produce error **E12**.

---

## Usage

### CLI commands

All commands talk to the running daemon over the Unix socket and print pretty-printed JSON.

```text
mo-cli request --file <path.json>     # submit lease request
mo-cli heartbeat --lease <id>
mo-cli extend --lease <id> --minutes <n>
mo-cli release --lease <id>
mo-cli status
mo-cli queue
mo-cli leases
mo-cli health
mo-cli fold --desktop <id>           # e.g. vd1 (safety-checked)
mo-cli expand                        # start one additional desktop if rules allow
mo-cli cancel --agent <id> --key <uuid>
mo-cli clear-flag --desktop <id>     # clear a flagged desktop after stop failure
```

### Request schema (JSON)

Required / common fields:

```json
{
  "agent_id": "string",
  "job_id": "string",
  "idempotency_key": "uuid",
  "resource_class": "vd-chrome | headless-chrome | cpu-ram | main-display",
  "cpu_cores": 1,
  "mem_mb": 512,
  "mem_mb_max": null,
  "desktops": 0,
  "duration_min": 30,
  "priority_class": "P0 | P1 | P2",
  "deadline": null,
  "needs_bridge": false,
  "needs_display": false,
  "capabilities": []
}
```

Validation highlights (errors are structured with code + next_action):

- `desktops` must be 0 or 1.
- `duration_min` 5–120.
- `cpu_cores` 1–4, `mem_mb` 256–4096.
- Idempotency key must be a valid UUID.
- Capability mismatches (e.g. desktops requested on a host without them) return **E02**.

### Error codes (E01–E14)

| Code | Typical meaning | Suggested next action |
|------|-----------------|-----------------------|
| E01 | Malformed request | Fix and resend with same idempotency key |
| E02 | Unknown class / missing host capability | Choose valid class or different host |
| E03 | Duration out of range | Use 5–120 minutes |
| E04 | No free slot / queued | Wait for next_slot |
| E05 | Battery gate | Wait for charge |
| E06 | Capacity (CPU/RAM/desktops) | Wait, release, or reduce request |
| E07 | Disk full | Free space |
| E08 | Unknown lease | Check lease id |
| E09 | Extension limit | Release and request new lease |
| E10 | Internal / IO / SQLite | Stop, recover, do not force grant |
| E11 | Second daemon | Check lock file |
| E12 | Unknown host | Set `MO_HOST` correctly |
| E13 | Fold safety / CDP / Chrome | Inspect leases and desktop state |
| E14 | Attempt to fold `:1` | Never fold the main display |

---

## Architecture

```
┌─────────────┐     Unix socket (JSON lines)     ┌──────────────────┐
│   mo-cli    │ ───────────────────────────────► │    mo-daemon     │
│  (optional) │ ◄─────────────────────────────── │                  │
└─────────────┘                                  │  ┌────────────┐  │
                                                 │  │  server    │  │
                                                 │  │  (handle) │  │
                                                 │  └─────┬──────┘  │
                                                 │        │         │
                                                 │  ┌─────▼──────┐  │
                                                 │  │ queue /    │  │
                                                 │  │ lease /    │  │
                                                 │  │ sched      │  │
                                                 │  └─────┬──────┘  │
                                                 │        │         │
                                                 │  ┌─────▼──────┐  │
                                                 │  │   store    │  │
                                                 │  │ (SQLite)   │  │
                                                 │  └────────────┘  │
                                                 │                  │
                                                 │  inventory ·     │
                                                 │  battery ·       │
                                                 │  desktops ·      │
                                                 │  dashboard ·     │
                                                 │  host adapters   │
                                                 └──────────────────┘
```

**Major modules** (under `src/`):

| Module | Responsibility |
|--------|----------------|
| `core` | Types, error codes, config, monotonic time helpers |
| `store` | SQLite schema, transactions, recovery, audit, prune |
| `queue` | Validation, submit, cancel, expiry |
| `lease` | Grant, heartbeat, extend, release, expire, reclaim, pick_desktop |
| `sched` | Ordered plan, free-capacity calculation, aging |
| `server` | Unix socket accept loop, command dispatch, drain_queue, tick |
| `inventory` | Load, memory, disk, Chrome count, bridge probe (cached) |
| `battery` | Sysfs reading and state machine |
| `desktops` | List / start / stop / fold / expand, CDP checks |
| `host` | Adapter trait + chromebook / oracle / generic implementations |
| `dashboard` | Gather + render static HTML |
| `cli` | clap-based command parsing (feature-gated) |

**Daemon threads**:

- Main: socket accept + per-connection handlers (2 s read timeout, 64 KiB line limit).
- Inventory snapshot every 60 s.
- Dashboard refresh (configurable, default 30 s) — gather once, write via temp + rename.
- Scheduler tick every 10 s (expire, sweep, reclaim, drain, bypass check, daily prune).

**Single-instance**: exclusive lock on `MO_LOCK_PATH` via `File::try_lock`. Second start fails with **E11**.

---

## Project Structure

```
.
├── Cargo.toml          # package, bins, features, release profile
├── Cargo.lock          # pinned (committed)
├── README.md
├── .gitignore
├── src/
│   ├── main.rs         # mo-daemon entry
│   ├── bin/mo-cli.rs   # mo-cli entry
│   ├── lib.rs
│   ├── core.rs
│   ├── store.rs
│   ├── queue.rs
│   ├── lease.rs
│   ├── sched.rs
│   ├── server.rs
│   ├── inventory.rs
│   ├── battery.rs
│   ├── desktops.rs
│   ├── host.rs
│   ├── dashboard.rs
│   ├── notify.rs
│   └── cli.rs          # behind feature = "cli"
├── systemd/
│   └── mo-daemon.service
└── implementation/     # internal ledger, ADRs, benchmarks, test evidence
    ├── state.yaml
    ├── adr-001-fix-run.md
    ├── benchmarks.md
    ├── review-validation.md
    └── test-evidence.md
```

---

## Development

```bash
# Format
cargo fmt

# Lint (zero warnings expected)
cargo clippy --all-targets -- -D warnings

# Unit tests
cargo test
# or release-mode tests
cargo test --release

# Build daemon only
cargo build --release

# Build CLI
cargo build --release --features cli
```

Release profile (from `Cargo.toml`):

- `opt-level = 3`
- thin LTO
- `codegen-units = 1`
- strip symbols
- panic = unwind (backtrace retained)

`clap` is optional and only linked into `mo-cli` when the `cli` feature is enabled; the daemon binary does not carry a CLI parser.

### Local testing tips

- Point all paths at a temporary directory:

  ```bash
  export MO_DB_PATH=/tmp/mo-test/mo.db
  export MO_SOCKET_PATH=/tmp/mo-test/mo.sock
  export MO_LOCK_PATH=/tmp/mo-test/mo.lock
  export MO_DASHBOARD_PATH=/tmp/mo-test/dashboard.html
  export MO_RAM_GUARD_MB=0          # if host MemAvailable is low
  export MO_DISK_GUARD_GB=0
  ```

- The test suite exercises many paths with in-memory or temporary fixtures; some adversarial scenarios require the real chromebook-vm environment (live Chrome, reboot, etc.).

---

## Testing

```bash
cargo test --release
```

As of the 2026-11-06 architecture-review fix run the library reports **94 passed, 0 failed**. Clippy is clean and `cargo fmt --check` is clean.

Additional evidence (benchmarks, adversarial results, defect tracking) lives under `implementation/`. Those documents record measurements on a specific host and are not claimed as universal performance guarantees.

---

## Troubleshooting

| Symptom | Likely cause | Action |
|---------|--------------|--------|
| `E11: mo-daemon already running` | Lock held | Check `MO_LOCK_PATH`; kill the other process or remove a truly stale lock (OS releases on death) |
| `E10: SQLite error … file is not a database` | Corrupt DB | Restore from backup or delete (loses state) |
| `E10: DB schema version N is newer than binary` | Binary too old | Upgrade the binary |
| Grants always queued / E04 | Capacity or battery | Inspect `mo-cli status`, lower guards for testing, check battery state |
| `E05` battery messages | Low battery | Charge or adjust `MO_BATTERY_*` thresholds |
| Dashboard not updating | Write failure or wrong path | Check permissions on `MO_DASHBOARD_PATH` parent; look for “dashboard write failed” on stderr |
| Second start succeeds after crash | Expected | `try_lock` is released by the kernel when the process dies |
| CLI cannot connect | Daemon down or wrong socket | Verify `MO_SOCKET_PATH` matches on both sides |

Diagnostic commands:

```bash
mo-cli health
mo-cli status
ls -l $MO_SOCKET_PATH $MO_LOCK_PATH $MO_DB_PATH
journalctl --user -u mo-daemon -f   # when using the systemd unit
```

---

## Performance and Operational Notes

Measurements recorded on a development host (see `implementation/benchmarks.md`) after the fix run:

- Status round-trip: median ~3 ms.
- Simple cpu-ram grant: ~4 ms (bridge-up path).
- Idle RSS: ~5.4 MB.
- Database size after 1000 leases (checkpointed): well under 1 MB.
- Dashboard refresh cadence holds the configured interval.

These numbers are host-specific and were obtained with test fixtures; treat them as indicative only.

Operational constraints:

- One daemon process per host (enforced by lock).
- Provisioning a virtual desktop runs on the connection thread; a hung start script occupies that thread until the 2 s read timeout / script timeout logic intervenes. Full asynchronous provision remains a future improvement.
- Socket mode is 0600; only the owning user can connect.

---

## Security and Privacy

- No network sockets are opened by MO itself.
- The Unix socket is created with mode 0600.
- The lock file is mode 0600.
- Secrets (if any) should never be placed in the SQLite database or request JSON that is logged; the current design stores only operational metadata.
- Report vulnerabilities via the repository’s issue tracker or by contacting the maintainer privately.

---

## Limitations

- **Linux only** — relies on `/proc`, Unix sockets, and `flock`-style locking.
- **Cooperative agents** — MO cannot prevent a process started outside its control from consuming resources; it can only detect and audit bypass desktops.
- **Single host** — no clustering or remote resource sharing.
- **Desktop count per request** — currently 0 or 1; multi-desktop reservation is not implemented.
- **ARM64 cross-compilation** — currently blocked on the absence of an aarch64 cross-linker on the documented build hosts (TASK-020).
- **External scripts** — `vd-start.sh` / `vd-stop.sh` are assumed to exist and behave correctly when desktop features are used; MO does not ship them.

---

## Contributing

1. Open an issue describing the problem or proposed change.
2. Fork, create a feature branch, and keep commits focused.
3. Ensure `cargo fmt`, `cargo clippy --all-targets`, and `cargo test` pass.
4. Update or add tests for new behavior.
5. Submit a pull request against `main`.

Internal implementation notes, ADRs, and the task ledger live under `implementation/`. Prefer updating those documents when making architectural changes.

---

## License

MIT License. See the `license` field in `Cargo.toml` and the repository license file if present.

---

## Support and References

- Source: https://github.com/movahedi-ca/mo
- Issues: https://github.com/movahedi-ca/mo/issues
- Internal design history: `implementation/state.yaml`, `implementation/adr-001-fix-run.md`

The original detailed playbook that drove the V3 design is kept outside this repository; the code and the files under `implementation/` are the authoritative description of the shipped system.
