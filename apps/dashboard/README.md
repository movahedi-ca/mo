# MO Dashboard app

Standalone fullscreen viewer for the MO resource assistant dashboard.
One borderless fullscreen window. Loads the live dashboard file.
No browser UI. No tabs. No address bar. No Chrome.

## Status: LIVE on chromebook-vm since 2026-11-06 17:25 EST

- Binary: `~/.local/bin/mo-dashboard` (326 KB, Rust, release build)
- Window: fullscreen on :1, on top of the main Chrome (tabs untouched behind it)
- Loads `file:///home/auto/jobs/mo/dashboard.html` directly, refreshes with it
- Autostart: `~/.config/autostart/mo-dashboard.desktop` (MO Dashboard)
- Verified live: screenshot of :1 shows the dashboard edge to edge, MO
  character visible top right, no browser UI

## Build (on chromebook-vm)

```sh
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
install -m 755 target/release/mo-dashboard ~/.local/bin/mo-dashboard
```

## Run

```sh
DISPLAY=:1 mo-dashboard &
```

## Language: Rust (shipped)

Rust first, per plan. cargo 1.99.1 via rustup on the VM, webkit2gtk 2.54.0.
Build fixes found on the way (webkit2gtk 2.0.2 crate):
- `set_sandbox_enabled` needs Cargo feature `v2_26` (set in Cargo.toml).
- `WebContext::new()` is not usable in this crate version; the inherent
  `WebContext::default()` returns the shared default context (Option).
- gtk 0.18 has no `gtk::Inhibit`; close handling uses `connect_destroy`.

## Sandbox note

`WebContext::set_sandbox_enabled(false)` is set in code. The dashboard is
a local file under /home/auto/jobs/mo. WebKitGTK's default sandbox denies
that path (surf hit the same wall: file under /home/auto/jobs/mo failed
with "Permission denied" even with WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1,
while the same file copied to /tmp loaded). The file is written by
mo-daemon on this host only. No remote content is ever loaded.

## Step 1 note (surf, superseded)

surf 2.1 was installed and ran fullscreen on :1 first (2026-11-06 ~17:15 EST)
as the fast step. It could not read the live file path (sandbox, see above),
so the live file was served over loopback HTTP (python3 http.server on
127.0.0.1:8931) for that step only. The HTTP shim was stopped when the
dedicated app took over (17:25 EST). surf is no longer running.

## C fallback (unused)

`mo-dashboard.c` is the same app in C (gcc, gtk+-3.0 + webkit2gtk-4.1).
It was written while the Rust build was fixed, and never compiled or run.
Kept as a fallback only. The shipped binary is Rust.
