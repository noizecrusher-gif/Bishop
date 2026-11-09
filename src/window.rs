//! A Bishop window: the header bar, the tab bar, and one `Session` per tab. Each session is
//! a terminal with its own agent panel, Claude Code process, and MCP socket; all agents run
//! in the same workspace, so they share CLAUDE.md, personal instructions, and memory.

mod agent;
mod stop;
mod tabs;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use serde_json::json;
use vte4::prelude::*;

use crate::mcp::{self, ToolRequest};
use crate::prefs;
use crate::settings::{self, Settings};
use crate::shell;

const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;
const BANNER_HOLD: Duration = Duration::from_millis(1500);
/// In full screen, the header bar slides in when the pointer is this close to the top edge
/// and hides again once it moves below `REVEAL_HIDE_Y`.
const REVEAL_SHOW_Y: f64 = 2.0;
const REVEAL_HIDE_Y: f64 = 96.0;
/// Three Emergency Stop clicks within this window make a Hard Stop.
const HARD_STOP_CLICKS: Duration = Duration::from_millis(1500);
/// After a tab closes, Claude Code gets this long to exit on SIGHUP before it is killed.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(3);

/// Claude Code never prompts for Bishop's tools: Bishop gates `send_keys` itself, so
/// automatic mode can be switched while Claude is running.
const ALLOWED_TOOLS: &str = "mcp__bishop__*";

/// Claude Code reads only the user's own settings, never project settings in its workspace
/// (which the agent could write with commands it types), and runs no hooks: a hook would run
/// commands outside Bishop's approvals, audit log and Emergency Stop.
const SETTING_SOURCES: &str = "user";
const LOCKED_SETTINGS: &str = r#"{"disableAllHooks":true}"#;

const AGENT_PROMPT: &str = "You are the assistant built into Bishop, a Linux terminal emulator. \
When the user mentions their terminal, screen, shell, or session, they mean the Bishop terminal \
session you reach through the `bishop` MCP tools: the terminal in your own Bishop tab. Follow the \
Bishop agent guide in CLAUDE.md.";

/// Written to the agent workspace as CLAUDE.md so Claude Code loads it as memory.
const AGENT_GUIDE: &str = include_str!("agent_guide.md");

static NEXT_WINDOW_ID: AtomicU32 = AtomicU32::new(1);

/// One window: the header bar and tab bar act on whichever tab is showing.
struct Host {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    agent_button: gtk::ToggleButton,
    stop_button: gtk::Button,
    zoom_label: gtk::Label,
    toolbar: adw::ToolbarView,
    stack: gtk::Stack,
    tab_bar: Rc<tabs::TabBar>,
    sessions: RefCell<Vec<Rc<Session>>>,
    zoom: Cell<f64>,
    /// Set while the header controls are updated to match the current tab.
    syncing: Cell<bool>,
    id: String,
    next_tab: Cell<u32>,
}

/// One tab: a terminal, its agent panel, and everything the agent needs to drive it.
struct Session {
    host: Weak<Host>,
    window: adw::ApplicationWindow,
    /// The tab's content in the window's stack.
    page: gtk::Widget,
    tab: tabs::Tab,
    title: RefCell<String>,
    terminal: vte4::Terminal,
    agent_terminal: vte4::Terminal,
    agent_pane: gtk::Box,
    agent_running: Cell<bool>,
    agent_pid: Cell<Option<glib::Pid>>,
    restart_agent: Cell<bool>,
    auto_switch: gtk::Switch,
    auto_mode: Cell<bool>,
    /// Set while Bishop itself flips the switch, to skip the usual toast.
    quiet_auto_toggle: Cell<bool>,
    approval: ApprovalBar,
    pending_approval: RefCell<Option<async_channel::Sender<bool>>>,
    /// The full input and warning of the pending approval, for See More.
    approval_details: RefCell<Option<(String, Option<String>)>>,
    approval_dialog: RefCell<Option<adw::Window>>,
    /// In-flight agent operations (typing, or waiting on a command).
    agent_busy: Cell<u32>,
    last_agent_input: Cell<Option<Instant>>,
    /// Bumped when the user takes over; waits started under an older value stop.
    cancel_generation: Cell<u64>,
    shell_pid: Cell<Option<glib::Pid>>,
    /// The shell has sent VTE shell-integration events (preexec/precmd).
    integration_seen: Cell<bool>,
    shell_waiters: RefCell<Vec<async_channel::Sender<agent::ShellEvent>>>,
    /// Where the shell's integration hook reports finished commands.
    shell_events: RefCell<Option<shell::Events>>,
    last_output: Cell<Instant>,
    /// The agent has started in this tab at least once; Emergency Stop shows from then on.
    agent_started: Cell<bool>,
    /// Emergency Stop pressed: all agent input is refused until the user resumes.
    stopped: Cell<bool>,
    /// Hard Stop pressed (or the tab closed): Claude Code is killed and disconnected for the
    /// life of the tab.
    hard_stopped: Cell<bool>,
    /// An Emergency Stop is currently terminating the running command.
    terminating: Cell<bool>,
    stop_clicks: RefCell<Vec<Instant>>,
    /// When the agent started driving the terminal (it typed, and the user hasn't typed
    /// since), and earlier such spans, in `procs::now_ticks` units. Processes started inside
    /// them are the agent's, and Emergency Stop kills them even if they run in the background.
    agent_driving_since: Cell<Option<u64>>,
    agent_driving_spans: RefCell<Vec<(u64, u64)>>,
    floating_stop_button: gtk::Button,
    stopped_banner: adw::Banner,
    mcp_server: RefCell<Option<mcp::Server>>,
    paned: gtk::Paned,
    paned_positioned: Cell<bool>,
    banner: adw::Banner,
    banner_generation: Cell<u64>,
    toasts: adw::ToastOverlay,
    session_id: String,
    socket_path: PathBuf,
    mcp_config: PathBuf,
}

