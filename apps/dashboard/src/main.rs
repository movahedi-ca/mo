// mo-dashboard: standalone fullscreen viewer for the MO dashboard.
// Native GTK, no WebKit, no browser of any kind.
// One borderless fullscreen window. On :0 it IS the physical display
// (native GTK window, no VNC, no Chrome frame). On :1 it is the same
// window on the VNC display (fallback copy). 1960s theme: warm cream,
// teal, burnt orange, brown, monospace type, retro MO face.
//
// On :0 the window hides itself while the kiosk view is a vd, so the
// kiosk Chrome showing that vd comes forward; it shows itself again
// when the view returns to main. The 3 second poll loop drives this
// from the kiosk-target file, so the back button on the vd view also
// brings it back. On :1 the window never hides.
//
// Live data: polls mo-daemon through its Unix socket every 3 seconds
// (one "live" call: status + queue + leases + desktops). No page reload.
// No network port. No HTTP. The socket stays local only.
//
// Clickable desktops: a tile click switches the :0 kiosk view to that
// desktop (kiosk-view.sh re-points the noVNC bridge). View switching
// only. It never stops, kills, or folds any desktop. It never touches
// a lease. The back control on the vd view (or the main-display tile)
// returns the kiosk view to :1.
//
// Display off: a manual button runs `xset -display :0 dpms force off`
// on the physical display (:0, eDP-1). :1 is TigerVNC and has no DPMS
// extension, so the :1 route fails. Off zeroes the DPMS timers first
// so the blank can never self-trigger; on restores DPMS disabled, the
// as-found state, so it never blanks on its own. It only blanks the
// display. It never stops the daemon, never kills Chrome, never stops
// or folds any desktop, never touches a lease. It never triggers on
// its own. Wake is by any mouse or key input. `mo-cli display on`
// turns it back on from anywhere.
//
// Usage: mo-dashboard            (dashboard window on :1)
//        mo-dashboard --back     (small back-to-main window, on $DISPLAY)

use gtk::prelude::*;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::Duration;

// 1960s palette.
const POLL_SECS: u32 = 3;
const SOCKET_PATH: &str = "jobs/mo/mo.sock";
const KIOSK_VIEW: &str = "/home/auto/jobs/kiosk-view.sh";
const APP_PATH: &str = "/home/auto/.local/bin/mo-dashboard";

