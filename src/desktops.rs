//! Desktops: minimal list for TASK-004. Start/stop/fold land in TASK-010.

use crate::core::{Desktop, MoError, Result};

/// Static desktop list: main display plus vd1-vd10. Up status lands in TASK-010.
pub fn list_desktops() -> Vec<Desktop> {
    let mut out = vec![Desktop {
        id: "main-display".to_string(),
        display: ":1".to_string(),
        cdp_port: Some(9222),
        vnc_port: Some(5901),
        profile: None,
        up: true,
        leased: false,
    }];
    for n in 1..=10 {
        out.push(Desktop {
            id: format!("vd{n}"),
            display: format!(":{}", n + 1),
            cdp_port: Some(9332 + n),
            vnc_port: Some(5902 + n),
            profile: Some(format!("/home/auto/.config/chrome-vd{n}")),
            up: false,
            leased: false,
        });
    }
    out
}

/// Start a desktop. Real start lands in TASK-010.
pub fn start_desktop(_id: &str) -> Result<Desktop> {
    Err(MoError::e10("desktop start lands in TASK-010"))
}

/// Stop a desktop. Real stop lands in TASK-010.
pub fn stop_desktop(_id: &str) -> Result<()> {
    Err(MoError::e10("desktop stop lands in TASK-010"))
}