/// Bar shown above the user's terminal when Claude wants to type and needs approval.
struct ApprovalBar {
    revealer: gtk::Revealer,
    label: gtk::Label,
    warning: gtk::Label,
    /// Shown when the input or warning is too long for the bar.
    more: gtk::Button,
    approve: gtk::Button,
    deny: gtk::Button,
}

pub fn new(app: &adw::Application) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Bishop")
        .default_width(1100)
        .default_height(700)
        .build();

    let title = adw::WindowTitle::new("Bishop", "");
    let header = adw::HeaderBar::builder()
        .title_widget(&title)
        .decoration_layout(":minimize,maximize,close")
        .build();
    let zoom_label = gtk::Label::new(Some("100%"));
    let menu_popover = gtk::PopoverMenu::from_model(Some(&build_menu()));
    menu_popover.add_child(&zoom_row(&zoom_label), "zoom");
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .popover(&menu_popover)
        .primary(true)
        .build();
    let agent_button = gtk::ToggleButton::builder()
        .label("Agent")
        .tooltip_text("Show this tab's Claude Code panel")
        .build();
    let stop_button = emergency_stop_button();
    header.pack_end(&menu_button);
    header.pack_end(&agent_button);
    header.pack_start(&stop_button);

    let current = settings::get();
    let tab_bar = tabs::TabBar::new(current.behavior.tab_scroll_limit);
    let stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&tab_bar.widget);
    toolbar.set_content(Some(&stack));
    window.set_content(Some(&toolbar));

    let host = Rc::new(Host {
        window: window.clone(),
        title,
        agent_button: agent_button.clone(),
        stop_button: stop_button.clone(),
        zoom_label,
        toolbar: toolbar.clone(),
        stack,
        tab_bar: tab_bar.clone(),
        sessions: RefCell::new(Vec::new()),
        zoom: Cell::new(1.0),
        syncing: Cell::new(false),
        id: format!("{}-{}", std::process::id(), NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed)),
        next_tab: Cell::new(1),
    });
    host.add_actions();

    agent_button.connect_toggled(glib::clone!(
        #[weak]
        host,
        move |button| {
            if !host.syncing.get()
                && let Some(session) = host.current()
            {
                session.set_agent_visible(button.is_active());
            }
        }
    ));
    stop_button.connect_clicked(glib::clone!(
        #[weak]
        host,
        move |_| {
            if let Some(session) = host.current() {
                session.stop_clicked();
            }
        }
    ));
    tab_bar.new_button.connect_clicked(glib::clone!(
        #[weak]
        host,
        move |_| host.add_tab()
    ));
    let weak = Rc::downgrade(&host);
    settings::subscribe(move |settings| {
        weak.upgrade().inspect(|host| host.tab_bar.set_limit(settings.behavior.tab_scroll_limit)).is_some()
    });

    window.connect_fullscreened_notify(glib::clone!(
        #[weak]
        host,
        move |window| host.fullscreen_changed(window.is_fullscreen())
    ));
    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    motion.connect_motion(glib::clone!(
        #[weak]
        window,
        #[weak]
        toolbar,
        move |_, _, y| {
            if !window.is_fullscreen() {
                return;
            }
            if y <= REVEAL_SHOW_Y {
                toolbar.set_reveal_top_bars(true);
            } else if y > REVEAL_HIDE_Y {
                toolbar.set_reveal_top_bars(false);
            }
        }
    ));
    window.add_controller(motion);

    host.add_tab();
    // This handler owns the window state; every other closure holds a weak reference.
    window.connect_close_request(move |_| {
        host.close_all();
        glib::Propagation::Proceed
    });
    window
}

fn new_terminal() -> vte4::Terminal {
    vte4::Terminal::builder().hexpand(true).vexpand(true).build()
}

