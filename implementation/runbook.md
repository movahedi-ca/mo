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

Two views of the same data. The static file `~/jobs/mo/dashboard.html`
is rewritten by the daemon every 30 s (1960s theme since 2026-11-06
17:35 EST: cream #F7EEDC, teal #0E7C7B, burnt orange #C65D21, brown
#5C3A21, monospace). The live view is the native MO Dashboard app
below, which polls the daemon socket every 3 s. The pinned-tab plan
is superseded: the app is the display now, no browser at all.

## Dashboard app (native GTK, no WebKit, no Chrome)

`apps/dashboard/`, binary `~/.local/bin/mo-dashboard`, autostart
`~/.config/autostart/mo-dashboard.desktop`. One borderless fullscreen
window on :1. Rust, gtk 0.18 + cairo-rs 0.18 + serde + libc. The
WebKit build (webkit2gtk 2.0.2) was replaced 2026-11-06 17:40 EST;
the C fallback `mo-dashboard.c` was never compiled and is unused.

Live data: polls `~/jobs/mo/mo.sock` with `{"cmd":"live"}` every 3 s
(1 s timeouts). No page reload, no network port, no HTTP. Numbers
move within seconds: verified 17:51:40 -> 17:52:13 EST (queue 2 -> 1,
leases 1 -> 3, load 3.35 -> 2.74, RAM 3736 -> 3744 MB).

Clickable desktops: every tile is a button. A tile click runs
`~/jobs/kiosk-view.sh vdK`, which writes the VNC port to
`~/jobs/kiosk-target` (5902 = main :1, 5902+K = vdK; vd1 = 5903,
vd4 = 5906) and restarts `novnc-kiosk.service`. The :0 kiosk bridge
(`~/jobs/kiosk-bridge.sh`, ExecStart of novnc-kiosk.service) reads
that file and serves the target on 6081. View switching only: never
stops, kills, or folds a desktop, never touches a lease. A small
`mo-dashboard --back` window ("BACK TO MAIN", 360x100 at 780,24) sits
on the viewed desktop; clicking it runs `kiosk-view.sh main` and
exits. Port correction, 2026-11-06: the task brief said 5901+K; the
real VNC ports are 5902+K (TigerVNC :1 is 5901, the kiosk bridge
reads the :1 mirror feed on 5902). The daemon's `live` output
confirms it (main-display vnc_port 5901 is the TigerVNC server, vdK
is 5902+K).

Display off: a manual button only, never automatic. The physical
display is :0 (eDP-1). :1 is TigerVNC and has NO DPMS extension, so
`DISPLAY=:1 xset dpms force off` fails ("server does not have
extension for dpms option"); `xset s activate` also does nothing on
:1 (the saver never blanks the VNC framebuffer). The button and
`mo-cli display` therefore drive :0:

- off: `xset -display :0 dpms 0 0 0` (zero the timers so the blank
  can never self-trigger) then `xset -display :0 dpms force off`
- on: `xset -display :0 dpms force on` then `xset -display :0 -dpms`
  (restores the as-found state: DPMS disabled)
- state (live): `xset -display :0 q`, "Monitor is Off" means off;
  no "Monitor is" line means DPMS is disabled, which is also on
- `mo-cli display off` / `mo-cli display on` run the same commands
  and print the live state back as JSON

Wake is by any input on :0 (mouse move or key press). Verified
2026-11-06 18:00-18:02 EST: button press -> "Monitor is Off" +
app label "DISPLAY: OFF" + button "DISPLAY ON"; mouse move on :0 ->
"Monitor is On" + dashboard back; `mo-cli display on` -> DPMS
Disabled, timers 600/600/600 (as found). Note: a screenshot never
goes black for this route, because DPMS powers the panel while the
framebuffer keeps its image; the proof is the xset state, not a
black frame. The xrandr route (`--output VNC-0 --off`) DOES blank
the framebuffer but `--auto` does NOT restore it (stays 32x32,
"VNC-0 disconnected"); restore needs an explicit
`--mode 1920x1080`, and the RandR change pops the Xfce Display
settings dialog. Do not use xrandr for display off.

Known app bugs, found live 2026-11-06 and fixed:
- RefCell double borrow in the back-window reap block
  (refresh() held one borrow_mut while taking a second) panicked
  the app the first time a back click was reaped (17:52 EST, app
  aborted, Chrome came forward). Fixed: check-then-clear with one
  borrow at a time. Re-verified: back click 18:00 EST, app alive
  12 s later, no crash.
- GTK theme painted buttons with its own gradient, hiding the
  1960s colors (white text on pale tiles, unreadable DISPLAY OFF
  button). Fixed: `background-image: none` on button and desk
  classes. The :hover rule also washed the big button to cream;
  fixed with a darker-orange `.big-button:hover`.
- Startup drain blocks the socket: the daemon runs
  `drain_queue()` on the main thread BEFORE `serve()` binds the
  socket, and the drain loop has no cap. With a non-empty queue
  (stress session, 2026-11-06 17:46 EST) the socket stayed
  refused for ~5 minutes (main thread in nanosleep inside the
  grant/provision poll) while the tick thread kept working. It
  cleared on its own when the queue drained. Fix pending: move
  the startup drain into a background thread (or cap it). Do not
  restart the daemon to clear this while another session holds
  leases: a restart kills the daemon's children (vd Xvfb/openbox)
  and breaks their leases.

Kiosk scripts live in the repo at `apps/dashboard/kiosk-view.sh`
and `apps/dashboard/kiosk-bridge.sh`, installed in `~/jobs/`.

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
| CLI cannot connect | Daemon down or wrong socket | Check MO_SOCKET_PATH and daemon status. If the daemon just restarted with a non-empty queue, the startup drain can hold the socket for minutes (see Dashboard app section); wait, do not restart while another session holds leases. |
| App shows DAEMON: DOWN after a daemon restart | Startup drain blocking serve() | Same as above. The socket returns when the queue drains. |

## Open items

- Dashboard tab on :1: SUPERSEDED by the standalone app (no browser for the display). Do not pin a tab.
- Startup drain blocks the socket (see Dashboard app section): move the startup drain_queue() into a background thread or cap it. Not fixed in this run; needs a daemon restart at a quiet moment (no live leases).
- TEST-010 (reboot recovery on a real host reboot): not run. No safe window yet.
- TEST-020 ARM64: blocked. No aarch64 cross linker on the build host or the VM.
- README on remote main was rewritten by another session during deploy
  (d8ffea0c, 0f3ed2c7, 2026-11-06 16:46-16:48 EST). Kept as is. Synced locally.
  It claims 94 tests. The count is 99 after Patch 12. Fix the number on
  the next README touch.
- No Oracle VM exists. No Oracle adapter test has run.
