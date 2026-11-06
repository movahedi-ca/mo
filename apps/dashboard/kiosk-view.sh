#!/bin/bash
# kiosk-view: point the :0 kiosk bridge at a desktop.
# Usage: kiosk-view.sh main|vd1..vd10
#   main -> VNC 5902 (:1 mirror feed, the normal view)
#   vdK  -> VNC 5902+K (vdK x11vnc; vd4 is 5906)
# View switching only. Never stops, kills, or folds any desktop.
# Never touches a lease. Writes the target, restarts the bridge.
set -u
TARGET="${1:?usage: kiosk-view.sh main|vd1..vd10}"
case "$TARGET" in
  main) PORT=5902 ;;
  vd*)
    K="${TARGET#vd}"
    case "$K" in ''|*[!0-9]*) echo "usage: kiosk-view.sh main|vd1..vd10" >&2; exit 1;; esac
    [ "$K" -lt 1 ] || [ "$K" -gt 10 ] && { echo "vd range is 1..10" >&2; exit 1; }
    PORT=$((5902 + K))
    ;;
  *) echo "usage: kiosk-view.sh main|vd1..vd10" >&2; exit 1 ;;
esac
echo "$PORT" > "$HOME/jobs/kiosk-target"
systemctl --user restart novnc-kiosk.service
echo "kiosk view: $TARGET (VNC $PORT)"