fn scrolled(terminal: &vte4::Terminal) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(terminal)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(320)
        .build()
}

fn agent_pane(agent_terminal: &vte4::Terminal, auto_switch: &gtk::Switch) -> gtk::Box {
    let mode_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    mode_row.append(
        &gtk::Label::builder()
            .label("Automatic mode")
            .xalign(0.0)
            .hexpand(true)
            .build(),
    );
    mode_row.append(auto_switch);
    // Solid including its margins and the separator, so no gap shows through when the
    // background is translucent.
    let header = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["solid"]).build();
    header.append(&mode_row);
    header.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let pane = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .visible(false)
        .build();
    pane.append(&header);
    pane.append(&scrolled(agent_terminal));
    pane
}

fn approval_bar() -> ApprovalBar {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(4)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .selectable(true)
        .css_classes(["monospace"])
        .build();
    let warning = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(2)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .visible(false)
        .css_classes(["error", "heading"])
        .build();
    let more = gtk::Button::builder()
        .label("See More")
        .tooltip_text("Review the whole input in a separate window")
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    let deny = gtk::Button::builder().label("Deny").valign(gtk::Align::Center).build();
    let approve = gtk::Button::builder()
        .label("Type It")
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    row.append(&gtk::Label::builder().label("Claude wants to type:").css_classes(["heading"]).build());
    row.append(&label);
    row.append(&more);
    row.append(&deny);
    row.append(&approve);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&warning);
    content.append(&row);

    let frame = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["solid"]).build();
    frame.append(&content);
    frame.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let revealer = gtk::Revealer::builder().child(&frame).build();
    ApprovalBar { revealer, label, warning, more, approve, deny }
}

fn emergency_stop_button() -> gtk::Button {
    gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("process-stop-symbolic")
                .label("Emergency Stop")
                .build(),
        )
        .tooltip_text(
            "Stop this tab's agent and terminate the command it's running (Ctrl+Shift+Esc).\n\
             Click three times quickly for a Hard Stop, which disconnects the agent from this tab.",
        )
        .css_classes(["destructive-action"])
        .visible(false)
        .build()
}

fn is_modifier(key: gdk::Key) -> bool {
    use gdk::Key;
    [
        Key::Shift_L, Key::Shift_R, Key::Control_L, Key::Control_R, Key::Alt_L, Key::Alt_R,
        Key::Super_L, Key::Super_R, Key::Meta_L, Key::Meta_R, Key::ISO_Level3_Shift, Key::Caps_Lock,
    ]
    .contains(&key)
}

/// Window shortcuts that pass through the terminal's key handler; they aren't typing.
fn is_shortcut(key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
    use gdk::{Key, ModifierType};
    modifiers.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK)
        || (modifiers.contains(ModifierType::CONTROL_MASK) && [Key::Page_Up, Key::Page_Down].contains(&key))
}

fn spawn(
    terminal: &vte4::Terminal,
    argv: &[&str],
    env: &[&str],
    cwd: &Path,
    on_started: impl FnOnce(glib::Pid) + 'static,
) {
    let program = argv[0].to_owned();
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        cwd.to_str(),
        argv,
        env,
        glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        None::<&gio::Cancellable>,
        glib::clone!(
            #[weak]
            terminal,
            move |result| match result {
                Ok(pid) => on_started(pid),
                Err(e) => {
                    terminal.feed(format!("bishop: failed to start {program}: {e}\r\n").as_bytes());
                }
            }
        ),
    );
}

fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let windows = gio::Menu::new();
    windows.append(Some("New Tab"), Some("win.new-tab"));
    windows.append(Some("New Window"), Some("app.new-window"));
    windows.append(Some("Full Screen"), Some("win.fullscreen"));
    menu.append_section(None, &windows);

    let zoom = gio::Menu::new();
    let zoom_item = gio::MenuItem::new(None, None);
    zoom_item.set_attribute_value("custom", Some(&"zoom".to_variant()));
    zoom.append_item(&zoom_item);
    menu.append_section(None, &zoom);

    let settings = gio::Menu::new();
    settings.append(Some("Preferences"), Some("win.preferences"));
    settings.append(Some("Agent Configuration"), Some("win.agent-config"));
    menu.append_section(None, &settings);

    let about = gio::Menu::new();
    about.append(Some("About Bishop"), Some("app.about"));
    menu.append_section(None, &about);

    menu
}

fn zoom_row(label: &gtk::Label) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    row.append(
        &gtk::Button::builder()
            .icon_name("zoom-out-symbolic")
            .action_name("win.zoom-out")
            .tooltip_text("Zoom Out")
            .css_classes(["flat", "circular"])
            .build(),
    );
    row.append(
        &gtk::Button::builder()
            .child(label)
            .action_name("win.zoom-reset")
            .tooltip_text("Reset Zoom")
            .hexpand(true)
            .css_classes(["flat"])
            .build(),
    );
    row.append(
        &gtk::Button::builder()
            .icon_name("zoom-in-symbolic")
            .action_name("win.zoom-in")
            .tooltip_text("Zoom In")
            .css_classes(["flat", "circular"])
            .build(),
    );
    row
}

