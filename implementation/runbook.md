# MO runbook

Operator guide for the MO daemon on chromebook-vm. Live since 2026-11-06.
Playbook: ~/workspace/your_files/mo-local-resource-assistant-playbook-v3.md (V3.1).
Repo: https://github.com/movahedi-ca/mo (branch main). Local repo: ~/workspace/mo (branch master).

## What MO does

MO queues lease requests for scarce local resources. Agents ask. MO grants.
MO never runs jobs. The agent runs the job through the bridge and mouse.
MO adds queue plus lease behind those paths. It does not replace them.

Resources: CPU cores, RAM, virtual desktops (vd1-vd10), main display :1.
MO never kills :1 Chrome. MO never folds :1. MO never opens a network port.
Unix socket only. No HTTP. No LLM on the VM.

## Where things live on chromebook-vm

- Binaries: ~/.local/bin/mo-daemon, ~/.local/bin/mo-cli
- Unit: ~/.config/systemd/user/mo-daemon.service (enabled)
- State: ~/jobs/mo/mo.db, mo.sock (600), mo.lock (600)
- Dashboard: ~/jobs/mo/dashboard.html (rewritten every 30 s)
- Backups: ~/.local/bin/mo-daemon.bak-20261106-1640, ~/jobs/mo/mo.db.bak-20261106-1640

## Daily checks

```bash
systemctl --user status mo-daemon --no-pager | head -6
~/.local/bin/mo-cli health
~/.local/bin/mo-cli status
~/.local/bin/mo-cli leases
~/.local/bin/mo-cli queue
```

Health must show daemon up, db ok, lock held.
Status shows battery, bridge, desktops up, load, MemAvailable, queue depth.
Alerts list bypass desktops and recent grant failures.

## Lease flow (operator test)

```bash
cat > /tmp/req.json <<'EOF'
{
  "agent_id": "tester", "job_id": "smoke",
  "idempotency_key": "<new-uuid-v4>",
  "resource_class": "cpu-ram", "cpu_cores": 1, "mem_mb": 512,
  "desktops": 0, "duration_min": 15, "priority_class": "P2",
  "needs_bridge": false, "needs_display": false, "capabilities": []
}
EOF
~/.local/bin/mo-cli request --file /tmp/req.json
~/.local/bin/mo-cli heartbeat --lease <lease_id>
~/.local/bin/mo-cli extend --lease <lease_id> --minutes 10
~/.local/bin/mo-cli release --lease <lease_id>
```

For a vd-chrome lease use resource_class vd-chrome, desktops 1,
needs_display false. needs_display true routes to main-display :1.
Do not request main-display unless the job truly needs :1.

## Warm desktops (Patch 11)

A vd desktop that is already up is reused, never restarted.
vd-start exits 1 on a running desktop, so MO checks CDP first.
The lease records started_desktop: 0 means warm reuse, 1 means MO started it.
Release and rollback stop a desktop only when started_desktop is 1.
MO never stops a desktop another agent or the operator started.

Known limitation: a warm desktop may already run another job's Chrome.
A lease on it is shared use. The holder must not assume a clean browser.
Recorded as an operator decision, not a bug. Do not change pick order silently.

## Single-flight drain (Patch 12)

The tick thread, the startup drain, and every request and release handler
all call drain_queue. Two concurrent drains granted one queued request
twice (live, 2026-11-06: one vd-chrome request got vd1 and vd2 in the
same second). drain_queue now holds a lock for the whole pass.
A second drain waits, re-lists, and finds the request already granted.

## Dashboard

File only, no server. Open file:///home/auto/jobs/mo/dashboard.html in :1 Chrome.
Pin the tab. Never open it while :1 runs live job-application work.
On 2026-11-06 the tab was NOT pinned: :1 showed a live WELL Health
careers session with a restore-pages dialog. Opening a tab then would
steal focus. Pin it at a quiet moment. The file itself refreshes every
30 s and shows the MO character, queue, leases, desktops, battery.

## Battery

