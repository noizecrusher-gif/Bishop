//! The agent's side of a tab: MCP tool calls, approvals, guardrails, and takeover.

use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use serde_json::{Value, json};
use vte4::prelude::*;

use super::Session;
use crate::safety;

/// How often waits re-check the terminal.
const POLL: Duration = Duration::from_millis(150);
/// Without shell integration, output must be quiet this long (with a prompt showing) for a
/// command to count as finished.
const QUIET: Duration = Duration::from_millis(1200);
/// A user keypress this soon after the agent typed counts as taking over.
const TAKEOVER_WINDOW: Duration = Duration::from_secs(2);
/// `run_command` and `wait_for` return at most this much output.
const MAX_OUTPUT_LINES: usize = 200;

const DECLINED: &str = "The user declined this input. Ask them how they would like to proceed.";
const INTERRUPTED: &str = "Interrupted: the user started typing in the terminal and has taken \
over. Automatic mode is now off. Read the screen and ask the user before doing anything else.";
pub(super) const STOPPED: &str = "Emergency stop: the user stopped you and terminated the running \
command. Bishop refuses all input from you until the user resumes the agent. Do not retry. Tell \
the user what you were doing when you were stopped, and wait for their instructions.";
const HARD_STOPPED: &str = "Hard stop: the user disconnected you from this terminal.";
const BUSY: &str = "Another input of yours is still waiting for the user's approval. Wait for \
that call to return before sending more.";
const CHANGED: &str = "Not typed: while the user was approving, the terminal changed (a different \
program is now in front, or it is at a password prompt). Read the screen and ask again.";
const NOT_PRINTABLE: &str = "`text` must be printable. Send Enter, Tab, Escape, Ctrl-C and other \
control keys with `keys`.";
/// Keys that bring back a line from shell history: what they submit can't be checked.
const HISTORY_KEYS: &[&[u8]] =
    &[b"\x1b[A", b"\x1bOA", b"\x1b[B", b"\x1bOB", b"\x10", b"\x0e", b"\x12", b"\x1b.", b"\x1b_"];
/// After the approval bar shows a new request, its buttons wait this long, so a click meant
/// for something else can't approve it.
const APPROVAL_ARM: Duration = Duration::from_millis(800);
const PASSWORD_BLOCKED: &str = "Blocked: the terminal is at a password prompt, and Bishop never \
lets the agent type there. Ask the user to enter it themselves (you may send only ctrl-c to cancel).";

/// The shell hook's message travels separately from the terminal output, so it can arrive
/// before VTE has drawn the command's last output and the new prompt. Once it does, the screen
/// is read when the cursor has reached the prompt line and output has been quiet this long,
/// or after `MAX_SETTLE` regardless.
const SETTLE: Duration = Duration::from_millis(80);
const MAX_SETTLE: Duration = Duration::from_millis(2000);

#[derive(Clone, Copy, Debug)]
pub(super) enum ShellEvent {
    /// The command finished with status `exit`; the shell is about to draw its prompt.
    Finished { exit: Option<u64> },
}

/// The approval bar shows at most this many characters of the input, and of the warning;
/// See More opens the whole thing in its own window.
const PREVIEW_CHARS: usize = 160;
const WARNING_PREVIEW_CHARS: usize = 160;

/// Counts an in-flight agent operation for as long as it is alive.
struct Busy<'a>(&'a Session);

impl<'a> Busy<'a> {
    fn new(session: &'a Session) -> Self {
        session.agent_busy.set(session.agent_busy.get() + 1);
        session.refresh_tab();
        Self(session)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.agent_busy.set(self.0.agent_busy.get() - 1);
        self.0.refresh_tab();
    }
}