impl Host {
    fn add_actions(self: &Rc<Self>) {
        let action = |name: &str, f: fn(&Rc<Host>)| {
            let host = Rc::downgrade(self);
            gio::ActionEntry::builder(name)
                .activate(move |_: &adw::ApplicationWindow, _, _| {
                    if let Some(host) = host.upgrade() {
                        f(&host);
                    }
                })
                .build()
        };
        // Actions on the tab that's showing.
        let on_tab = |name: &str, f: fn(&Rc<Session>)| {
            let host = Rc::downgrade(self);
            gio::ActionEntry::builder(name)
                .activate(move |_: &adw::ApplicationWindow, _, _| {
                    if let Some(session) = host.upgrade().and_then(|host| host.current()) {
                        f(&session);
                    }
                })
                .build()
        };
        self.window.add_action_entries([
            gio::ActionEntry::builder("fullscreen")
                .state(false.to_variant())
                .activate(|window: &adw::ApplicationWindow, _, _| {
                    if window.is_fullscreen() {
                        window.unfullscreen();
                    } else {
                        window.fullscreen();
                    }
                })
                .build(),
            action("new-tab", |h| h.add_tab()),
            on_tab("close-tab", |s| {
                if let Some(host) = s.host.upgrade() {
                    host.close_tab(s);
                }
            }),
            action("next-tab", |h| h.cycle(1)),
            action("previous-tab", |h| h.cycle(-1)),
            action("zoom-in", |h| h.set_zoom(h.zoom.get() + ZOOM_STEP)),
            action("zoom-out", |h| h.set_zoom(h.zoom.get() - ZOOM_STEP)),
            action("zoom-reset", |h| h.set_zoom(1.0)),
            on_tab("copy", |s| s.focused_terminal().copy_clipboard_format(vte4::Format::Text)),
            on_tab("paste", |s| s.focused_terminal().paste_clipboard()),
            on_tab("agent-stop", |s| s.emergency_stop()),
            on_tab("agent-hard-stop", |s| {
                s.emergency_stop();
                s.hard_stop();
            }),
            on_tab("agent-resume", |s| s.resume_agent()),
            action("preferences", |h| prefs::show_preferences(&h.window)),
            on_tab("agent-config", |s| {
                let session = Rc::downgrade(s);
                prefs::show_agent_config(&s.window, move || {
                    if let Some(session) = session.upgrade() {
                        session.restart_agent();
                    }
                });
            }),
        ]);
    }

    /// Opens a new tab at the right end of the tab bar and switches to it.
    fn add_tab(self: &Rc<Self>) {
        let number = self.next_tab.replace(self.next_tab.get() + 1);
        let session = Session::new(self, &format!("{}-{number}", self.id));
        self.stack.add_child(&session.page);
        self.tab_bar.add(&session.tab);
        self.sessions.borrow_mut().push(session.clone());
        self.select(&session);
    }

    /// The tab that's showing.
    fn current(&self) -> Option<Rc<Session>> {
        let page = self.stack.visible_child()?;
        self.sessions.borrow().iter().find(|s| s.page == page).cloned()
    }

    fn select(&self, session: &Rc<Session>) {
        self.stack.set_visible_child(&session.page);
        for other in self.sessions.borrow().iter() {
            other.tab.set_selected(Rc::ptr_eq(other, session));
        }
        self.tab_bar.reveal(&session.tab);
        self.sync_chrome();
        session.focus();
    }

    /// Switches `step` tabs to the right (or left, if negative), wrapping around.
    fn cycle(&self, step: isize) {
        let next = {
            let sessions = self.sessions.borrow();
            let Some(current) = self.current() else { return };
            let Some(index) = sessions.iter().position(|s| Rc::ptr_eq(s, &current)) else { return };
            let count = sessions.len() as isize;
            sessions[(index as isize + step).rem_euclid(count) as usize].clone()
        };
        self.select(&next);
    }