const CSS: &str = "
window { background-color: #F7EEDC; }
* { font-family: 'DejaVu Sans Mono', 'Liberation Mono', monospace; color: #5C3A21; }
.title { font-size: 30px; font-weight: bold; letter-spacing: 2px; color: #5C3A21; }
.subtitle { font-size: 15px; letter-spacing: 2px; color: #0E7C7B; }
.panel { background-color: #FFF8E7; border: 2px solid #5C3A21; border-radius: 14px; padding: 10px; }
.panel-title { font-size: 17px; font-weight: bold; letter-spacing: 2px; color: #0E7C7B; }
.readout { font-size: 17px; }
.dim { color: #8A6D4B; font-size: 14px; }
.warn { color: #B3261E; font-weight: bold; }
button { background-image: none; background-color: #FFF8E7; border: 2px solid #5C3A21; border-radius: 12px; padding: 8px; }
button:hover { background-color: #F7EEDC; }
.big-button:hover { background-color: #A84E1A; color: #FFF8E7; }
.big-button:hover * { color: #FFF8E7; }
button:disabled { opacity: 0.45; }
.desk-up { background-image: none; background-color: #FFF8E7; }
.desk-leased { background-image: none; background-color: #D9A441; }
.desk-viewing { background-image: none; background-color: #0E7C7B; }
.desk-viewing * { color: #FFF8E7; }
.desk-down { background-image: none; background-color: #E8DCC4; }
.big-button { background-image: none; font-size: 19px; font-weight: bold; letter-spacing: 1px; background-color: #C65D21; color: #FFF8E7; border: 2px solid #5C3A21; border-radius: 14px; padding: 12px 18px; }
.big-button * { color: #FFF8E7; }
progressbar trough { background-color: #FFF8E7; border: 2px solid #5C3A21; border-radius: 8px; min-height: 18px; }
progressbar progress { background-color: #0E7C7B; border-radius: 6px; }
";

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/home/auto".to_string())
}

fn socket_path() -> String {
    format!("{}/{}", home(), SOCKET_PATH)
}

fn fmt_clock(unix: i64) -> String {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = unix as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--:--".to_string();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// One socket round trip: send one JSON line, read one JSON line.
fn socket_call(cmd: &str) -> Option<Value> {
    let mut stream = UnixStream::connect(socket_path()).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let msg = serde_json::json!({ "cmd": cmd });
    let mut line = serde_json::to_string(&msg).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut out = String::new();
    reader.read_line(&mut out).ok()?;
    serde_json::from_str(&out).ok()
}

/// DPMS state from `xset -display :0 q`. The physical display is :0.
/// No "Monitor is" line at all means DPMS is disabled, which also
/// means the screen is on. Only an explicit "Monitor is Off" is off.
fn dpms_on() -> bool {
    let out = Command::new("xset").args(["-display", ":0", "q"]).output();
    match out {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).contains("Monitor is Off"),
        Err(_) => true,
    }
}

fn display_off() {
    // Zero the DPMS timers first so the blank can never self-trigger.
    let _ = Command::new("xset")
        .args(["-display", ":0", "dpms", "0", "0", "0"])
        .output();
    let _ = Command::new("xset")
        .args(["-display", ":0", "dpms", "force", "off"])
        .output();
}

fn display_on() {
    let _ = Command::new("xset")
        .args(["-display", ":0", "dpms", "force", "on"])
        .output();
    // Restore the as-found state: DPMS disabled, no auto-blank.
    let _ = Command::new("xset").args(["-display", ":0", "-dpms"]).output();
}

/// Current kiosk view target port. 5902 is :1. 5902+K is vdK.
fn kiosk_target() -> i64 {
    std::fs::read_to_string(format!("{}/jobs/kiosk-target", home()))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(5902)
}

fn view_from_target(port: i64) -> String {
    if port == 5902 {
        "main-display".to_string()
    } else if (5903..=5912).contains(&port) {
        format!("vd{}", port - 5902)
    } else {
        "main-display".to_string()
    }
}

/// Face state from battery + leases, same rule as the daemon.
fn face_state(battery_state: &str, active_leases: usize) -> &'static str {
    match battery_state {
        "Refuse" => "refuse",
        "Warn" | "Restrict" | "Unknown" => "warn",
        _ => {
            if active_leases > 0 {
                "busy"
            } else {
                "idle"
            }
        }
    }
}

/// Retro MO face, drawn with Cairo. Teal head, orange antenna ball,
/// dot eyes, mouth by state. No image file, no web content.
fn draw_face(cr: &cairo::Context, face: &str) {
    let (r, g, b) = match face {
        "warn" => (0.70, 0.42, 0.0),
        "refuse" => (0.70, 0.15, 0.12),
        _ => (0.055, 0.49, 0.48),
    };
    // Antenna.
    cr.set_source_rgb(r, g, b);
    cr.set_line_width(4.0);
    cr.move_to(60.0, 8.0);
    cr.line_to(60.0, 26.0);
    let _ = cr.stroke();
    cr.set_source_rgb(0.78, 0.36, 0.13);
    cr.arc(60.0, 8.0, 6.0, 0.0, 2.0 * std::f64::consts::PI);
    let _ = cr.fill();
    // Rounded head.
    cr.set_source_rgb(r, g, b);
    rounded_rect(cr, 16.0, 26.0, 88.0, 74.0, 16.0);
    let _ = cr.fill();
    // Eyes.
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.arc(44.0, 56.0, 7.0, 0.0, 2.0 * std::f64::consts::PI);
    let _ = cr.fill();
    cr.arc(76.0, 56.0, 7.0, 0.0, 2.0 * std::f64::consts::PI);
    let _ = cr.fill();
    cr.set_source_rgb(0.36, 0.23, 0.13);
    cr.arc(44.0, 56.0, 2.5, 0.0, 2.0 * std::f64::consts::PI);
    let _ = cr.fill();
    cr.arc(76.0, 56.0, 2.5, 0.0, 2.0 * std::f64::consts::PI);
    let _ = cr.fill();
    // Mouth by state.
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_line_width(4.0);
    cr.set_line_cap(cairo::LineCap::Round);
    match face {
        "busy" | "warn" => {
            cr.move_to(46.0, 82.0);
            cr.line_to(74.0, 82.0);
            let _ = cr.stroke();
        }
        "refuse" => {
            cr.arc(60.0, 92.0, 12.0, std::f64::consts::PI * 1.15, std::f64::consts::PI * 1.85);
            let _ = cr.stroke();
        }
        _ => {
            cr.arc(60.0, 74.0, 12.0, std::f64::consts::PI * 0.15, std::f64::consts::PI * 0.85);
            let _ = cr.stroke();
        }
    }
    // M-O under the face.
    cr.set_source_rgb(0.36, 0.23, 0.13);
    cr.select_font_face("DejaVu Sans Mono", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(20.0);
    cr.move_to(38.0, 132.0);
    let _ = cr.show_text("M-O");
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
}

/// Run the kiosk view switch in a thread. Never blocks the UI.
fn switch_view(target: &str) {
    let target = target.to_string();
    std::thread::spawn(move || {
        let _ = Command::new(KIOSK_VIEW).arg(&target).output();
    });
}

/// The small back-to-main window, shown on the vd display itself.
fn run_back_window() {
    gtk::init().expect("gtk init failed");
    apply_css();
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("MO - back to main");
    window.set_decorated(false);
    window.set_keep_above(true);
    window.set_default_size(360, 100);
    window.move_((1920 - 360) / 2, 24);
    let button = gtk::Button::with_label("◀  BACK TO MAIN");
    button.style_context().add_class("big-button");
    button.connect_clicked(|_| {
        switch_view("main");
        gtk::main_quit();
    });
    window.add(&button);
    window.connect_destroy(|_| gtk::main_quit());
    window.show_all();
    gtk::main();
}

fn apply_css() {
    let css = gtk::CssProvider::new();
    let _ = css.load_from_data(CSS.as_bytes());
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

struct Widgets {
    window: gtk::Window,
    clock: gtk::Label,
    daemon: gtk::Label,
    battery_label: gtk::Label,
    battery_bar: gtk::ProgressBar,
    resources: gtk::Label,
    queue_box: gtk::Box,
    queue_count: gtk::Label,
    leases_box: gtk::Box,
    leases_count: gtk::Label,
    desk_buttons: Vec<(String, gtk::Button, gtk::Label)>,
    view_label: gtk::Label,
    display_state: gtk::Label,
    display_note: gtk::Label,
    display_button: gtk::Button,
    face_area: gtk::DrawingArea,
    face_state: std::rc::Rc<std::cell::Cell<&'static str>>,
    back_child: std::rc::Rc<std::cell::RefCell<Option<Child>>>,
    last_render: std::rc::Rc<std::cell::RefCell<String>>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--back") {
        run_back_window();
        return;
    }
    gtk::init().expect("gtk init failed");
    apply_css();

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("MO Dashboard");
    window.set_decorated(false);
    window.set_default_size(1920, 1080);
    window.fullscreen();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_start(18);
    root.set_margin_end(18);
    root.set_margin_top(12);
    root.set_margin_bottom(12);

    // Top bar: face, title, clock, daemon state.
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let face_area = gtk::DrawingArea::new();
    face_area.set_size_request(120, 150);
    let face_state = std::rc::Rc::new(std::cell::Cell::new("idle"));
    {
        let fs = face_state.clone();
        face_area.connect_draw(move |_, cr| {
            draw_face(cr, fs.get());
            gtk::glib::Propagation::Stop
        });
    }
    let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let title = gtk::Label::new(Some("MO — LOCAL RESOURCE ASSISTANT"));
    title.style_context().add_class("title");
    title.set_halign(gtk::Align::Start);
    let subtitle = gtk::Label::new(Some("ONE QUEUE · ONE LEASE AT A TIME · NO DRAMA"));
    subtitle.style_context().add_class("subtitle");
    subtitle.set_halign(gtk::Align::Start);
    let clock = gtk::Label::new(Some("UPDATED --:--:--"));
    clock.style_context().add_class("readout");
    clock.set_halign(gtk::Align::Start);
    let daemon = gtk::Label::new(Some("DAEMON: …"));
    daemon.style_context().add_class("readout");
    daemon.set_halign(gtk::Align::Start);
    title_box.pack_start(&title, false, false, 0);
    title_box.pack_start(&subtitle, false, false, 0);
    title_box.pack_start(&clock, false, false, 0);
    title_box.pack_start(&daemon, false, false, 0);
    top.pack_start(&face_area, false, false, 0);
    top.pack_start(&title_box, true, true, 0);
    root.pack_start(&top, false, false, 0);

    // Middle row: battery + resources.
    let mid = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let battery_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    battery_panel.style_context().add_class("panel");
    let battery_title = gtk::Label::new(Some("BATTERY"));
    battery_title.style_context().add_class("panel-title");
    battery_title.set_halign(gtk::Align::Start);
    let battery_label = gtk::Label::new(Some("—"));
    battery_label.style_context().add_class("readout");
    battery_label.set_halign(gtk::Align::Start);
    let battery_bar = gtk::ProgressBar::new();
    battery_bar.set_fraction(0.0);
    battery_bar.set_size_request(260, 20);
    battery_panel.pack_start(&battery_title, false, false, 0);
    battery_panel.pack_start(&battery_label, false, false, 0);
    battery_panel.pack_start(&battery_bar, false, false, 0);
    let res_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    res_panel.style_context().add_class("panel");
    let res_title = gtk::Label::new(Some("RESOURCES"));
    res_title.style_context().add_class("panel-title");
    res_title.set_halign(gtk::Align::Start);
    let resources = gtk::Label::new(Some("—"));
    resources.style_context().add_class("readout");
    resources.set_halign(gtk::Align::Start);
    res_panel.pack_start(&res_title, false, false, 0);
    res_panel.pack_start(&resources, false, false, 0);
    mid.pack_start(&battery_panel, false, false, 0);
    mid.pack_start(&res_panel, true, true, 0);
    root.pack_start(&mid, false, false, 0);

    // Queue + leases row.
    let ql = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let queue_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    queue_panel.style_context().add_class("panel");
    let queue_count = gtk::Label::new(Some("QUEUE (0)"));
    queue_count.style_context().add_class("panel-title");
    queue_count.set_halign(gtk::Align::Start);
    let queue_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    queue_panel.pack_start(&queue_count, false, false, 0);
    queue_panel.pack_start(&queue_box, false, false, 0);
    let leases_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    leases_panel.style_context().add_class("panel");
    let leases_count = gtk::Label::new(Some("LEASES (0)"));
    leases_count.style_context().add_class("panel-title");
    leases_count.set_halign(gtk::Align::Start);
    let leases_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    leases_panel.pack_start(&leases_count, false, false, 0);
    leases_panel.pack_start(&leases_box, false, false, 0);
    ql.pack_start(&queue_panel, true, true, 0);
    ql.pack_start(&leases_panel, true, true, 0);
    root.pack_start(&ql, false, false, 0);

    // Desktops grid.
    let desk_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    desk_panel.style_context().add_class("panel");
    let desk_title = gtk::Label::new(Some("DESKTOPS — CLICK A TILE TO VIEW IT"));
    desk_title.style_context().add_class("panel-title");
    desk_title.set_halign(gtk::Align::Start);
    let desk_grid = gtk::Grid::new();
    desk_grid.set_column_spacing(8);
    desk_grid.set_row_spacing(8);
    desk_panel.pack_start(&desk_title, false, false, 0);
    desk_panel.pack_start(&desk_grid, false, false, 0);
    root.pack_start(&desk_panel, false, false, 0);

    // Bottom bar: view + display control.
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    bottom.style_context().add_class("panel");
    let view_label = gtk::Label::new(Some("VIEWING: MAIN DISPLAY"));
    view_label.style_context().add_class("readout");
    view_label.set_halign(gtk::Align::Start);
    let display_state = gtk::Label::new(Some("DISPLAY: ON"));
    display_state.style_context().add_class("readout");
    let display_note = gtk::Label::new(Some(""));
    display_note.style_context().add_class("dim");
    let display_button = gtk::Button::with_label("DISPLAY OFF");
    display_button.style_context().add_class("big-button");
    display_button.connect_clicked(|_| {
        if dpms_on() {
            display_off();
        } else {
            display_on();
        }
    });
    bottom.pack_start(&view_label, true, true, 0);
    bottom.pack_start(&display_state, false, false, 0);
    bottom.pack_start(&display_note, false, false, 0);
    bottom.pack_start(&display_button, false, false, 0);
    root.pack_start(&bottom, false, false, 0);

    window.add(&root);

    // Desktop tiles: main-display + vd1..vd10.
    let mut desk_buttons: Vec<(String, gtk::Button, gtk::Label)> = Vec::new();
    let ids: Vec<String> = std::iter::once("main-display".to_string())
        .chain((1..=10).map(|k| format!("vd{k}")))
        .collect();
    for (i, id) in ids.iter().enumerate() {
        let label = gtk::Label::new(Some(&format!("{}\n:{}", id, "?")));
        let button = gtk::Button::new();
        button.add(&label);
        button.set_sensitive(false);
        let col = (i % 6) as i32;
        let row = (i / 6) as i32;
        desk_grid.attach(&button, col, row, 1, 1);
        desk_buttons.push((id.clone(), button, label));
    }
    let widgets = Widgets {
        window: window.clone(),
        clock,
        daemon,
        battery_label,
        battery_bar,
        resources,
        queue_box,
        queue_count,
        leases_box,
        leases_count,
        desk_buttons,
        view_label,
        display_state,
        display_note,
        display_button,
        face_area,
        face_state,
        back_child: std::rc::Rc::new(std::cell::RefCell::new(None)),
        last_render: std::rc::Rc::new(std::cell::RefCell::new(String::new())),
    };
    let widgets = std::rc::Rc::new(std::cell::RefCell::new(widgets));
    {
        let w = widgets.clone();
        for (id, button, _) in &widgets.borrow().desk_buttons {
            let id = id.clone();
            let w2 = w.clone();
            button.connect_clicked(move |_| {
                on_tile_clicked(&id, &w2);
            });
        }
    }

    window.connect_destroy(|_| gtk::main_quit());
    window.show_all();

    // First paint + 3 second live loop.
    refresh(&widgets);
    let w = widgets.clone();
    gtk::glib::timeout_add_seconds_local(POLL_SECS, move || {
        refresh(&w);
        gtk::glib::ControlFlow::Continue
    });
    gtk::main();
}

/// Tile click: view switching only. Never stops, kills, or folds.
fn on_tile_clicked(id: &str, w: &std::rc::Rc<std::cell::RefCell<Widgets>>) {
    let viewing = view_from_target(kiosk_target());
    if id == viewing {
        return;
    }
    if id == "main-display" {
        switch_view("main");
        kill_back_child(w);
    } else {
        switch_view(id);
        spawn_back_child(id, w);
    }
    // The poll loop picks the new target up within 3 seconds and
    // repaints the view label and the tile states.
}

fn spawn_back_child(id: &str, w: &std::rc::Rc<std::cell::RefCell<Widgets>>) {
    kill_back_child(w);
    let k: i64 = id[2..].parse().unwrap_or(1);
    let display = format!(":{}", k + 1);
    let child = Command::new(APP_PATH)
        .arg("--back")
        .env("DISPLAY", &display)
        .spawn()
        .ok();
    *w.borrow().back_child.borrow_mut() = child;
}

fn kill_back_child(w: &std::rc::Rc<std::cell::RefCell<Widgets>>) {
    if let Some(mut child) = w.borrow().back_child.borrow_mut().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// One poll: socket live call + DPMS state + repaint.
fn refresh(w: &std::rc::Rc<std::cell::RefCell<Widgets>>) {
    let live = socket_call("live");
    let viewing = view_from_target(kiosk_target());
    let on = dpms_on();
    let w = w.borrow_mut();

    // On :0 the dashboard is the physical display itself. While the
    // kiosk view is a vd, hide so the kiosk Chrome showing that vd
    // comes forward; when the view returns to main, show again. The
    // poll loop drives this, so the back button on the vd view also
    // brings it back. On :1 (the fallback copy) never hide.
    let on_zero = std::env::var("DISPLAY").map(|d| d == ":0").unwrap_or(false);
    if on_zero {
        if viewing == "main-display" {
            w.window.show_all();
            w.window.present();
        } else {
            w.window.hide();
        }
    }

    // Reap a back window that exited on its own (back button clicked).
    // One borrow at a time: the check borrow must drop before the
    // clear borrow, or the RefCell panics (hit 2026-11-06 17:52).
    let exited = w
        .back_child
        .borrow_mut()
        .as_mut()
        .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
    if exited {
        *w.back_child.borrow_mut() = None;
    }

    w.view_label.set_text(&format!(
        "VIEWING: {}",
        if viewing == "main-display" {
            "MAIN DISPLAY".to_string()
        } else {
            viewing.to_uppercase()
        }
    ));
    w.display_state
        .set_text(if on { "DISPLAY: ON" } else { "DISPLAY: OFF" });
    w.display_button
        .set_label(if on { "DISPLAY OFF" } else { "DISPLAY ON" });

    let Some(v) = live else {
        w.daemon.set_text("DAEMON: DOWN — SOCKET UNREACHABLE");
        w.daemon.style_context().add_class("warn");
        return;
    };
    w.daemon.style_context().remove_class("warn");
    let status = v.get("status").cloned().unwrap_or(Value::Null);
    let now = status.get("now_unix").and_then(|n| n.as_i64()).unwrap_or(0);
    w.clock.set_text(&format!("UPDATED {}", fmt_clock(now)));
    let daemon_up = status
        .get("daemon")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        == "up";
    let queue_depth = status
        .get("queue_depth")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let active = status
        .get("active_leases")
        .and_then(|n| n.as_u64())
        .unwrap_or(0) as usize;
    w.daemon.set_text(&format!(
        "DAEMON: {} · QUEUE {} · LEASES {}",
        if daemon_up { "UP" } else { "DOWN" },
        queue_depth,
        active
    ));
    let note = if active > 0 {
        format!("{active} LEASE(S) ACTIVE — DISPLAY OFF IS YOUR CALL")
    } else {
        String::new()
    };
    w.display_note.set_text(&note);

    let battery_pct = status
        .get("battery_pct")
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    let battery_state = status
        .get("battery_state")
        .and_then(|s| s.as_str())
        .unwrap_or("Unknown");
    w.battery_label
        .set_text(&format!("{}% — {}", battery_pct, battery_state));
    w.battery_bar
        .set_fraction((battery_pct as f64 / 100.0).clamp(0.0, 1.0));

    let load1 = status.get("load1").and_then(|n| n.as_f64()).unwrap_or(0.0);
    let mem_av = status
        .get("mem_available_mb")
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    let mem_tot = status
        .get("mem_total_mb")
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    let disk = status
        .get("disk_free_gb")
        .and_then(|n| n.as_f64())
        .unwrap_or(0.0);
    let chrome = status
        .get("chrome_count")
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    w.resources.set_text(&format!(
        "LOAD {:.2} · RAM {} / {} MB · DISK {:.1} GB FREE · CHROME {}",
        load1, mem_av, mem_tot, disk, chrome
    ));

    let face = face_state(battery_state, active);
    if w.face_state.get() != face {
        w.face_state.set(face);
        w.face_area.queue_draw();
    }

    // Queue + leases rows. Rebuild only when the text changed.
    let mut render = String::new();
    let queue = v
        .get("queue")
        .and_then(|q| q.as_array())
        .cloned()
        .unwrap_or_default();
    render.push_str(&format!("Q{}\n", queue.len()));
    for q in &queue {
        render.push_str(&format!(
            "{} {} {} {} {}m\n",
            q.get("agent_id").and_then(|s| s.as_str()).unwrap_or("?"),
            q.get("job_id").and_then(|s| s.as_str()).unwrap_or("?"),
            q.get("resource_class")
                .and_then(|s| s.as_str())
                .unwrap_or("?"),
            q.get("priority_class")
                .and_then(|s| s.as_str())
                .unwrap_or("?"),
            q.get("duration_min").and_then(|n| n.as_i64()).unwrap_or(0),
        ));
    }
    let leases = v
        .get("leases")
        .and_then(|q| q.as_array())
        .cloned()
        .unwrap_or_default();
    render.push_str(&format!("L{}\n", leases.len()));
    for l in &leases {
        render.push_str(&format!(
            "{} {} {} {}\n",
            l.get("lease_id").and_then(|s| s.as_str()).unwrap_or("?"),
            l.get("agent_id").and_then(|s| s.as_str()).unwrap_or("?"),
            l.get("desktop").and_then(|s| s.as_str()).unwrap_or("?"),
            l.get("state").and_then(|s| s.as_str()).unwrap_or("?"),
        ));
    }
    if *w.last_render.borrow() != render {
        *w.last_render.borrow_mut() = render;
        w.queue_count.set_text(&format!("QUEUE ({})", queue.len()));
        for child in w.queue_box.children() {
            w.queue_box.remove(&child);
        }
        if queue.is_empty() {
            let l = gtk::Label::new(Some("QUEUE EMPTY"));
            l.style_context().add_class("dim");
            l.set_halign(gtk::Align::Start);
            w.queue_box.pack_start(&l, false, false, 0);
        }
        for q in &queue {
            let l = gtk::Label::new(Some(&format!(
                "{} · {} · {} · {} · {}m",
                q.get("agent_id").and_then(|s| s.as_str()).unwrap_or("?"),
                q.get("job_id").and_then(|s| s.as_str()).unwrap_or("?"),
                q.get("resource_class")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?"),
                q.get("priority_class")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?"),
                q.get("duration_min").and_then(|n| n.as_i64()).unwrap_or(0),
            )));
            l.set_halign(gtk::Align::Start);
            w.queue_box.pack_start(&l, false, false, 0);
        }
        w.leases_count.set_text(&format!("LEASES ({})", leases.len()));
        for child in w.leases_box.children() {
            w.leases_box.remove(&child);
        }
        if leases.is_empty() {
            let l = gtk::Label::new(Some("NO ACTIVE LEASES"));
            l.style_context().add_class("dim");
            l.set_halign(gtk::Align::Start);
            w.leases_box.pack_start(&l, false, false, 0);
        }
        for l in &leases {
            let lab = gtk::Label::new(Some(&format!(
                "{} · {} · {} · {}",
                l.get("lease_id").and_then(|s| s.as_str()).unwrap_or("?"),
                l.get("agent_id").and_then(|s| s.as_str()).unwrap_or("?"),
                l.get("desktop").and_then(|s| s.as_str()).unwrap_or("?"),
                l.get("state").and_then(|s| s.as_str()).unwrap_or("?"),
            )));
            lab.set_halign(gtk::Align::Start);
            w.leases_box.pack_start(&lab, false, false, 0);
        }
        w.queue_box.show_all();
        w.leases_box.show_all();
    }

    // Desktop tiles.
    let desktops = v
        .get("desktops")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let leased: Vec<String> = leases
        .iter()
        .filter_map(|l| {
            l.get("desktop")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    for (id, button, label) in &w.desk_buttons {
        let found = desktops
            .iter()
            .find(|d| d.get("id").and_then(|s| s.as_str()).unwrap_or("") == id);
        let (display, up) = match found {
            Some(d) => (
                d.get("display")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
                    .to_string(),
                d.get("up").and_then(|b| b.as_bool()).unwrap_or(false),
            ),
            None => ("?".to_string(), false),
        };
        let state = if id == "main-display" {
            "MAIN :1"
        } else if leased.contains(id) {
            "LEASED"
        } else if up {
            "UP"
        } else {
            "DOWN"
        };
        label.set_text(&format!("{}\n{}\n{}", id, display, state));
        let ctx = button.style_context();
        for class in ["desk-up", "desk-leased", "desk-viewing", "desk-down"] {
            ctx.remove_class(class);
        }
        if *id == viewing {
            ctx.add_class("desk-viewing");
        } else if id == "main-display" || leased.contains(id) {
            ctx.add_class("desk-leased");
        } else if up {
            ctx.add_class("desk-up");
        } else {
            ctx.add_class("desk-down");
        }
        button.set_sensitive(*id == "main-display" || up);
    }
}
