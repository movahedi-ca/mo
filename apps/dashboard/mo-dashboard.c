/* mo-dashboard (C fallback): standalone fullscreen viewer for the MO dashboard.
 * One borderless fullscreen window. Loads the live dashboard file.
 * No browser UI. No tabs. No address bar. No Chrome.
 * Sandbox off: the dashboard is a local file under /home/auto/jobs/mo.
 * WebKitGTK's default sandbox denies that path. The file is written by
 * mo-daemon on this host only. No remote content is ever loaded.
 *
 * Build: gcc mo-dashboard.c -o mo-dashboard $(pkg-config --cflags --libs gtk+-3.0 webkit2gtk-4.1)
 */
#include <gtk/gtk.h>
#include <webkit2/webkit2.h>

#define DASHBOARD_URL "file:///home/auto/jobs/mo/dashboard.html"

int main(int argc, char *argv[]) {
    gtk_init(&argc, &argv);

    WebKitWebContext *context = webkit_web_context_get_default();
    webkit_web_context_set_sandbox_enabled(context, FALSE);

    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "MO Dashboard");
    gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
    gtk_window_set_default_size(GTK_WINDOW(window), 1920, 1080);
    gtk_window_fullscreen(GTK_WINDOW(window));
    g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);

    WebKitWebView *view = WEBKIT_WEB_VIEW(webkit_web_view_new_with_context(context));
    webkit_web_view_load_uri(view, DASHBOARD_URL);
    gtk_container_add(GTK_CONTAINER(window), GTK_WIDGET(view));

    gtk_widget_show_all(window);
    gtk_main();
    return 0;
}