    /// Closes a tab and ends its shell and agent. Closing the last tab closes the window.
    fn close_tab(&self, session: &Rc<Session>) {
        let showing = self.stack.visible_child().as_ref() == Some(&session.page);
        let index = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(index) = sessions.iter().position(|s| Rc::ptr_eq(s, session)) else { return };
            sessions.remove(index);
            index
        };
        session.shutdown();
        self.tab_bar.remove(&session.tab);
        self.stack.remove(&session.page);
        // Show the tab that took its place, or the new last one.
        let next = {
            let sessions = self.sessions.borrow();
            sessions.get(index).or(sessions.last()).cloned()
        };
        match next {
            None => self.window.close(),
            Some(next) if showing => self.select(&next),
            Some(_) => self.sync_chrome(),
        }
    }

    fn close_all(&self) {
        for session in self.sessions.take() {
            session.shutdown();
        }
    }

    /// Makes the header bar reflect the tab that's showing.
    fn sync_chrome(&self) {
        let Some(session) = self.current() else { return };
        self.syncing.set(true);
        self.agent_button.set_active(session.agent_pane.is_visible());
        self.syncing.set(false);
        let hard_stopped = session.hard_stopped.get();
        self.agent_button.set_sensitive(!hard_stopped);
        self.agent_button.set_tooltip_text(Some(if hard_stopped {
            "Disconnected by Hard Stop. Open a new tab to use the agent."
        } else {
            "Show this tab's Claude Code panel"
        }));
        self.stop_button.set_visible(session.shows_stop());
        let title = session.title.borrow();
        self.title.set_subtitle(&title);
        self.window.set_title(Some(if title.is_empty() { "Bishop" } else { &title }));
    }

    fn fullscreen_changed(&self, fullscreen: bool) {
        self.toolbar.set_reveal_top_bars(!fullscreen);
        for session in self.sessions.borrow().iter() {
            session.update_floating_stop();
        }
        self.window.change_action_state("fullscreen", &fullscreen.to_variant());
        if fullscreen && let Some(session) = self.current() {
            session.toast("Press F11, or move the pointer to the top edge, to leave full screen");
        }
    }

    /// Zoom applies to every tab in the window.
    fn set_zoom(&self, scale: f64) {
        let scale = ((scale * 10.0).round() / 10.0).clamp(MIN_ZOOM, MAX_ZOOM);
        self.zoom.set(scale);
        for session in self.sessions.borrow().iter() {
            session.set_zoom(scale);
        }
        self.zoom_label.set_text(&format!("{:.0}%", scale * 100.0));
    }
}

