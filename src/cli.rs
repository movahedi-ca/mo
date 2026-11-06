//! mo-cli commands. JSON in, JSON out. CLI never writes state directly.
//! Every command goes through the Unix socket to mo-daemon.

use crate::core::{LeaseRequest, Result};
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
    /// Fold a desktop when all 7 safety checks pass.
    Fold {
        #[arg(long)]
        desktop: String,
    },
    /// Expand one desktop when the queue rule allows it.
    Expand,
}

/// CLI entry.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket = crate::server::default_socket_path();
    let msg = match &cli.cmd {
        Cmd::Request { file } => {
            let text = std::fs::read_to_string(file)?;
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
        Cmd::Fold { desktop } => json!({ "cmd": "fold", "desktop": desktop }),
        Cmd::Expand => json!({ "cmd": "expand" }),
    };
    let resp = crate::server::call(&socket, &msg)?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
