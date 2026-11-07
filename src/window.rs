mod agent;
mod stop;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use serde_json::json;
use vte4::prelude::*;

use crate::mcp::{self, ToolRequest};
use crate::prefs;
use crate::settings::{self, Settings};

const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;
const BANNER_HOLD: Duration = Duration::from_millis(1500);
/// In full screen, the header bar slides in when the pointer is this close to the top edge
/// and hides again once it moves below `REVEAL_HIDE_Y`.
const REVEAL_SHOW_Y: f64 = 2.0;
const REVEAL_HIDE_Y: f64 = 64.0;
/// Three Emergency Stop clicks within this window make a Hard Stop.
const HARD_STOP_CLICKS: Duration = Duration::from_millis(1500);

/// Claude Code never prompts for Bishop's tools: Bishop gates `send_keys` itself, so
/// automatic mode can be switched while Claude is running.
const ALLOWED_TOOLS: &str = "mcp__bishop__*";

const AGENT_PROMPT: &str = "You are the assistant built into Bishop, a Linux terminal emulator. \
When the user mentions their terminal, screen, shell, or session, they mean the Bishop terminal \
session you reach through the `bishop` MCP tools. Follow the Bishop agent guide in CLAUDE.md.";

/// Written to the agent workspace as CLAUDE.md so Claude Code loads it as memory.
const AGENT_GUIDE: &str = include_str!("agent_guide.md");

static NEXT_WINDOW_ID: AtomicU32 = AtomicU32::new(1);

struct Bishop {
    window: adw::ApplicationWindow,
    terminal: vte4::Terminal,
    agent_terminal: vte4::Terminal,
    agent_pane: gtk::Box,
    agent_button: gtk::ToggleButton,
    agent_running: Cell<bool>,
    agent_pid: Cell<Option<glib::Pid>>,
    restart_agent: Cell<bool>,
    auto_switch: gtk::Switch,
    auto_mode: Cell<bool>,
    /// Set while Bishop itself flips the switch, to skip the usual toast.
    quiet_auto_toggle: Cell<bool>,
    approval: ApprovalBar,
    pending_approval: RefCell<Option<async_channel::Sender<bool>>>,
    /// In-flight agent operations (typing, or waiting on a command).
    agent_busy: Cell<u32>,
    last_agent_input: Cell<Option<Instant>>,
    /// Bumped when the user takes over; waits started under an older value stop.
    cancel_generation: Cell<u64>,
    shell_pid: Cell<Option<glib::Pid>>,
    /// The shell has sent VTE shell-integration events (preexec/precmd).
    integration_seen: Cell<bool>,
    shell_waiters: RefCell<Vec<async_channel::Sender<agent::ShellEvent>>>,
    last_output: Cell<Instant>,
    /// The agent has started in this window at least once; Emergency Stop shows from then on.
    agent_started: Cell<bool>,
    /// Emergency Stop pressed: all agent input is refused until the user resumes.
    stopped: Cell<bool>,
    /// Hard Stop pressed: Claude Code is killed and disconnected for the life of the window.
    hard_stopped: Cell<bool>,
    /// An Emergency Stop is currently terminating the running command.
    terminating: Cell<bool>,
    stop_clicks: RefCell<Vec<Instant>>,
    /// When the agent started driving the terminal (it typed, and the user hasn't typed
    /// since), and earlier such spans, in `procs::now_ticks` units. Processes started inside
    /// them are the agent's, and Emergency Stop kills them even if they run in the background.
    agent_driving_since: Cell<Option<u64>>,
    agent_driving_spans: RefCell<Vec<(u64, u64)>>,
    stop_button: gtk::Button,
    floating_stop_button: gtk::Button,
    stopped_banner: adw::Banner,
    mcp_server: RefCell<Option<mcp::Server>>,
    paned: gtk::Paned,
    paned_positioned: Cell<bool>,
    banner: adw::Banner,
    banner_generation: Cell<u64>,
    zoom_label: gtk::Label,
    toasts: adw::ToastOverlay,
    toolbar: adw::ToolbarView,
    window_id: String,
    socket_path: PathBuf,
    mcp_config: PathBuf,
}