impl Session {
    fn new(host: &Rc<Host>, id: &str) -> Rc<Self> {
        let terminal = new_terminal();
        let agent_terminal = new_terminal();
        let terminal_pane = scrolled(&terminal);
        let auto_switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("When on, Claude types into your terminal without asking first")
            .build();
        let agent_pane = agent_pane(&agent_terminal, &auto_switch);

        let banner = adw::Banner::new("Claude is typing in this terminal");
        let approval = approval_bar();
        let main_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let stopped_banner = adw::Banner::builder()
            .title("Emergency stop: the agent can't type until you resume it")
            .button_label("Resume Agent")
            .build();
        main_box.append(&stopped_banner);
        main_box.append(&banner);
        main_box.append(&approval.revealer);
        main_box.append(&terminal_pane);

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&main_box)
            .end_child(&agent_pane)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(true)
            .build();
        // In full screen the header bar is hidden, so Emergency Stop floats over the terminal.
        let floating_stop_button = emergency_stop_button();
        floating_stop_button.set_halign(gtk::Align::End);
        floating_stop_button.set_valign(gtk::Align::Start);
        floating_stop_button.set_margin_top(12);
        floating_stop_button.set_margin_end(12);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&paned));
        overlay.add_overlay(&floating_stop_button);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&overlay));

        let current = settings::get();
        auto_switch.set_active(current.agent.automatic_mode);
        terminal.set_font_scale(host.zoom.get());
        agent_terminal.set_font_scale(host.zoom.get());

        let dir = mcp::runtime_dir();
        let state = Rc::new(Session {
            host: Rc::downgrade(host),
            window: host.window.clone(),
            page: toasts.clone().upcast(),
            tab: tabs::Tab::new(),
            title: RefCell::new(String::new()),
            terminal: terminal.clone(),
            agent_terminal: agent_terminal.clone(),
            agent_pane,
            agent_running: Cell::new(false),
            agent_pid: Cell::new(None),
            restart_agent: Cell::new(false),
            auto_switch: auto_switch.clone(),
            auto_mode: Cell::new(current.agent.automatic_mode),
            quiet_auto_toggle: Cell::new(false),
            approval,
            pending_approval: RefCell::new(None),
            approval_details: RefCell::new(None),
            approval_dialog: RefCell::new(None),
            agent_busy: Cell::new(0),
            last_agent_input: Cell::new(None),
            cancel_generation: Cell::new(0),
            shell_pid: Cell::new(None),
            integration_seen: Cell::new(false),
            shell_waiters: RefCell::new(Vec::new()),
            shell_events: RefCell::new(None),
            last_output: Cell::new(Instant::now()),
            agent_started: Cell::new(false),
            stopped: Cell::new(false),
            hard_stopped: Cell::new(false),
            terminating: Cell::new(false),
            stop_clicks: RefCell::new(Vec::new()),
            agent_driving_since: Cell::new(None),
            agent_driving_spans: RefCell::new(Vec::new()),
            floating_stop_button: floating_stop_button.clone(),
            stopped_banner: stopped_banner.clone(),
            mcp_server: RefCell::new(None),
            paned,
            paned_positioned: Cell::new(false),
            banner,
            banner_generation: Cell::new(0),
            toasts,
            socket_path: dir.join(format!("{id}.sock")),
            mcp_config: dir.join(format!("{id}.mcp.json")),
            session_id: id.to_owned(),
        });

        state.start_mcp_server();
        state.apply_settings(&current);
        let weak = Rc::downgrade(&state);
        settings::subscribe(move |settings| {
            weak.upgrade().inspect(|state| state.apply_settings(settings)).is_some()
        });

        let argv = shell::user_shell(&current.behavior.shell);
        let weak = Rc::downgrade(&state);
        let events = shell::Events::listen(dir.join(format!("{id}.events")), move |event| {
            if let Some(state) = weak.upgrade() {
                state.hook_event(event);
            }
        });
        let (argv, env) = match &events {
            Ok(events) => shell::with_hooks(&argv, events.path()),
            Err(e) => {
                state.toast(&format!("Shell integration unavailable: {e}"));
                (argv, Vec::new())
            }
        };
        state.shell_events.replace(events.ok());
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let env: Vec<&str> = env.iter().map(String::as_str).collect();
        let weak = Rc::downgrade(&state);
        spawn(&terminal, &argv, &env, &glib::home_dir(), move |pid| {
            if let Some(state) = weak.upgrade() {
                state.shell_pid.set(Some(pid));
            }
        });
        state.connect_signals();
        state
    }

    fn connect_signals(self: &Rc<Self>) {
        let state = self;
        self.terminal.connect_contents_changed(glib::clone!(
            #[weak]
            state,
            move |_| state.last_output.set(Instant::now())
        ));
        // Any real keypress in the terminal while Claude is working hands control back to the user.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            state,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                if !is_modifier(key) && !is_shortcut(key, modifiers) {
                    state.user_typed();
                    state.take_over();
                }
                glib::Propagation::Proceed
            }
        ));
        self.terminal.add_controller(keys);
        // The shell exited: close its tab.
        self.terminal.connect_child_exited(glib::clone!(
            #[weak]
            state,
            move |_, _| {
                if let Some(host) = state.host.upgrade() {
                    host.close_tab(&state);
                }
            }
        ));
        // The title signal is deprecated in newer VTE (for termprops) but works in every version.
        #[allow(deprecated)]
        self.terminal.connect_window_title_changed(glib::clone!(
            #[weak]
            state,
            move |term| {
                #[allow(deprecated)]
                let text = term.window_title().unwrap_or_default();
                state.tab.set_title(&text);
                state.title.replace(text.into());
                state.chrome_changed();
            }
        ));

        self.agent_terminal.connect_child_exited(glib::clone!(
            #[weak]
            state,
            move |term, _| {
                state.agent_running.set(false);
                state.agent_pid.set(None);
                if state.hard_stopped.get() {
                    term.feed(b"\r\n[Hard stop: the agent is disconnected. Open a new tab to use it again.]\r\n");
                    return;
                }
                if state.restart_agent.replace(false) {
                    state.start_agent();
                    return;
                }
                term.feed(b"\r\n[Claude Code exited. Toggle the Agent panel to restart it.]\r\n");
                state.set_agent_visible(false);
            }
        ));
        self.floating_stop_button.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.stop_clicked()
        ));
        self.stopped_banner.connect_button_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.resume_agent()
        ));
        self.auto_switch.connect_active_notify(glib::clone!(
            #[weak]
            state,
            move |switch| state.set_auto_mode(switch.is_active())
        ));
        self.approval.approve.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.resolve_approval(true)
        ));
        self.approval.deny.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.resolve_approval(false)
        ));
        self.approval.more.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.show_approval_details()
        ));

        // Clicking a tab shows it; middle-clicking or its close button closes it.
        let click = gtk::GestureClick::builder().button(0).build();
        click.connect_released(glib::clone!(
            #[weak]
            state,
            move |gesture, _, _, _| {
                let Some(host) = state.host.upgrade() else { return };
                match gesture.current_button() {
                    gdk::BUTTON_PRIMARY => host.select(&state),
                    gdk::BUTTON_MIDDLE => host.close_tab(&state),
                    _ => {}
                }
            }
        ));
        self.tab.root.add_controller(click);
        self.tab.close.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| {
                if let Some(host) = state.host.upgrade() {
                    host.close_tab(&state);
                }
            }
        ));
    }

    fn start_mcp_server(self: &Rc<Self>) {
        let (tx, rx) = async_channel::unbounded::<ToolRequest>();
        match mcp::serve(&self.socket_path, tx) {
            Ok(server) => *self.mcp_server.borrow_mut() = Some(server),
            Err(e) => {
                self.toast(&format!("Agent bridge unavailable: {e}"));
                return;
            }
        }

        let exe = std::env::current_exe().unwrap_or_else(|_| "bishop".into());
        let config = json!({
            "mcpServers": {
                "bishop": {
                    "type": "stdio",
                    "command": exe,
                    "args": ["mcp-bridge", self.socket_path],
                },
            },
        });
        if let Err(e) = std::fs::write(&self.mcp_config, config.to_string()) {
            self.toast(&format!("Could not write agent config: {e}"));
        }

        let state = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(request) = rx.recv().await {
                let Some(state) = state.upgrade() else { break };
                // Run each call separately so reads aren't blocked behind a pending approval.
                glib::spawn_future_local(async move {
                    let result = state.handle_tool(&request.name, &request.args).await;
                    let _ = request.reply.send(result);
                });
            }
        });
    }

    fn set_agent_visible(self: &Rc<Self>, visible: bool) {
        self.agent_pane.set_visible(visible);
        if visible {
            if !self.paned_positioned.replace(true) {
                self.paned.set_position((self.paned.width() as f64 * 0.6) as i32);
            }
            self.start_agent();
            self.agent_terminal.grab_focus();
        } else {
            self.terminal.grab_focus();
        }
        self.chrome_changed();
    }

    fn start_agent(self: &Rc<Self>) {
        if self.agent_running.get() || self.hard_stopped.get() {
            return;
        }
        let agent = settings::get().agent;
        let Some(claude) = agent.claude_program() else {
            self.agent_terminal.feed(
                b"Claude Code (`claude`) was not found.\r\n\
                  Install it from https://claude.com/claude-code, or set its path in\r\n\
                  Menu > Agent Configuration, then reopen this panel.\r\n",
            );
            return;
        };
        let workspace = match agent_workspace() {
            Ok(dir) => dir,
            Err(e) => {
                self.toast(&format!("Could not prepare the agent workspace: {e}"));
                glib::home_dir()
            }
        };
        let claude = claude.to_string_lossy();
        let config = self.mcp_config.to_string_lossy();
        let mut argv = vec![
            &*claude,
            "--mcp-config",
            &config,
            "--strict-mcp-config",
            "--tools",
            "",
            "--allowedTools",
            ALLOWED_TOOLS,
            "--setting-sources",
            SETTING_SOURCES,
            "--settings",
            LOCKED_SETTINGS,
            "--append-system-prompt",
            AGENT_PROMPT,
        ];
        if !agent.model.is_empty() {
            argv.extend(["--model", &agent.model]);
        }
        self.agent_terminal.reset(true, true);
        self.agent_running.set(true);
        self.agent_started.set(true);
        self.update_floating_stop();
        let state = Rc::downgrade(self);
        spawn(&self.agent_terminal, &argv, &[], &workspace, move |pid| {
            if let Some(state) = state.upgrade() {
                state.agent_pid.set(Some(pid));
                if let Some(server) = state.mcp_server.borrow().as_ref() {
                    server.set_agent(pid.0);
                }
            }
        });
    }

    /// Restarts Claude in this tab's panel (showing the panel if hidden), so new agent
    /// settings take effect.
    fn restart_agent(self: &Rc<Self>) {
        if self.hard_stopped.get() {
            self.toast("The agent was disconnected by a Hard Stop. Open a new tab to use it again.");
            return;
        }
        match self.agent_pid.get() {
            Some(pid) if self.agent_running.get() => {
                // The child-exited handler starts the new session.
                self.restart_agent.set(true);
                unsafe { libc::kill(pid.0, libc::SIGHUP) };
            }
            _ => {
                self.agent_running.set(false);
                if self.agent_pane.is_visible() {
                    self.start_agent();
                } else {
                    self.set_agent_visible(true);
                }
            }
        }
        self.toast("Restarting the agent");
    }

    /// The tab is closing: cut the agent off for good, and end Claude Code and the shell.
    fn shutdown(&self) {
        self.hard_stopped.set(true);
        self.stopped.set(true);
        self.restart_agent.set(false);
        self.cancel_generation.set(self.cancel_generation.get() + 1);
        self.resolve_approval(false);
        self.close_approval_dialog();
        if let Some(server) = self.mcp_server.take() {
            server.disconnect();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.mcp_config);
        self.shell_events.take();
        // Claude Code leads its own process group: hang it up, then make sure.
        if let Some(pid) = self.agent_pid.take() {
            unsafe { libc::kill(-pid.0, libc::SIGHUP) };
            glib::timeout_add_local_once(AGENT_EXIT_GRACE, move || {
                if !crate::procs::in_group(pid.0).is_empty() {
                    unsafe { libc::kill(-pid.0, libc::SIGKILL) };
                }
            });
        }
        // As when a terminal window closes: the shell hangs up its jobs and exits.
        if let Some(pid) = self.shell_pid.get() {
            unsafe { libc::kill(pid.0, libc::SIGHUP) };
        }
    }

    fn apply_settings(&self, settings: &Settings) {
        let appearance = &settings.appearance;
        let colors = appearance.colors();
        let foreground = prefs::rgba(&colors.foreground);
        let background = prefs::rgba(&colors.background);
        let cursor = prefs::rgba(&colors.cursor);
        let selection = prefs::rgba(&colors.selection);
        let selection_text = prefs::rgba(&colors.selection_text);
        let palette: Vec<gdk::RGBA> = colors.palette.iter().map(|c| prefs::rgba(c)).collect();
        let palette: Vec<&gdk::RGBA> = palette.iter().collect();
        let opacity = appearance.background_opacity.min(100) as f32 / 100.0;
        let translucent = gdk::RGBA::new(background.red(), background.green(), background.blue(), opacity);
        // The window behind the terminals must be transparent for their background alpha to
        // show the desktop; the header bar and other controls keep a solid background.
        if opacity < 1.0 {
            self.window.add_css_class("translucent");
        } else {
            self.window.remove_css_class("translucent");
        }
        let font = pango::FontDescription::from_string(&format!(
            "{} {}",
            appearance.font_family, appearance.font_size
        ));
        let blink = if appearance.cursor_blink {
            vte4::CursorBlinkMode::On
        } else {
            vte4::CursorBlinkMode::Off
        };

        for terminal in [&self.terminal, &self.agent_terminal] {
            terminal.set_colors(Some(&foreground), Some(&background), &palette);
            // `set_colors` ignores alpha; only `set_color_background` applies it.
            terminal.set_color_background(&translucent);
            terminal.set_color_cursor(Some(&cursor));
            terminal.set_color_cursor_foreground(Some(&background));
            terminal.set_color_highlight(Some(&selection));
            terminal.set_color_highlight_foreground(Some(&selection_text));
            terminal.set_font(Some(&font));
            terminal.set_cursor_shape(appearance.cursor_shape.to_vte());
            terminal.set_cursor_blink_mode(blink);
            terminal.set_scrollback_lines(settings.behavior.scrollback_lines as i64);
            terminal.set_audible_bell(settings.behavior.audible_bell);
        }
        // Match the window chrome to the terminal: a light background gets light chrome.
        let luminance = 0.2126 * background.red() + 0.7152 * background.green() + 0.0722 * background.blue();
        adw::StyleManager::default().set_color_scheme(if luminance < 0.5 {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });
    }

    /// Emergency Stop shows once the agent has started in this tab, until a Hard Stop.
    fn shows_stop(&self) -> bool {
        self.agent_started.get() && !self.hard_stopped.get()
    }

    /// In full screen, Emergency Stop floats over the terminal so it's never hidden.
    fn update_floating_stop(&self) {
        self.floating_stop_button.set_visible(self.shows_stop() && self.window.is_fullscreen());
        self.chrome_changed();
    }

    /// Something shown in the tab bar or header bar changed.
    fn chrome_changed(&self) {
        self.refresh_tab();
        if let Some(host) = self.host.upgrade() {
            host.sync_chrome();
        }
    }

    /// Shows what the agent is doing on the tab, so it's visible from other tabs.
    fn refresh_tab(&self) {
        let activity = if self.stopped.get() || self.hard_stopped.get() {
            tabs::Activity::Stopped
        } else if self.pending_approval.borrow().is_some() {
            tabs::Activity::Waiting
        } else if self.agent_busy.get() > 0 {
            tabs::Activity::Busy
        } else {
            tabs::Activity::Idle
        };
        self.tab.set_activity(activity);
    }

    fn focus(&self) {
        if self.agent_pane.is_visible() && self.agent_terminal.has_focus() {
            return;
        }
        self.terminal.grab_focus();
    }

    fn set_zoom(&self, scale: f64) {
        self.terminal.set_font_scale(scale);
        self.agent_terminal.set_font_scale(scale);
    }

    fn focused_terminal(&self) -> &vte4::Terminal {
        if self.agent_terminal.has_focus() {
            &self.agent_terminal
        } else {
            &self.terminal
        }
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

pub fn agent_dir() -> PathBuf {
    glib::user_data_dir().join("bishop").join("agent")
}

/// `~/.local/share/bishop/agent`: every agent panel's working directory, holding the Bishop
/// guide as CLAUDE.md so Claude Code loads it as project memory. All tabs share it, and so
/// share their memory.
fn agent_workspace() -> std::io::Result<PathBuf> {
    let dir = agent_dir();
    crate::safety::private_dir(&dir)?;
    std::fs::write(dir.join("CLAUDE.md"), AGENT_GUIDE)?;
    Ok(dir)
}