impl Session {
    pub(super) async fn handle_tool(self: &Rc<Self>, name: &str, args: &Value) -> Result<String, String> {
        if self.hard_stopped.get() {
            return Err(HARD_STOPPED.into());
        }
        let result = match name {
            "read_screen" => Ok(self.screen_text()),
            "read_scrollback" => {
                let lines = args["lines"].as_i64().unwrap_or(200).clamp(1, 5000);
                Ok(self.scrollback_text(lines))
            }
            "get_session_info" => Ok(self.session_info()),
            "send_keys" => self.send_keys(args).await,
            "run_command" => self.run_command(args).await,
            "wait_for" => self.wait_for(args).await,
            _ => Err(format!("unknown tool: {name}")),
        };
        // What the model sees: recognizable secrets are hidden unless the user opted out.
        if crate::settings::get().agent.share_secrets { result } else { result.map(|text| safety::redact(&text)) }
    }

    async fn send_keys(self: &Rc<Self>, args: &Value) -> Result<String, String> {
        if args["text"].as_str().is_some_and(|text| text.chars().any(char::is_control)) {
            return Err(NOT_PRINTABLE.into());
        }
        let bytes = encode_input(args)?;
        if bytes.is_empty() {
            return Err("nothing to send: provide `text` and/or `keys`".into());
        }
        self.authorize("send_keys", &describe_input(args), &bytes).await?;
        let _busy = Busy::new(self);
        self.type_input(&bytes)?;
        let wait = args["wait_ms"].as_u64().unwrap_or(500).min(10_000);
        glib::timeout_future(Duration::from_millis(wait)).await;
        Ok(format!("Sent {} bytes. Screen after {wait} ms:\n{}", bytes.len(), self.screen_text()))
    }

    async fn run_command(self: &Rc<Self>, args: &Value) -> Result<String, String> {
        let command = args["command"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .ok_or("`command` is required")?;
        if command.contains(['\n', '\r']) {
            return Err("Run one command per call: `command` must not contain newlines.".into());
        }
        if command.chars().any(char::is_control) {
            return Err("`command` must not contain control characters.".into());
        }
        let timeout = Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(30_000).clamp(1_000, 600_000));
        let bytes = format!("{command}\r").into_bytes();
        self.authorize("run_command", &format!("{} ⏎", safety::visible(command)), &bytes).await?;

        let _busy = Busy::new(self);
        let generation = self.cancel_generation.get();
        let tracked = self.at_local_prompt();
        let (tx, events) = async_channel::unbounded();
        self.shell_waiters.borrow_mut().push(tx);
        let (start_col, start_row) = self.terminal.cursor_position();
        // Output starts on the line after the command, which may wrap over several lines.
        let columns = self.terminal.column_count().max(1);
        let end = start_col + command.chars().count() as i64;
        let output_row = start_row + ((end + columns - 1) / columns).max(1);
        let prompt = self.cursor_line();
        self.type_input(&bytes)?;

        if tracked {
            self.wait_for_prompt(events, output_row, &prompt, timeout, generation).await
        } else {
            self.wait_until_quiet(output_row, &prompt, timeout, generation).await
        }
    }