/// Bar shown above the user's terminal when Claude wants to type and needs approval.
struct ApprovalBar {
    revealer: gtk::Revealer,
    label: gtk::Label,
    warning: gtk::Label,
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
    let stop_button = emergency_stop_button();
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
        .tooltip_text("Show the Claude Code panel")
        .build();
    header.pack_end(&menu_button);
    header.pack_end(&agent_button);
    header.pack_start(&stop_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let current = settings::get();
    auto_switch.set_active(current.agent.automatic_mode);

    let id = format!("{}-{}", std::process::id(), NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed));
    let dir = mcp::runtime_dir();
    let state = Rc::new(Bishop {
        window: window.clone(),
        terminal: terminal.clone(),
        agent_terminal: agent_terminal.clone(),
        agent_pane,
        agent_button: agent_button.clone(),
        agent_running: Cell::new(false),
        agent_pid: Cell::new(None),
        restart_agent: Cell::new(false),
        auto_switch: auto_switch.clone(),
        auto_mode: Cell::new(current.agent.automatic_mode),
        quiet_auto_toggle: Cell::new(false),
        approval,
        pending_approval: RefCell::new(None),
        agent_busy: Cell::new(0),
        last_agent_input: Cell::new(None),
        cancel_generation: Cell::new(0),
        shell_pid: Cell::new(None),
        integration_seen: Cell::new(false),
        shell_waiters: RefCell::new(Vec::new()),
        last_output: Cell::new(Instant::now()),
        agent_started: Cell::new(false),
        stopped: Cell::new(false),
        hard_stopped: Cell::new(false),
        terminating: Cell::new(false),
        stop_clicks: RefCell::new(Vec::new()),
        agent_driving_since: Cell::new(None),
        agent_driving_spans: RefCell::new(Vec::new()),
        stop_button: stop_button.clone(),
        floating_stop_button: floating_stop_button.clone(),
        stopped_banner: stopped_banner.clone(),
        mcp_server: RefCell::new(None),
        paned,
        paned_positioned: Cell::new(false),
        banner,
        banner_generation: Cell::new(0),
        zoom_label,
        toasts,
        toolbar: toolbar.clone(),
        socket_path: dir.join(format!("{id}.sock")),
        mcp_config: dir.join(format!("{id}.mcp.json")),
        window_id: id,
    });

    state.add_actions();
    state.start_mcp_server();
    state.apply_settings(&current);
    let weak = Rc::downgrade(&state);
    settings::subscribe(move |settings| {
        weak.upgrade().inspect(|state| state.apply_settings(settings)).is_some()
    });

