# ADR-001: architecture-review fix run, 2026-11-06

## Context

The review of commit e6ef028 found 16 issues (F01-F16), two
footguns, and a systemd note. All 16 findings and both footguns
were validated CONFIRMED against the code. See
implementation/review-validation.md. Patches 0-10 fixed them.

## Decisions

### (a) synchronous stays FULL

Measured on this host: 200 single-row commits, FULL 0.03 ms
each, NORMAL 0.02 ms each. The difference is noise. The
Chromebook lost power on 2026-11-05, so kernel-crash durability
matters more than 0.01 ms per commit. FULL stays. If NORMAL is
reconsidered later, the durability lost is: a commit that
returned OK can vanish after a kernel crash or power loss.

### (b) wait_cdp / vd-start.sh stay on the connection thread

The full review shape (reserve, return, worker provisions) was
not taken. It changes cmd_request semantics and the two-phase
grant tests, and it is too big for this run. The safe subset
shipped: one thread per connection, 2 s read timeout, 64 KiB
line cap, single-flight snapshot, bridge only when needed.
A hung provision still occupies one connection thread, not the
whole daemon. Async provision is a follow-up, not a fix-run item.

### (c) panic=abort rejected

panic stays unwind. A panic in the field must still leave a
backtrace in the log. Binary size cost is small (daemon
2.8 MB). The release profile still takes opt-level 3, thin LTO,
codegen-units 1, strip.

## Recorded, not changed

- Extended and ExtensionPending lease states parse but are never
  written. extend keeps the lease active, which is what the
  sweep expects. Removing the states would churn the state
  machine for no behaviour change.
- The blanket From<std::io::Error> to E10 stays for `?`
  ergonomics. The client-facing misuse (CLI missing file) was
  fixed in patch 5, and the two daemon startup sites (socket
  bind/chmod, db dir creation) now map the error at the call
  site with a specific message.
- CDP/VNC port numbering (9332+n, 5902+n, main 9222/5901) is
  the documented vdK convention. It is only consulted when the
  adapter reports desktops, so an Oracle host never reaches it.
- The /home/auto string survives only as the $HOME fallback
  when HOME is unset. Profile and jobs paths derive from $HOME
  and are overridable (MO_PROFILE_ROOT, MO_JOBS_DIR).

## V3.1 playbook flags

These fixes change a V3 design decision. The playbook needs a
V3.1 amendment, not a rewrite:

- Interrupted leases expire 10 min after the last beat; until
  then they stay inside the unique desktop index.
- Scheduler tick every 10 s (expire, sweep, reclaim, drain)
  plus a drain after every release. V3 drained on request only.
- pick_desktop prefers a desktop already up; expand and
  provision are battery-gated. V3 picked the first free id.
- Heartbeats no longer write the heartbeats table. The last
  beat rides on leases.last_heartbeat_unix.
- Dashboard writes via tmp + rename, sleeps the configured
  interval, and prints host local time. No more fixed -5 / EST.
- validate rejects desktops other than 0 or 1. V3 allowed 2.
- Extend conflicts only when a queued request fits solely
  because this lease goes away. V3 blocked on any queued
  desktop request.
- Battery gate is skipped when the adapter reports no battery.
- New config: MO_CHROME_BUDGET_MB (default 1024),
  MO_PROFILE_ROOT, MO_JOBS_DIR.
- Build: clap is feature-gated (build mo-cli with
  --features cli), release profile opt-level 3 / thin LTO /
  strip, Cargo.lock committed.