    /// Waits for the shell's precmd event, which marks exactly when the command finished.
    async fn wait_for_prompt(
        &self,
        events: async_channel::Receiver<ShellEvent>,
        output_row: i64,
        prompt: &str,
        timeout: Duration,
        generation: u64,
    ) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancel_generation.get() != generation {
                return Err(self.interruption());
            }
            if Instant::now() >= deadline {
                return Ok(self.still_running(timeout));
            }
            match glib::future_with_timeout(POLL, events.recv()).await {
                Ok(Ok(ShellEvent::Finished { .. })) if self.cancel_generation.get() != generation => {
                    return Err(self.interruption());
                }
                Ok(Ok(ShellEvent::Finished { exit })) => {
                    self.settle(output_row, prompt).await;
                    if self.cancel_generation.get() != generation {
                        return Err(self.interruption());
                    }
                    let (col, row) = self.terminal.cursor_position();
                    let output = self.output_before(output_row, row, col, prompt);
                    let exit = exit.map_or("unknown".to_owned(), |code| code.to_string());
                    return Ok(format!("Exit code: {exit}\n\n{}", limit_output(&output)));
                }
                Ok(Err(_)) => return Err("the terminal closed".into()),
                Err(_) => {}
            }
        }
    }

    /// Waits for VTE to catch up with the shell after a finished-command event: the cursor on
    /// or past the line where the prompt must start, showing a prompt, and output quiet.
    async fn settle(&self, output_row: i64, prompt: &str) {
        let started = Instant::now();
        let prompt = prompt.trim_end();
        while started.elapsed() < MAX_SETTLE {
            let line = self.cursor_line();
            let at_prompt = self.terminal.cursor_position().1 >= output_row
                && (looks_like_prompt(&line) || (!prompt.is_empty() && line.starts_with(prompt)));
            if at_prompt && self.last_output.get().elapsed() >= SETTLE {
                return;
            }
            glib::timeout_future(Duration::from_millis(20)).await;
        }
    }

    /// Without shell integration (e.g. over SSH): finished once output has gone quiet and
    /// the cursor line looks like a prompt.
    async fn wait_until_quiet(
        &self,
        output_row: i64,
        prompt: &str,
        timeout: Duration,
        generation: u64,
    ) -> Result<String, String> {
        let started = Instant::now();
        loop {
            glib::timeout_future(POLL).await;
            if self.cancel_generation.get() != generation {
                return Err(self.interruption());
            }
            let quiet = self.last_output.get().elapsed() >= QUIET && started.elapsed() >= QUIET;
            if quiet && looks_like_prompt(&self.cursor_line()) {
                let (col, row) = self.terminal.cursor_position();
                let output = self.output_before(output_row, row, col, prompt);
                return Ok(format!(
                    "The command appears finished: output stopped and a prompt is showing. This \
                     session has no shell integration (for example, it's connected over SSH), so the \
                     exit code is unknown; run `echo $?` next if you need it.\n\n{}",
                    limit_output(&output)
                ));
            }
            if started.elapsed() >= timeout {
                return Ok(self.still_running(timeout));
            }
        }
    }

    fn still_running(&self, timeout: Duration) -> String {
        format!(
            "No prompt after {}s: the command is still running or waiting for input (expected if it \
             started an interactive program such as ssh, vim, or a REPL). Use wait_for or read_screen \
             to check on it, or send ctrl-c to stop it.\n\nScreen now:\n{}",
            timeout.as_secs(),
            self.screen_text()
        )
    }

    async fn wait_for(&self, args: &Value) -> Result<String, String> {
        let pattern = args["pattern"].as_str().ok_or("`pattern` is required")?;
        let regex = regex::Regex::new(pattern).map_err(|e| format!("invalid pattern: {e}"))?;
        let timeout = Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(30_000).clamp(100, 600_000));
        let _busy = Busy::new(self);
        let generation = self.cancel_generation.get();
        let start_row = self.terminal.cursor_position().1;
        let started = Instant::now();
        loop {
            let (col, row) = self.terminal.cursor_position();
            let text = self.text_between(start_row, row, col.max(self.terminal.column_count()));
            if let Some(found) = regex.find(&text) {
                return Ok(format!(
                    "Matched {:?} after {:.1}s.\n\nScreen now:\n{}",
                    found.as_str(),
                    started.elapsed().as_secs_f64(),
                    self.screen_text()
                ));
            }
            if self.cancel_generation.get() != generation {
                return Err(self.interruption());
            }
            if started.elapsed() >= timeout {
                return Ok(format!(
                    "No match for {pattern:?} after {}s.\n\nScreen now:\n{}",
                    timeout.as_secs(),
                    self.screen_text()
                ));
            }
            glib::timeout_future(POLL).await;
        }
    }

    /// Applies the guardrails to agent input, asking the user when needed. Every outcome is
    /// written to the audit log.
    async fn authorize(&self, tool: &str, display: &str, bytes: &[u8]) -> Result<(), String> {
        if self.stopped.get() {
            self.audit(tool, display, "blocked: emergency stop", None);
            return Err(STOPPED.into());
        }
        let (_, password_prompt) = self.pty_state();
        if password_prompt && bytes != b"\x03" {
            self.audit(tool, display, "blocked: password prompt", None);
            return Err(PASSWORD_BLOCKED.into());
        }

        let warning = self.warning_for(bytes);
        // Nothing is typed while an approval is pending: the approval applies to what the bar
        // shows, in the terminal as the user sees it (even harmless input, like `cd`, could
        // change what an approved command does).
        if self.pending_approval.borrow().is_some() {
            self.audit(tool, display, "refused: another input awaits approval", warning);
            return Err(BUSY.into());
        }
        if self.auto_mode.get() && warning.is_none() {
            self.audit(tool, display, "automatic", None);
            return Ok(());
        }
        let generation = self.cancel_generation.get();
        let foreground = self.foreground_group();
        let approved = self.request_approval(display, warning).await;
        if self.cancel_generation.get() != generation {
            self.audit(tool, display, "interrupted", warning);
            return Err(self.interruption());
        }
        if !approved {
            self.audit(tool, display, "denied", warning);
            return Err(DECLINED.into());
        }
        // The wait can be long: what was approved must still be what will be typed into.
        let (_, password_prompt) = self.pty_state();
        if self.stopped.get() {
            return Err(STOPPED.into());
        }
        if (password_prompt && bytes != b"\x03") || self.foreground_group() != foreground {
            self.audit(tool, display, "refused: terminal changed while waiting", warning);
            return Err(CHANGED.into());
        }
        self.audit(tool, display, "approved", warning);
        Ok(())
    }

    /// Why `bytes` needs the user's approval regardless of automatic mode, if it does.
    fn warning_for(&self, bytes: &[u8]) -> Option<&'static str> {
        let typed = String::from_utf8_lossy(bytes);
        if safety::hidden_characters(&typed) {
            return Some("contains invisible or direction-changing characters, so it may not be what it looks like");
        }
        // Enter, Ctrl-J and Ctrl-O all run the line.
        let submits = bytes.iter().any(|b| matches!(b, b'\r' | b'\n' | 0x0f));
        if submits && HISTORY_KEYS.iter().any(|key| bytes.windows(key.len()).any(|w| w == *key)) {
            return Some("runs a line recalled from shell history, which Bishop can't check");
        }
        safety::dangerous(&typed).or_else(|| {
            // On submit, the whole command line: what's already there (it may wrap over several
            // rows, or have been typed earlier) followed by what's typed now.
            submits.then(|| safety::dangerous(&format!("{}{typed}", self.command_line()))).flatten()
        })
    }

    fn foreground_group(&self) -> Option<i32> {
        let pty = self.terminal.pty()?;
        Some(unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) })
    }

    async fn request_approval(&self, display: &str, warning: Option<&str>) -> bool {
        let (tx, rx) = async_channel::bounded(1);
        self.pending_approval.replace(Some(tx));
        self.close_approval_dialog();
        let bar = &self.approval;
        let warning_text = warning.map(|reason| format!("⚠ This looks destructive: it {reason}. Check it carefully."));
        let (input_preview, input_cut) = preview(display, PREVIEW_CHARS);
        let (warning_preview, warning_cut) = preview(warning_text.as_deref().unwrap_or_default(), WARNING_PREVIEW_CHARS);
        bar.label.set_text(&input_preview);
        bar.warning.set_visible(warning.is_some());
        bar.warning.set_text(&warning_preview);
        bar.more.set_visible(input_cut || warning_cut);
        self.approval_details.replace(Some((display.to_owned(), warning_text)));
        if warning.is_some() {
            bar.approve.remove_css_class("suggested-action");
            bar.approve.add_css_class("destructive-action");
        } else {
            bar.approve.remove_css_class("destructive-action");
            bar.approve.add_css_class("suggested-action");
        }
        bar.revealer.set_reveal_child(true);
        arm_after_delay(&bar.approve);
        self.refresh_tab();
        let approved = rx.recv().await.unwrap_or(false);
        if self.pending_approval.borrow().is_none() {
            bar.revealer.set_reveal_child(false);
            self.approval_details.take();
            self.close_approval_dialog();
        }
        self.refresh_tab();
        approved
    }

    /// See More: the whole input and warning in a scrollable window, with Confirm and Cancel.
    pub(super) fn show_approval_details(self: &Rc<Self>) {
        let Some((input, warning)) = self.approval_details.borrow().clone() else { return };
        self.close_approval_dialog();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        if let Some(warning) = &warning {
            content.append(
                &gtk::Label::builder()
                    .label(warning)
                    .xalign(0.0)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .selectable(true)
                    .css_classes(["error", "heading"])
                    .build(),
            );
        }
        content.append(&gtk::Label::builder().label("Claude wants to type:").xalign(0.0).css_classes(["heading"]).build());
        let text = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .vexpand(true)
            .css_classes(["card", "monospace"])
            .build();
        text.buffer().set_text(&input);
        content.append(&text);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let cancel = gtk::Button::builder().label("Cancel").build();
        let confirm = gtk::Button::builder()
            .label("Confirm")
            .css_classes([if warning.is_some() { "destructive-action" } else { "suggested-action" }])
            .build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::End)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        buttons.append(&cancel);
        buttons.append(&confirm);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        toolbar.add_bottom_bar(&buttons);
        arm_after_delay(&confirm);
        let dialog = adw::Window::builder()
            .title("Review Claude's Input")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(720)
            .default_height(480)
            .content(&toolbar)
            .build();

        // The approval bar resolves the request and closes this window.
        cancel.connect_clicked(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |_| state.resolve_approval(false)
        ));
        confirm.connect_clicked(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |_| state.resolve_approval(true)
        ));
        dialog.connect_close_request(glib::clone!(
            #[weak(rename_to = state)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                state.approval_dialog.take();
                glib::Propagation::Proceed
            }
        ));
        self.approval_dialog.replace(Some(dialog.clone()));
        dialog.present();
        cancel.grab_focus();
    }

    pub(super) fn close_approval_dialog(&self) {
        let dialog = self.approval_dialog.take();
        if let Some(dialog) = dialog {
            dialog.close();
        }
    }

    pub(super) fn resolve_approval(&self, approved: bool) {
        if let Some(tx) = self.pending_approval.take() {
            let _ = tx.try_send(approved);
        }
    }

    pub(super) fn set_auto_mode(&self, enabled: bool) {
        self.auto_mode.set(enabled);
        if self.quiet_auto_toggle.replace(false) {
            return;
        }
        if enabled {
            self.resolve_approval(true);
            self.toast("Automatic mode on: Claude types without asking");
        } else {
            self.toast("Automatic mode off: Claude asks before typing");
        }
    }

    /// The user typed in the terminal while the agent was working: cancel pending approvals
    /// and waits, and turn automatic mode off.
    pub(super) fn take_over(&self) {
        let active = self.pending_approval.borrow().is_some()
            || self.agent_busy.get() > 0
            || self.last_agent_input.get().is_some_and(|at| at.elapsed() < TAKEOVER_WINDOW);
        if !active {
            return;
        }
        self.cancel_generation.set(self.cancel_generation.get() + 1);
        self.last_agent_input.set(None);
        self.resolve_approval(false);
        if self.auto_switch.is_active() {
            self.quiet_auto_toggle.set(true);
            self.auto_switch.set_active(false);
        }
        self.audit("takeover", "", "user typed", None);
        self.toast("You took over: Claude's pending actions were cancelled and automatic mode is off");
    }

    /// The only path from the agent to the terminal. Everything runs on the main thread, so
    /// once Emergency Stop sets `stopped`, no agent input can reach the terminal.
    fn type_input(self: &Rc<Self>, bytes: &[u8]) -> Result<(), String> {
        if self.stopped.get() || self.hard_stopped.get() {
            return Err(STOPPED.into());
        }
        self.show_typing_banner();
        self.last_agent_input.set(Some(Instant::now()));
        if self.agent_driving_since.get().is_none() {
            self.agent_driving_since.set(Some(crate::procs::now_ticks()));
        }
        self.terminal.feed_child(bytes);
        Ok(())
    }

    /// The user pressed a key in the terminal: the agent is no longer the one driving it.
    pub(super) fn user_typed(&self) {
        if let Some(since) = self.agent_driving_since.take() {
            self.agent_driving_spans.borrow_mut().push((since, crate::procs::now_ticks()));
        }
    }

    /// Whether a process started while the agent was driving the terminal.
    pub(super) fn started_by_agent(&self, process: &crate::procs::Process) -> bool {
        // Allow a tick of slack for clock rounding.
        let start = process.start + 1;
        self.agent_driving_since.get().is_some_and(|since| start >= since)
            || self.agent_driving_spans.borrow().iter().any(|&(from, to)| start >= from && process.start <= to)
    }

    /// Why an in-flight call was cut short.
    fn interruption(&self) -> String {
        if self.stopped.get() { STOPPED } else { INTERRUPTED }.into()
    }

    fn show_typing_banner(self: &Rc<Self>) {
        let generation = self.banner_generation.get() + 1;
        self.banner_generation.set(generation);
        self.banner.set_revealed(true);
        let state = Rc::downgrade(self);
        glib::timeout_add_local_once(super::BANNER_HOLD, move || {
            if let Some(state) = state.upgrade()
                && state.banner_generation.get() == generation
            {
                state.banner.set_revealed(false);
            }
        });
    }

    /// The shell's integration hook reported a finished command.
    pub(super) fn hook_event(&self, event: crate::shell::Event) {
        self.integration_seen.set(true);
        let crate::shell::Event::Precmd(exit) = event else { return };
        let event = ShellEvent::Finished { exit };
        self.shell_waiters.borrow_mut().retain(|tx| tx.try_send(event).is_ok());
    }

    /// True when the user's own shell is at its prompt and reports shell-integration events,
    /// so `run_command` can tell exactly when a command finishes.
    fn at_local_prompt(&self) -> bool {
        let Some(shell) = self.shell_pid.get() else { return false };
        let Some(pty) = self.terminal.pty() else { return false };
        self.integration_seen.get() && unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) } == shell.0
    }

    pub(super) fn screen_text(&self) -> String {
        let t = &self.terminal;
        let top = t.vadjustment().map_or(0, |adj| adj.value() as i64);
        let (col, row) = t.cursor_position();
        let text = t.text_format(vte4::Format::Text).unwrap_or_default();
        format!(
            "[{}x{} screen, cursor at row {}, column {}]\n{}",
            t.column_count(),
            t.row_count(),
            row - top,
            col,
            text.trim_end()
        )
    }

    fn scrollback_text(&self, lines: i64) -> String {
        let t = &self.terminal;
        let Some(adj) = t.vadjustment() else { return String::new() };
        // End at the cursor line; rows below it are empty screen.
        let end = t.cursor_position().1;
        let start = (end - lines + 1).max(adj.lower() as i64);
        self.text_between(start, end, t.column_count())
    }

    /// Text from the start of `start_row` up to `end_row`/`end_col`.
    fn text_between(&self, start_row: i64, end_row: i64, end_col: i64) -> String {
        if start_row > end_row {
            return String::new();
        }
        let (text, _) = self.terminal.text_range_format(vte4::Format::Text, start_row, 0, end_row, end_col);
        text.map(|text| text.trim_end().to_owned()).unwrap_or_default()
    }

    /// Command output from `start_row` through `row`/`col`, minus the shell's next prompt.
    /// VTE reports shell events after drawing the prompt, so the last line usually ends in
    /// it; `prompt` is the prompt seen before the command ran.
    fn output_before(&self, start_row: i64, row: i64, col: i64, prompt: &str) -> String {
        let mut output = self.text_between(start_row, row - 1, self.terminal.column_count());
        if col > 0 && row >= start_row {
            let line: String =
                self.text_between(row, row, self.terminal.column_count()).chars().take(col as usize).collect();
            let rest = strip_prompt(&line, prompt);
            if !rest.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(rest);
            }
        }
        output
    }

    /// The whole line the cursor is on, including rows it has wrapped onto: VTE joins soft-wrapped
    /// rows without a newline, so it's whatever follows the last hard line break.
    fn command_line(&self) -> String {
        let row = self.terminal.cursor_position().1;
        let text = self.text_between((row - 40).max(0), row, self.terminal.column_count());
        text.rsplit('\n').next().unwrap_or_default().to_owned()
    }

    pub(super) fn cursor_line(&self) -> String {
        let row = self.terminal.cursor_position().1;
        self.text_between(row, row, self.terminal.column_count())
    }

    fn session_info(&self) -> String {
        let t = &self.terminal;
        #[allow(deprecated)]
        let (title, cwd) = (t.window_title(), t.current_directory_uri());
        let (foreground, password_prompt) = self.pty_state();
        let info = json!({
            "title": title.as_deref(),
            "cwd": cwd.as_deref(),
            "foreground_process": foreground,
            "columns": t.column_count(),
            "rows": t.row_count(),
            "password_prompt_likely": password_prompt,
            "at_local_shell_prompt": self.at_local_prompt(),
            "shell_integration": self.integration_seen.get(),
            "automatic_mode": self.auto_mode.get(),
        });
        serde_json::to_string_pretty(&info).unwrap_or_default()
    }

    /// Foreground process command line, and whether the terminal looks like it is at a
    /// password prompt: echo is off and either the TTY is in canonical mode (classic
    /// getpass) or the cursor line reads like a password prompt (sudo-rs reads in raw
    /// mode, and over SSH the local TTY is always raw).
    fn pty_state(&self) -> (Option<String>, bool) {
        let Some(pty) = self.terminal.pty() else { return (None, false) };
        let fd = pty.fd().as_raw_fd();
        let pgrp = unsafe { libc::tcgetpgrp(fd) };
        let foreground = (pgrp > 0)
            .then(|| std::fs::read(format!("/proc/{pgrp}/cmdline")).ok())
            .flatten()
            .map(|raw| String::from_utf8_lossy(&raw).trim_end_matches('\0').replace('\0', " "));

        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        let echo_off = unsafe { libc::tcgetattr(fd, &mut termios) } == 0 && termios.c_lflag & libc::ECHO == 0;
        let password_prompt = echo_off
            && (termios.c_lflag & libc::ICANON != 0 || safety::looks_like_password_prompt(&self.cursor_line()));
        (foreground, password_prompt)
    }

    pub(super) fn audit(&self, tool: &str, input: &str, decision: &str, warning: Option<&str>) {
        safety::audit(json!({
            "tab": self.session_id,
            "tool": tool,
            "input": input,
            "decision": decision,
            "warning": warning,
            "foreground_process": self.pty_state().0,
        }));
    }
}

