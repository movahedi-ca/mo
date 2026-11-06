#!/bin/bash
# kiosk-bridge: websockify launcher for the :0 kiosk viewer.
# Reads the target VNC port from ~/jobs/kiosk-target (default 5902 = :1).
# Started by novnc-kiosk.service (ExecStart). Use kiosk-view.sh to switch.
PORT="$(cat "$HOME/jobs/kiosk-target" 2>/dev/null || echo 5902)"
exec /usr/bin/python3 /usr/bin/websockify --web /usr/share/novnc 6081 "localhost:$PORT"
