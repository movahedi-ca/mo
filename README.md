# MO Local Resource Assistant

One Rust package. One queue. Leases by SQLite. Unix socket only.
No HTTP server. No LLM. No network port.

Build: `cargo build --release`
Test: `cargo test`
Lint: `cargo clippy --all-targets` (zero warnings)
Format: `cargo fmt --check`

Runtime paths come from env (V3 section 9.4):
MO_DB_PATH, MO_SOCKET_PATH, MO_LOCK_PATH, MO_DASHBOARD_PATH.
Defaults live under ~/jobs/mo.

Playbook: ~/workspace/your_files/mo-local-resource-assistant-playbook-v3.md
