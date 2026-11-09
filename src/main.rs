mod bridge;
mod compat;
mod mcp;
mod prefs;
mod procs;
mod safety;
mod settings;
mod shell;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "io.github.noizecrusher_gif.Bishop";

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if matches!(args.get(1).map(String::as_str), Some("--version" | "-V")) {
        println!("bishop {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }

    // Claude Code launches this binary as its MCP server: `bishop mcp-bridge <socket>`.
    if args.get(1).map(String::as_str) == Some("mcp-bridge") {
        return match args.get(2) {
            Some(socket) => bridge::run(socket),
            None => {
                eprintln!("usage: bishop mcp-bridge <socket>");
                glib::ExitCode::FAILURE
            }
        };
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(startup);
    app.connect_activate(|app| window::new(app).present());
    app.run_with_args(&args[..1])
}

fn startup(app: &adw::Application) {
    settings::load();
    if let Err(e) = shell::install_hooks() {
        eprintln!("bishop: could not install shell integration: {e}");
    }

    // Development builds run from the source tree: load icons from ./data/icons. Installed
    // builds use the system icon theme.
    #[cfg(debug_assertions)]
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
    gtk::Window::set_default_icon_name(APP_ID);
    load_css();

    app.add_action_entries([
        gio::ActionEntry::builder("new-window")
            .activate(|app: &adw::Application, _, _| window::new(app).present())
            .build(),
        gio::ActionEntry::builder("about")
            .activate(|app: &adw::Application, _, _| show_about(app))
            .build(),
    ]);

    app.set_accels_for_action("app.new-window", &["<Ctrl><Shift>n"]);
    app.set_accels_for_action("win.fullscreen", &["F11"]);
    app.set_accels_for_action("win.zoom-in", &["<Ctrl>plus", "<Ctrl>equal"]);
    app.set_accels_for_action("win.zoom-out", &["<Ctrl>minus"]);
    app.set_accels_for_action("win.zoom-reset", &["<Ctrl>0"]);
    app.set_accels_for_action("win.copy", &["<Ctrl><Shift>c"]);
    app.set_accels_for_action("win.paste", &["<Ctrl><Shift>v"]);
    app.set_accels_for_action("win.agent-stop", &["<Ctrl><Shift>Escape"]);
    app.set_accels_for_action("win.new-tab", &["<Ctrl><Shift>t"]);
    app.set_accels_for_action("win.close-tab", &["<Ctrl><Shift>w"]);
    app.set_accels_for_action("win.next-tab", &["<Ctrl>Page_Down"]);
    app.set_accels_for_action("win.previous-tab", &["<Ctrl>Page_Up"]);
}

/// With background opacity below 100%, windows get `.translucent`: the window and the
/// terminal widgets' theme background (libadwaita paints `vte-terminal` with the view
/// color, underneath VTE's own background) become transparent so VTE's translucent
/// background shows the desktop. The header bar, tab bar and `.solid` controls stay opaque.
fn load_css() {
    let Some(display) = gtk::gdk::Display::default() else { return };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "window.translucent, window.translucent vte-terminal { background-color: transparent; }
         window.translucent headerbar,
         window.translucent toolbarview > .top-bar { background-color: @headerbar_bg_color; }
         window.translucent .solid,
         window.translucent paned > separator { background-color: @window_bg_color; }
         .bishop-tabbar { background-color: @headerbar_bg_color; padding: 0 6px 6px 6px; }
         .bishop-tab { border-radius: 8px; padding: 2px 2px 2px 10px; margin: 0 2px; min-height: 30px; }
         .bishop-tab:hover { background-color: alpha(currentColor, 0.07); }
         .bishop-tab.selected { background-color: alpha(currentColor, 0.15); }
         .bishop-tab button { min-width: 24px; min-height: 24px; padding: 0; }",
    );
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
}

fn show_about(app: &adw::Application) {
    let about = adw::AboutDialog::builder()
        .application_name("Bishop")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A terminal with a built-in AI assistant that sees what you see and types where you type.")
        .build();
    about.present(app.active_window().as_ref());
}
