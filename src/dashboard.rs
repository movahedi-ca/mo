//! Dashboard generator: static HTML, meta refresh, no JS, no forms.
//! MO character is inline SVG under 2 KB. Face matches state:
//! idle smile, busy straight mouth, warn amber flat mouth, refuse red frown.
//! Dashboard never writes state. Dashboard never calls the socket.
//! Read only. Closing the tab is safe.

use crate::core::{est_label, now_unix, BatteryState, Config, Inventory};
use crate::store::Store;

/// Dashboard state for the MO face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoFace {
    Idle,
    Busy,
    Warn,
    Refuse,
}

/// Pick the face: refuse beats warn beats busy beats idle.
pub fn face_for(battery: BatteryState, active_leases: usize) -> MoFace {
    match battery {
        BatteryState::Refuse => MoFace::Refuse,
        BatteryState::Warn | BatteryState::Restrict | BatteryState::Unknown => MoFace::Warn,
        BatteryState::Normal | BatteryState::Recovery => {
            if active_leases > 0 {
                MoFace::Busy
            } else {
                MoFace::Idle
            }
        }
    }
}

/// MO character as inline SVG. Teal #0E7C7B head, white dot eyes,
/// one line mouth, one antenna line with one dot, M O under the face.
pub fn mo_svg(face: MoFace) -> String {
    let (head, mouth) = match face {
        MoFace::Idle => ("#0E7C7B", "M28 56 Q40 64 52 56"),
        MoFace::Busy => ("#0E7C7B", "M30 58 L50 58"),
        MoFace::Warn => ("#B26A00", "M30 58 L50 58"),
        MoFace::Refuse => ("#B3261E", "M28 60 Q40 52 52 60"),
    };
    format!(
        "<svg width=\"80\" height=\"100\" viewBox=\"0 0 80 100\" role=\"img\" aria-label=\"MO\">\
        <line x1=\"40\" y1=\"4\" x2=\"40\" y2=\"16\" stroke=\"{head}\" stroke-width=\"3\"/>\
        <circle cx=\"40\" cy=\"6\" r=\"4\" fill=\"{head}\"/>\
        <rect x=\"8\" y=\"16\" width=\"64\" height=\"56\" rx=\"12\" fill=\"{head}\"/>\
        <circle cx=\"28\" cy=\"38\" r=\"5\" fill=\"#FFFFFF\"/>\
        <circle cx=\"52\" cy=\"38\" r=\"5\" fill=\"#FFFFFF\"/>\
        <path d=\"{mouth}\" stroke=\"#FFFFFF\" stroke-width=\"3\" fill=\"none\" stroke-linecap=\"round\"/>\
        <text x=\"40\" y=\"92\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"16\" fill=\"#333333\">M O</text>\
        </svg>"
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the full dashboard page.
pub fn render(store: &Store, inv: &Inventory, cfg: &Config) -> String {
    let queued = store.list_queued_requests().unwrap_or_default();
    let leases = store
        .list_leases(&[
            "active",
            "granted",
            "reserving",
            "stale",
            "extension_pending",
        ])
        .unwrap_or_default();
    let battery_state = crate::battery::effective_state(inv.battery.as_ref());
    let face = face_for(battery_state, leases.len());
    let face_name = match face {
        MoFace::Idle => "idle",
        MoFace::Busy => "busy",
        MoFace::Warn => "warn",
        MoFace::Refuse => "refuse",
    };
    let battery_pct = inv.battery.as_ref().and_then(|b| b.pct).unwrap_or(0);
    let battery_label = match &inv.battery {
        Some(b) => match b.pct {
            Some(p) => format!("{p}%"),
            None => "unknown".to_string(),
        },
        None => "no sensor".to_string(),
    };
    let bridge_label = match (inv.bridge_known, inv.bridge_pool_free) {
        (true, Some(n)) => format!("{n} free"),
        (true, None) => "up, pool unknown".to_string(),
        (false, _) => "unknown".to_string(),
    };
    let desktops = crate::desktops::list_desktops();
    let held: Vec<String> = leases.iter().map(|l| l.desktop.clone()).collect();

    let mut queue_rows = String::new();
    if queued.is_empty() {
        queue_rows.push_str("<tr><td colspan=\"6\">Queue empty</td></tr>");
    }
    for q in &queued {
        queue_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&q.agent_id),
            esc(&q.job_id),
            esc(&q.resource_class),
            q.priority_class,
            q.desktops,
            q.duration_min
        ));
    }
    let mut lease_rows = String::new();
    if leases.is_empty() {
        lease_rows.push_str("<tr><td colspan=\"6\">No active leases</td></tr>");
    }
    for l in &leases {
        lease_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&l.lease_id),
            esc(&l.agent_id),
            esc(&l.desktop),
            esc(&l.state),
            est_label(l.start_unix),
            est_label(l.end_unix)
        ));
    }
    let mut desk_cells = String::new();
    for d in &desktops {
        let leased = held.contains(&d.id);
        let state = if d.id == "main-display" {
            "main :1"
        } else if leased {
            "leased"
        } else if d.up {
            "up"
        } else {
            "down"
        };
        desk_cells.push_str(&format!(
            "<div class=\"desk\"><b>{}</b><br>{}<br>{}</div>",
            esc(&d.id),
            esc(&d.display),
            state
        ));
    }
    let alerts = crate::notify::snapshot(store, inv, cfg);
    let mut alert_items = String::new();
    if let Some(list) = alerts.get("alerts").and_then(|a| a.as_array()) {
        if list.is_empty() {
            alert_items.push_str("<li>No alerts</li>");
        }
        for a in list {
            alert_items.push_str(&format!("<li>{}</li>", esc(a.as_str().unwrap_or(""))));
        }
    }
    let refresh = cfg.dashboard_refresh_sec;
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta http-equiv=\"refresh\" content=\"{refresh}\">\
        <title>MO - Local Resource Assistant</title>\
        <style>body{{font-family:sans-serif;margin:24px;color:#222}}h1{{font-size:28px}}table{{border-collapse:collapse;margin:8px 0}}td,th{{border:1px solid #ccc;padding:4px 8px;text-align:left}}th{{background:#eee}}.bar{{width:200px;height:16px;border:1px solid #999}}.bar>i{{display:block;height:16px;background:#0E7C7B}}.grid{{display:flex;flex-wrap:wrap;gap:8px}}.desk{{border:1px solid #ccc;padding:8px;min-width:90px;text-align:center}}.face{{float:right;text-align:center}}</style>\
        </head><body>\
        <div class=\"face\">{}<br>MO is {}</div>\
        <h1>MO - Local Resource Assistant</h1>\
        <p>Status: daemon up. Queue: {}. Active leases: {}. Updated: {}.</p>\
        <h2>Battery</h2>\
        <p>{} - {}</p>\
        <div class=\"bar\"><i style=\"width:{}%\"></i></div>\
        <h2>Queue</h2>\
        <table><tr><th>Agent</th><th>Job</th><th>Class</th><th>Priority</th><th>Desktops</th><th>Minutes</th></tr>{}</table>\
        <h2>Leases</h2>\
        <table><tr><th>Lease</th><th>Agent</th><th>Desktop</th><th>State</th><th>Start EST</th><th>End EST</th></tr>{}</table>\
        <h2>Desktops</h2><div class=\"grid\">{}</div>\
        <h2>Resources</h2>\
        <p>Load: {:.2}. RAM available: {} MB of {} MB. Disk free: {:.1} GB. Chrome processes: {}. Bridge pool: {}.</p>\
        <h2>Alerts</h2><ul>{}</ul>\
        <p><small>MO dashboard. Static file, read only. Refreshes every {} seconds.</small></p>\
        </body></html>",
        mo_svg(face),
        face_name,
        queued.len(),
        leases.len(),
        est_label(now_unix()),
        battery_label,
        battery_state.as_str(),
        battery_pct,
        queue_rows,
        lease_rows,
        desk_cells,
        inv.load1,
        inv.mem_available_mb,
        inv.mem_total_mb,
        inv.disk_free_gb,
        inv.chrome_count,
        bridge_label,
        alert_items,
        refresh
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::now_unix;

    fn tmp_store() -> (Store, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = format!("/tmp/mo-dash-{nanos}.db");
        (Store::open(&p).unwrap(), p)
    }

    #[test]
    fn svg_under_2kb_and_teal() {
        for face in [MoFace::Idle, MoFace::Busy, MoFace::Warn, MoFace::Refuse] {
            let svg = mo_svg(face);
            assert!(svg.len() < 2048, "svg {} bytes", svg.len());
            assert!(svg.contains("<svg"));
            assert!(!svg.contains("http"));
        }
        assert!(mo_svg(MoFace::Idle).contains("#0E7C7B"));
        assert!(mo_svg(MoFace::Warn).contains("#B26A00"));
        assert!(mo_svg(MoFace::Refuse).contains("#B3261E"));
        // Idle smiles, refuse frowns: different mouth paths.
        assert_ne!(mo_svg(MoFace::Idle), mo_svg(MoFace::Refuse));
    }

    #[test]
    fn face_selection() {
        assert_eq!(face_for(BatteryState::Refuse, 0), MoFace::Refuse);
        assert_eq!(face_for(BatteryState::Warn, 0), MoFace::Warn);
        assert_eq!(face_for(BatteryState::Unknown, 0), MoFace::Warn);
        assert_eq!(face_for(BatteryState::Normal, 1), MoFace::Busy);
        assert_eq!(face_for(BatteryState::Normal, 0), MoFace::Idle);
        assert_eq!(face_for(BatteryState::Recovery, 0), MoFace::Idle);
    }

    #[test]
    fn render_is_static_html() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let inv = crate::inventory::read_inventory_fresh();
        let html = render(&s, &inv, &cfg);
        assert!(html.contains("<meta http-equiv=\"refresh\" content=\"30\">"));
        assert!(html.contains("MO - Local Resource Assistant"));
        assert!(html.contains("EST"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("<form"));
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("fetch("));
        assert!(html.contains("#0E7C7B"));
        drop(s);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn render_shows_queue_and_leases() {
        let (s, p) = tmp_store();
        let cfg = Config::from_env();
        let now = now_unix();
        let req = crate::core::QueuedRequest {
            id: 0,
            agent_id: "scout".to_string(),
            job_id: "j1".to_string(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            resource_class: "vd-chrome".to_string(),
            cpu_cores: 1,
            mem_mb: 512,
            desktops: 1,
            duration_min: 30,
            priority_class: "P1".to_string(),
            deadline: None,
            needs_bridge: false,
            needs_display: false,
            created_unix: now,
            status: "queued".to_string(),
        };
        s.insert_request(&req).unwrap();
        let inv = crate::inventory::read_inventory_fresh();
        let html = render(&s, &inv, &cfg);
        assert!(html.contains("scout"));
        assert!(html.contains("j1"));
        assert!(html.contains("vd1"));
        drop(s);
        let _ = std::fs::remove_file(p);
    }
}