Battery gates: Warn 20, Restrict 10, Refuse 5 percent.
A warm desktop needs no battery gate. Nothing new starts.
At Refuse, no new vd lease starts. cpu-ram leases still grant.
If the host powers off, only the physical power button brings it back.
MO never claims wake works.

## Restart and recovery

```bash
systemctl --user restart mo-daemon
```

Active leases survive a daemon restart. They stay active with the same
end time. Interrupted leases revive inside a 10 minute window, then expire.
The unit restarts on failure (Restart=on-failure, 5 s, burst 5 per 60 s).

## Upgrade

Build on the dev host, transfer, restart. Never build on the VM.

```bash
cargo build --release --features cli   # mo-cli needs --features cli
~/workspace/chromebook-vm/cb-xfer.sh push target/release/mo-daemon /home/auto/.local/bin/mo-daemon
~/workspace/chromebook-vm/cb-xfer.sh push target/release/mo-cli /home/auto/.local/bin/mo-cli
systemctl --user restart mo-daemon   # run on the VM via cb-job.sh
```

Back up the running binary and mo.db first. Schema migrations run on
daemon start (v1 to v2 to v3 chain). Never edit the DB by hand.

## Rollback

Stop the daemon. Restore the backup binary and DB. Start the daemon.

```bash
systemctl --user stop mo-daemon
cp ~/.local/bin/mo-daemon.bak-20261106-1640 ~/.local/bin/mo-daemon
cp ~/jobs/mo/mo.db.bak-20261106-1640 ~/jobs/mo/mo.db
rm -f ~/jobs/mo/mo.db-wal ~/jobs/mo/mo.db-shm
systemctl --user start mo-daemon
```

The DB backup was copied while the daemon ran, so it may lag the live
DB by seconds. Prefer a fresh copy after stopping the daemon when the
rollback is planned. No rollback was used on 2026-11-06.

## Push to GitHub

Local master and remote main share 0 commits. Plain git push fails.
Push through the API path only. Never force-push. Never rewrite history.

```bash
python3 -c "
import sys
from importlib.machinery import SourceFileLoader
mod = SourceFileLoader('gh', '/home/hatch/workspace/skills/github/bin/gh.py.legacy').load_module()
sys.modules['gh'] = mod
sys.argv = ['push_files.py', '--by', 'main-chat', 'movahedi-ca/mo',
  '<message>', '<local>=<remote>', ...]
exec(open('/home/hatch/workspace/movahedi-ca-tools/push_files.py').read(), {'__name__': '__main__'})
"
```

Verify after push: remote tree matches local tree blob for blob
(compare git ls-tree -r HEAD with the API tree). Tag a release only
after the live deploy passes: v0.1.0 on 16e44943 (2026-11-06).

## Troubleshooting

| Symptom | Cause | Action |
|---|---|---|
| E11: mo-daemon already running | Lock held | Check the lock. A second daemon must not start. |
| Grant queued with E04 vd-start exit 1 | Fixed in Patch 11 | Upgrade. Warm desktops grant without vd-start. |
| One request, two leases | Fixed in Patch 12 | Upgrade. Drain is single-flight now. |
| vd-chrome request lands on :1 | needs_display true | Use needs_display false for vd-chrome. |
| Grants always queued | Capacity or battery | Read mo-cli status. Check MemAvailable, load, battery. |
| Dashboard stale | Daemon down or write fail | Check daemon status. Check the dashboard mtime. |
| E05 battery | Low battery | Charge. Or wait. Do not lower gates silently. |
| CLI cannot connect | Daemon down or wrong socket | Check MO_SOCKET_PATH and daemon status. |

## Open items

- Dashboard tab on :1: not pinned yet. Pin at a quiet moment.
- TEST-010 (reboot recovery on a real host reboot): not run. No safe window yet.
- TEST-020 ARM64: blocked. No aarch64 cross linker on the build host or the VM.
- README on remote main was rewritten by another session during deploy
  (d8ffea0c, 0f3ed2c7, 2026-11-06 16:46-16:48 EST). Kept as is. Synced locally.
  It claims 94 tests. The count is 99 after Patch 12. Fix the number on
  the next README touch.
- No Oracle VM exists. No Oracle adapter test has run.
