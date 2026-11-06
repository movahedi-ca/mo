# MO Dashboard app

Standalone fullscreen viewer for the MO resource assistant dashboard.
Native GTK, no WebKit, no browser of any kind. One borderless
fullscreen window on the PHYSICAL display :0. No browser UI. No tabs.
No address bar. No Chrome. No VNC.

## Status: LIVE on :0 since 2026-11-06 18:16 EST (moved from :1)

- Binary: `~/.local/bin/mo-dashboard` (573 KB, Rust, release build)
- Window: fullscreen native on :0, the physical panel. Chrome kiosk
  runs underneath for vd viewing only, never for the dashboard.
- The :0 move: Debian runs on bare metal (DMI Shyvana/Google),
  Xorg :0 is the physical panel, :1 is TigerVNC. `DISPLAY=:0`
  works from the VM shell (same ~/.Xauthority). Proven with a
  trivial xterm on :0 first, then the app. Autostart is
  `~/jobs/mirror-viewer.sh` via `mirror-display.service`. The old
  :1 copy was stopped and its XFCE autostart entry disabled.
- On :0 the window hides while the kiosk view is a vd (kiosk Chrome
  comes forward) and shows itself again on BACK TO MAIN, driven by
  the 3 s poll loop from `~/jobs/kiosk-target`. On :1 it never hid.
- Live data: polls the daemon Unix socket `~/jobs/mo/mo.sock`
  (`{"cmd":"live"}`) every 3 s. Numbers move without a page reload.
  No network port. No HTTP.
- Theme: 1960s. Warm cream, teal, burnt orange, brown, monospace
  type, Cairo-drawn retro MO face. Same theme in the static
  `dashboard.html` the daemon writes every 30 s.
- Clickable desktops: a tile click runs `~/jobs/kiosk-view.sh`,
  which re-points the :0 kiosk bridge at that desktop's VNC port
  (5902 = main :1, 5902+K = vdK). View switching only. A
  "BACK TO MAIN" window sits on the viewed desktop and returns
  the kiosk view to :1.
- Display off: a manual button only, never automatic. It drives
  the physical display :0 (eDP-1) through DPMS. :1 is TigerVNC
  and has no DPMS extension. `mo-cli display off` / `mo-cli
  display on` do the same from anywhere.
- Autostart: `~/jobs/mirror-viewer.sh` via `mirror-display.service`
  (xinit on :0). The old XFCE autostart entry
  (`~/.config/autostart/mo-dashboard.desktop`) is disabled.
- Verified live: :0 framebuffer screenshots (scrot, not VNC) show
  the dashboard edge to edge, no Chrome frame, live numbers moving
  MO character top left, live numbers moving within seconds,
  tile click switching the kiosk view to vd1 and back, display
  off/on with live state from `xset -display :0 q`

## Build (on chromebook-vm)

```sh
sudo apt-get install -y libgtk-3-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
install -m 755 target/release/mo-dashboard ~/.local/bin/mo-dashboard
```

No webkit2gtk. The WebKit build was replaced 2026-11-06 17:40 EST.
The C fallback (`mo-dashboard.c`) was written, never compiled, and
is unused. It stays in the repo as history only.

## Run

```sh
DISPLAY=:0 mo-dashboard &
DISPLAY=:0 mo-dashboard --back   # small back-to-main window, on $DISPLAY
```

## Kiosk scripts

- `kiosk-view.sh main|vd1..vd10` — write the target VNC port to
  `~/jobs/kiosk-target` and restart `novnc-kiosk.service`
- `kiosk-bridge.sh` — websockify launcher for the :0 kiosk viewer,
  reads `~/jobs/kiosk-target` (default 5902). Started by
  `novnc-kiosk.service` (ExecStart).

## Display off commands

```sh
# off (timers zeroed first so the blank can never self-trigger)
xset -display :0 dpms 0 0 0
xset -display :0 dpms force off
# on (restores the as-found state: DPMS disabled)
xset -display :0 dpms force on
xset -display :0 -dpms
# state (live)
xset -display :0 q   # "Monitor is Off" means off
# or
mo-cli display off
mo-cli display on
```

Wake is by any input on :0. A screenshot never goes black for
this route: DPMS powers the panel while the framebuffer keeps its
image. The proof is the xset state, not a black frame.