/// At most `max` characters of `text`, and whether any were cut. Long text keeps its start
/// and its end, since a harmful part can hide at either.
fn preview(text: &str, max: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= max {
        return (text.to_owned(), false);
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text.chars().skip(count - max / 3).collect();
    (format!("{} … {}", head.trim_end(), tail.trim_start()), true)
}

/// Makes `button` clickable only after `APPROVAL_ARM`.
fn arm_after_delay(button: &gtk::Button) {
    button.set_sensitive(false);
    glib::timeout_add_local_once(
        APPROVAL_ARM,
        glib::clone!(
            #[weak]
            button,
            move || button.set_sensitive(true)
        ),
    );
}

/// Keeps the last `MAX_OUTPUT_LINES` lines, noting how many were left out.
fn limit_output(output: &str) -> String {
    if output.is_empty() {
        return "(no output)".into();
    }
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= MAX_OUTPUT_LINES {
        return output.to_owned();
    }
    format!(
        "[showing the last {MAX_OUTPUT_LINES} of {} lines; use read_scrollback for more]\n{}",
        lines.len(),
        lines[lines.len() - MAX_OUTPUT_LINES..].join("\n")
    )
}

/// What's left of the line the next prompt was drawn on: output that didn't end with a
/// newline, if any.
fn strip_prompt<'a>(line: &'a str, prompt: &str) -> &'a str {
    let line = line.trim_end();
    let prompt = prompt.trim();
    if !prompt.is_empty()
        && let Some(rest) = line.strip_suffix(prompt)
    {
        return rest.trim_end();
    }
    if looks_like_prompt(line) { "" } else { line }
}

