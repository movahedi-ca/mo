// mo-dashboard: standalone fullscreen viewer for the MO dashboard.
// One borderless fullscreen window. Loads the live dashboard file.
// No browser UI. No tabs. No address bar. No Chrome.
use gtk::prelude::*;
use webkit2gtk::{WebContext, WebContextExt, WebView, WebViewExt};

const DASHBOARD_URL: &str = "file:///home/auto/jobs/mo/dashboard.html";

fn main() {
    gtk::init().expect("gtk init failed");

    // Sandbox off: the dashboard is a local file under /home/auto/jobs/mo.
    // WebKitGTK's default sandbox denies that path. The file is written
    // by mo-daemon on this host only. No remote content is ever loaded.
    // Note: webkit2gtk 2.0.2 has no usable WebContext::new(); the
    // inherent default() returns the shared default context.
    let context = WebContext::default().expect("web context failed");
    context.set_sandbox_enabled(false);

    let webview = WebView::with_context(&context);
    webview.load_uri(DASHBOARD_URL);

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("MO Dashboard");
    window.set_decorated(false);
    window.set_default_size(1920, 1080);
    window.add(&webview);
    window.fullscreen();

    window.connect_destroy(|_| {
        gtk::main_quit();
    });

    window.show_all();
    gtk::main();
}
