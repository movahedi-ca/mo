//! mo-cli commands. JSON in, JSON out. CLI never writes state directly.
//! Every command goes through the Unix socket to mo-daemon.

use crate::core::{LeaseRequest, MoError, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "mo-cli", version, about = "MO Local Resource Assistant client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Request a lease from a JSON file.
    Request {
        /// Path to the request JSON file.
        #[arg(long)]
        file: String,
    },
    /// Send a heartbeat for a lease.
    Heartbeat {
        #[arg(long)]
        lease: String,
    },
    /// Extend a lease. Max 30 minutes, max total 120.
    Extend {
        #[arg(long)]
        lease: String,
        #[arg(long)]
        minutes: i64,
    },
    /// Release a lease.
    Release {
        #[arg(long)]
        lease: String,
    },
    /// Show daemon status.
    Status,
    /// Show the ordered queue.
    Queue,
    /// Show active leases.
    Leases,
    /// Show daemon health.
    Health,
    /// Show one consistent live snapshot: status + queue + leases + desktops.
    Live,
    /// Turn the :1 display on or off via DPMS. Local only, never touches
    /// the daemon, a lease, or any desktop. State: on or off.
    Display {
        /// on or off.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Fold a desktop when all 7 safety checks pass.
    Fold {
        #[arg(long)]
        desktop: String,
    },
    /// Expand one desktop when the queue rule allows it.
    Expand,
    /// Cancel a queued request by agent + idempotency key.
    Cancel {
        /// Agent id that submitted the request.
        #[arg(long)]
        agent: String,
        /// Idempotency key of the request.
        #[arg(long)]
        key: String,
    },
    /// Clear a flagged desktop so fold can run on it again.
    ClearFlag {
        /// Desktop id, for example vd1.
        #[arg(long)]
        desktop: String,
    },
}

/// CLI entry.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Display is local only: it runs xset directly and never talks to
    // the daemon, so it works even when the screen is off and the
    // dashboard cannot be seen. The physical display is :0 (eDP-1).
    // :1 is TigerVNC and has no DPMS extension, so xset dpms fails
    // there; :0 has DPMS. Off zeroes the DPMS timers first, so the
    // blank can never self-trigger; on restores the as-found state
    // (DPMS disabled), so it never blanks on its own either.
    if let Cmd::Display { state } = &cli.cmd {
        let run = |args: &[&str]| std::process::Command::new("xset").args(args).output();
        let out = match state.as_str() {
            "off" => {
                let _ = run(&["-display", ":0", "dpms", "0", "0", "0"]);
                run(&["-display", ":0", "dpms", "force", "off"])
            }
            "on" => {
                let r = run(&["-display", ":0", "dpms", "force", "on"]);
                let _ = run(&["-display", ":0", "-dpms"]);
                r
            }
            _ => unreachable!("clap restricts display state to on|off"),
        }
        .map_err(|e| MoError::e10(format!("cannot run xset: {e}")))?;
        if !out.status.success() {
            return Err(MoError::e10(format!(
                "xset dpms force {state} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let live = run(&["-display", ":0", "q"])
            .map(|o| !String::from_utf8_lossy(&o.stdout).contains("Monitor is Off"))
            .unwrap_or(true);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "display": if live { "on" } else { "off" },
                "status": "ok",
            }))?
        );
        return Ok(());
    }
    let socket = crate::server::default_socket_path();
    let msg = match &cli.cmd {
        Cmd::Request { file } => {
            let text = std::fs::read_to_string(file)
                .map_err(|e| MoError::e01(format!("cannot read request file {file}: {e}")))?;
            let req: LeaseRequest = serde_json::from_str(&text)?;
            json!({ "cmd": "request", "request": req })
        }
        Cmd::Heartbeat { lease } => json!({ "cmd": "heartbeat", "lease_id": lease }),
        Cmd::Extend { lease, minutes } => {
            json!({ "cmd": "extend", "lease_id": lease, "minutes": minutes })
        }
        Cmd::Release { lease } => json!({ "cmd": "release", "lease_id": lease }),
        Cmd::Status => json!({ "cmd": "status" }),
        Cmd::Queue => json!({ "cmd": "queue" }),
        Cmd::Leases => json!({ "cmd": "leases" }),
        Cmd::Health => json!({ "cmd": "health" }),
        Cmd::Live => json!({ "cmd": "live" }),
        Cmd::Fold { desktop } => json!({ "cmd": "fold", "desktop": desktop }),
        Cmd::Expand => json!({ "cmd": "expand" }),
        Cmd::Cancel { agent, key } => json!({
            "cmd": "cancel",
            "agent_id": agent,
            "idempotency_key": key
        }),
        Cmd::ClearFlag { desktop } => json!({ "cmd": "clear-flag", "desktop": desktop }),
        Cmd::Display { .. } => unreachable!("display is handled before the socket call"),
    };
    let resp = crate::server::call(&socket, &msg)?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
