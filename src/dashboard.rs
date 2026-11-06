//! Dashboard generator. Minimal placeholder for TASK-013.
//! Full static HTML plus MO character lands in TASK-019.

use crate::core::{Config, Inventory};
use crate::store::Store;

/// Render the dashboard HTML. TASK-019 replaces this placeholder.
pub fn render(_store: &Store, _inv: &Inventory, _cfg: &Config) -> String {
    "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"30\"><title>MO</title></head><body><h1>MO</h1><p>Dashboard lands in TASK-019.</p></body></html>".to_string()
}