pub(super) fn looks_like_prompt(line: &str) -> bool {
    let line = line.trim_end();
    // zsh prompts end in `%`, but so does progress like `45%`.
    if let Some(rest) = line.strip_suffix('%') {
        return !rest.ends_with(|c: char| c.is_ascii_digit());
    }
    line.ends_with(['$', '#', '>', '❯', '»'])
}

/// Human-readable form of a `send_keys` request, e.g. `ls -la ⏎`.
fn describe_input(args: &Value) -> String {
    let mut out = safety::visible(args["text"].as_str().unwrap_or_default());
    for key in args["keys"].as_array().into_iter().flatten() {
        let name = key.as_str().unwrap_or_default();
        let shown = match name.to_ascii_lowercase().as_str() {
            "enter" | "return" => "⏎".to_owned(),
            other => format!("[{other}]"),
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&shown);
    }
    out
}

fn encode_input(args: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = args["text"].as_str().unwrap_or_default().as_bytes().to_vec();
    for key in args["keys"].as_array().into_iter().flatten() {
        let name = key.as_str().ok_or("`keys` must be an array of strings")?;
        bytes.extend(key_bytes(name).ok_or_else(|| format!("unknown key: {name}"))?);
    }
    Ok(bytes)
}

fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let name = name.to_ascii_lowercase();
    if let Some(letter) = name.strip_prefix("ctrl-").or_else(|| name.strip_prefix("ctrl+")) {
        let &[c] = letter.as_bytes() else { return None };
        return c.is_ascii_lowercase().then(|| vec![c & 0x1f]);
    }
    let seq: &[u8] = match name.as_str() {
        "enter" | "return" => b"\r",
        "tab" => b"\t",
        "escape" | "esc" => b"\x1b",
        "backspace" => b"\x7f",
        "delete" => b"\x1b[3~",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        _ => return None,
    };
    Some(seq.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_text_then_keys() {
        let args = json!({ "text": "ls", "keys": ["enter", "ctrl-c", "up"] });
        assert_eq!(encode_input(&args).unwrap(), b"ls\r\x03\x1b[A");
        assert!(encode_input(&json!({ "keys": ["hyper-x"] })).is_err());
        assert_eq!(describe_input(&args), "ls ⏎ [ctrl-c] [up]");
    }

    #[test]
    fn recognizes_prompts() {
        assert!(looks_like_prompt("user@web01:~$ "));
        assert!(looks_like_prompt("root@db1:/var/log# "));
        assert!(looks_like_prompt("~/src ❯"));
        assert!(!looks_like_prompt("[sudo] password for user:"));
        assert!(!looks_like_prompt("Downloading... 45%  "));
    }

    #[test]
    fn strips_the_next_prompt() {
        let prompt = "kim@web01:~$ ";
        assert_eq!(strip_prompt("kim@web01:~$", prompt), "");
        assert_eq!(strip_prompt("no newlinekim@web01:~$", prompt), "no newline");
        assert_eq!(strip_prompt("kim@web01:/tmp$", prompt), "");
        assert_eq!(strip_prompt("partial output", prompt), "partial output");
    }

    #[test]
    fn previews_long_input() {
        assert_eq!(preview("ls -la", 10), ("ls -la".to_owned(), false));
        let long = "é".repeat(300);
        let (shown, cut) = preview(&long, PREVIEW_CHARS);
        assert!(cut);
        assert!(shown.chars().count() <= PREVIEW_CHARS + 3);
        assert!(shown.contains(" … "));
        // The end is always shown.
        let sneaky = format!("ls {}; rm -rf ~", "a".repeat(400));
        assert!(preview(&sneaky, PREVIEW_CHARS).0.ends_with("rm -rf ~"));
    }

    #[test]
    fn limits_long_output() {
        let long: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let limited = limit_output(long.trim_end());
        assert!(limited.starts_with("[showing the last 200 of 500 lines"));
        assert!(limited.ends_with("line 499"));
        assert_eq!(limit_output(""), "(no output)");
    }
}
