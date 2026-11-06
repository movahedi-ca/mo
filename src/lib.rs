//! MO Local Resource Assistant. One package, modules inside.
//! V3 playbook: one SQLite file, Unix socket only, no HTTP, no LLM.

pub mod battery;
#[cfg(feature = "cli")]
pub mod cli;
pub mod core;
pub mod dashboard;
pub mod desktops;
pub mod host;
pub mod inventory;
pub mod lease;
pub mod notify;
pub mod queue;
pub mod sched;
pub mod server;
pub mod store;
