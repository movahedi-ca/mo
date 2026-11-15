#!/bin/sh
# MO demo on a generic Linux host.
# Starts mo-daemon with MO_HOST=generic-linux in a fresh temp dir.
# Fires 15 concurrent cpu-ram lease requests. Heartbeats and releases
# the grants. Cancels whatever stays queued. Restarts the daemon on
# the same database and reads the state back.
# Prints a result table. Real output only: queued rows say queued.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DAEMON="$ROOT/target/release/mo-daemon"
CLI="$ROOT/target/release/mo-cli"

if [ ! -x "$DAEMON" ] || [ ! -x "$CLI" ]; then
    echo "demo: release binaries not found. Build them first:" >&2
    echo "  cargo build --release --features cli" >&2
    exit 1
fi

WORK=$(mktemp -d "${MO_DEMO_BASE:-$HOME}/.mo-demo.XXXXXX")
export MO_HOST=generic-linux
export MO_DB_PATH="$WORK/mo.db"
export MO_SOCKET_PATH="$WORK/mo.sock"
export MO_LOCK_PATH="$WORK/mo.lock"
export MO_DASHBOARD_PATH="$WORK/dashboard.html"

DAEMON_PID=""
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

start_daemon() {
    "$DAEMON" > "$WORK/daemon.log" 2>&1 &
    DAEMON_PID=$!
    i=0
    # Wait until the daemon answers, not just until a socket file
    # exists: after a restart the stale socket file is still there
    # for a moment and a connect in that window is refused.
    while ! "$CLI" health > /dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -gt 50 ]; then
            echo "demo: daemon did not answer health checks" >&2
            cat "$WORK/daemon.log" >&2
            exit 1
        fi
        sleep 0.2
    done
}

field() {
    # field <file> <json-key>: first string value for the key
    grep -o "\"$2\": \"[^\"]*\"" "$1" | head -1 | sed 's/.*: "//; s/"$//'
}

echo "== MO demo: generic-linux host =="
echo "workdir: $WORK"
START=$(date +%s)
start_daemon
echo "daemon up: pid $DAEMON_PID host $MO_HOST"
"$CLI" health | grep -E '"status"|"daemon"' | head -2 || true

echo "== firing 15 concurrent cpu-ram requests (1 core, 256 MB, 5 min, P2) =="
i=1
while [ "$i" -le 15 ]; do
    KEY=$(cat /proc/sys/kernel/random/uuid)
    printf '%s' "$KEY" > "$WORK/key-$i"
    cat > "$WORK/req-$i.json" <<EOF
{
  "agent_id": "demo-agent-$i",
  "job_id": "demo-job-$i",
  "idempotency_key": "$KEY",
  "resource_class": "cpu-ram",
  "cpu_cores": 1,
  "mem_mb": 256,
  "desktops": 0,
  "duration_min": 5,
  "priority_class": "P2",
  "needs_bridge": false,
  "needs_display": false,
  "capabilities": []
}
EOF
    i=$((i + 1))
done

PIDS=""
i=1
while [ "$i" -le 15 ]; do
    "$CLI" request --file "$WORK/req-$i.json" > "$WORK/out-$i.json" 2>&1 &
    PIDS="$PIDS $!"
    i=$((i + 1))
done
# Wait for the request processes only. A bare wait would also wait
# for the daemon, which runs until the restart step.
for p in $PIDS; do
    wait "$p" || true
done

GRANTED=0
QUEUED=0
i=1
while [ "$i" -le 15 ]; do
    st=$(field "$WORK/out-$i.json" status)
    case "$st" in
        granted) GRANTED=$((GRANTED + 1)) ;;
        *) QUEUED=$((QUEUED + 1)) ;;
    esac
    i=$((i + 1))
done
echo "first wave: granted $GRANTED, queued or other $QUEUED"

echo "== heartbeat and release for each grant =="
HB=0
REL=0
i=1
while [ "$i" -le 15 ]; do
    lease=$(field "$WORK/out-$i.json" lease_id)
    if [ -n "$lease" ]; then
        if "$CLI" heartbeat --lease "$lease" > "$WORK/hb-$i.json" 2>&1; then
            HB=$((HB + 1))
        fi
        if "$CLI" release --lease "$lease" > "$WORK/rel-$i.json" 2>&1; then
            REL=$((REL + 1))
        fi
    fi
    i=$((i + 1))
done
echo "heartbeats ok: $HB, releases ok: $REL"

echo "== cancel requests still queued =="
CAN=0
i=1
while [ "$i" -le 15 ]; do
    lease=$(field "$WORK/out-$i.json" lease_id)
    if [ -z "$lease" ]; then
        if "$CLI" cancel --agent "demo-agent-$i" --key "$(cat "$WORK/key-$i")" > /dev/null 2>&1; then
            CAN=$((CAN + 1))
        fi
    fi
    i=$((i + 1))
done
echo "cancelled: $CAN"

echo "== status before restart =="
"$CLI" status | grep -E '"active_leases"|"queued"|"daemon"' || true

echo "== restart smoke: same database =="
kill "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
sleep 1
start_daemon
echo "daemon restarted: pid $DAEMON_PID"
if "$CLI" status > "$WORK/status-after.json" 2>&1; then
    RECOVERED="yes, status served"
    grep -E '"active_leases"|"daemon"' "$WORK/status-after.json" || true
else
    RECOVERED="NO, status failed after restart"
    cat "$WORK/status-after.json" || true
fi
LEASE_ROWS=$("$CLI" leases 2>/dev/null | grep -c '"lease_id"' || true)
echo "lease rows readable after restart: $LEASE_ROWS"

END=$(date +%s)
echo "== result table =="
printf '%-28s %s\n' "requests fired" "15"
printf '%-28s %s\n' "granted first wave" "$GRANTED"
printf '%-28s %s\n' "queued first wave" "$QUEUED"
printf '%-28s %s\n' "heartbeats ok" "$HB"
printf '%-28s %s\n' "releases ok" "$REL"
printf '%-28s %s\n' "queued cancelled" "$CAN"
printf '%-28s %s\n' "restart recovered" "$RECOVERED"
printf '%-28s %s\n' "wall seconds" "$((END - START))"
echo "demo workdir kept for inspection: $WORK"