    let shell = Some(current.behavior.shell)
        .filter(|shell| !shell.is_empty())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/bash".into());
    let argv = shell_words(&shell);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let weak = Rc::downgrade(&state);
    spawn(&terminal, &argv, &glib::home_dir(), move |pid| {
        if let Some(state) = weak.upgrade() {
            state.shell_pid.set(Some(pid));
        }
    });
    for prop in ["vte.shell.preexec", "vte.shell.precmd"] {
        terminal.connect_termprop_changed(
            Some(prop),
            glib::clone!(
                #[weak]
                state,
                move |_, prop| state.shell_event(prop)
            ),
        );
    }
    terminal.connect_contents_changed(glib::clone!(
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
            let shortcut = modifiers.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
            if !is_modifier(key) && !shortcut {
                state.user_typed();
                state.take_over();
            }
            glib::Propagation::Proceed
        }
    ));
    terminal.add_controller(keys);
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        window,
        move |_, _| window.close()
    ));
    terminal.connect_termprop_changed(
        Some("xterm.title"),
        glib::clone!(
            #[weak]
            window,
            #[weak]
            title,
            move |term, _| {
                let text = term.termprop_string("xterm.title").0.unwrap_or_default();
                title.set_subtitle(&text);
                window.set_title(Some(if text.is_empty() { "Bishop" } else { &text }));
            }
        ),
    );

    agent_terminal.connect_child_exited(glib::clone!(
        #[weak]
        state,
        move |term, _| {
            state.agent_running.set(false);
            state.agent_pid.set(None);
            if state.hard_stopped.get() {
                term.feed(b"\r\n[Hard stop: the agent is disconnected. Open a new Bishop window to use it again.]\r\n");
                return;
            }
            if state.restart_agent.replace(false) {
                state.start_agent();
                return;
            }
            term.feed(b"\r\n[Claude Code exited. Toggle the Agent panel to restart it.]\r\n");
            state.agent_button.set_active(false);
        }
    ));
    for button in [&stop_button, &floating_stop_button] {
        button.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.stop_clicked()
        ));
    }
    stopped_banner.connect_button_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.resume_agent()
    ));
    auto_switch.connect_active_notify(glib::clone!(
        #[weak]
        state,
        move |switch| state.set_auto_mode(switch.is_active())
    ));
    state.approval.approve.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.resolve_approval(true)
    ));
    state.approval.deny.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.resolve_approval(false)
    ));
    agent_button.connect_toggled(glib::clone!(
        #[weak]
        state,
        move |button| state.set_agent_visible(button.is_active())
    ));

    window.connect_fullscreened_notify(glib::clone!(
        #[weak]
        state,
        move |window| state.fullscreen_changed(window.is_fullscreen())
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
    // This handler owns the window state; every other closure holds a weak reference.
    window.connect_close_request(move |_| {
        let _ = std::fs::remove_file(&state.socket_path);
        let _ = std::fs::remove_file(&state.mcp_config);
        glib::Propagation::Proceed
    });

    terminal.grab_focus();
    window
}

fn new_terminal() -> vte4::Terminal {
    vte4::Terminal::builder().hexpand(true).vexpand(true).build()
}

/// Splits a shell setting like `/usr/bin/zsh -l` into argv on whitespace.
fn shell_words(command: &str) -> Vec<String> {
    command.split_whitespace().map(str::to_owned).collect()
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
    mode_row.add_css_class("solid");

    let pane = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .visible(false)
        .build();
    pane.append(&mode_row);
    pane.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
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
        .visible(false)
        .css_classes(["error", "heading"])
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
    ApprovalBar { revealer, label, warning, approve, deny }
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
            "Stop the agent and terminate the command it's running (Ctrl+Shift+Esc).\n\
             Click three times quickly for a Hard Stop, which disconnects the agent from this window.",
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

fn spawn(
    terminal: &vte4::Terminal,
    argv: &[&str],
    cwd: &Path,
    on_started: impl FnOnce(glib::Pid) + 'static,
) {
    let program = argv[0].to_owned();
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        cwd.to_str(),
        argv,
        &[],
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

impl Bishop {
    fn add_actions(self: &Rc<Self>) {
        let action = |name: &str, f: fn(&Rc<Bishop>)| {
            let state = Rc::downgrade(self);
            gio::ActionEntry::builder(name)
                .activate(move |_: &adw::ApplicationWindow, _, _| {
                    if let Some(state) = state.upgrade() {
                        f(&state);
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
            action("zoom-in", |s| s.set_zoom(s.terminal.font_scale() + ZOOM_STEP)),
            action("zoom-out", |s| s.set_zoom(s.terminal.font_scale() - ZOOM_STEP)),
            action("zoom-reset", |s| s.set_zoom(1.0)),
            action("copy", |s| s.focused_terminal().copy_clipboard_format(vte4::Format::Text)),
            action("paste", |s| s.focused_terminal().paste_clipboard()),
            action("agent-stop", |s| s.emergency_stop()),
            action("agent-hard-stop", |s| {
                s.emergency_stop();
                s.hard_stop();
            }),
            action("agent-resume", |s| s.resume_agent()),
            action("preferences", |s| prefs::show_preferences(&s.window)),
            action("agent-config", |s| {
                let state = Rc::downgrade(s);
                prefs::show_agent_config(&s.window, move || {
                    if let Some(state) = state.upgrade() {
                        state.restart_agent();
                    }
                });
            }),
        ]);
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
        if !visible {
            self.terminal.grab_focus();
            return;
        }
        if !self.paned_positioned.replace(true) {
            self.paned.set_position((self.paned.width() as f64 * 0.6) as i32);
        }
        self.start_agent();
        self.agent_terminal.grab_focus();
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
            "--append-system-prompt",
            AGENT_PROMPT,
        ];
        if !agent.model.is_empty() {
            argv.extend(["--model", &agent.model]);
        }
        self.agent_terminal.reset(true, true);
        self.agent_running.set(true);
        self.agent_started.set(true);
        self.update_stop_buttons();
        let state = Rc::downgrade(self);
        spawn(&self.agent_terminal, &argv, &workspace, move |pid| {
            if let Some(state) = state.upgrade() {
                state.agent_pid.set(Some(pid));
            }
        });
    }

    /// Restarts Claude in this window's panel (showing the panel if hidden), so new agent
    /// settings take effect.
    fn restart_agent(self: &Rc<Self>) {
        if self.hard_stopped.get() {
            self.toast("The agent was disconnected by a Hard Stop. Open a new window to use it again.");
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
                if self.agent_button.is_active() {
                    self.start_agent();
                } else {
                    self.agent_button.set_active(true);
                }
            }
        }
        self.toast("Restarting the agent");
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

    /// Emergency Stop is visible once the agent has started here, and in full screen it
    /// floats over the terminal so it's never hidden. A Hard Stop retires it.
    fn update_stop_buttons(&self) {
        let show = self.agent_started.get() && !self.hard_stopped.get();
        self.stop_button.set_visible(show);
        self.floating_stop_button.set_visible(show && self.window.is_fullscreen());
    }

    fn fullscreen_changed(&self, fullscreen: bool) {
        self.toolbar.set_reveal_top_bars(!fullscreen);
        self.update_stop_buttons();
        self.window.change_action_state("fullscreen", &fullscreen.to_variant());
        if fullscreen {
            self.toast("Press F11, or move the pointer to the top edge, to leave full screen");
        }
    }

    fn set_zoom(&self, scale: f64) {
        let scale = ((scale * 10.0).round() / 10.0).clamp(MIN_ZOOM, MAX_ZOOM);
        self.terminal.set_font_scale(scale);
        self.agent_terminal.set_font_scale(scale);
        self.zoom_label.set_text(&format!("{:.0}%", scale * 100.0));
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

/// `~/.local/share/bishop/agent`: the agent panel's working directory, holding the Bishop
/// guide as CLAUDE.md so Claude Code loads it as project memory.
fn agent_workspace() -> std::io::Result<PathBuf> {
    let dir = agent_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("CLAUDE.md"), AGENT_GUIDE)?;
    Ok(dir)
}
