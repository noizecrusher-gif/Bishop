//! Emergency Stop and Hard Stop.
//!
//! Emergency Stop refuses all further agent input (see `type_input`), cancels anything the
//! agent is waiting on, interrupts Claude's turn, and terminates whatever is running in the
//! terminal, escalating to SIGKILL. Processes Bishop isn't allowed to kill are reported
//! with a way to finish the job, never silently left behind.
//!
//! Hard Stop also kills Claude Code and disconnects it from the window for good.

use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};
use vte4::prelude::*;

use super::Bishop;
use crate::procs::{self, Process};

/// How long each escalation step gets before the next one.
const CTRL_C_GRACE: Duration = Duration::from_millis(500);
const TERM_GRACE: Duration = Duration::from_millis(1000);
const KILL_GRACE: Duration = Duration::from_millis(1000);
/// Over SSH, how long to wait for the remote prompt to come back after Ctrl-C.
const REMOTE_GRACE: Duration = Duration::from_millis(2000);

/// Foreground programs that relay keystrokes to another machine.
const REMOTE_CLIENTS: &[&str] = &["ssh", "mosh-client", "mosh", "telnet", "et"];

impl Bishop {
    /// The Emergency Stop button. Three clicks within `HARD_STOP_CLICKS` also Hard Stop.
    pub(super) fn stop_clicked(self: &Rc<Self>) {
        let now = Instant::now();
        let hard = {
            let mut clicks = self.stop_clicks.borrow_mut();
            clicks.retain(|at| now.duration_since(*at) < super::HARD_STOP_CLICKS);
            clicks.push(now);
            clicks.len() >= 3
        };
        self.emergency_stop();
        if hard {
            self.hard_stop();
        }
    }

    pub(super) fn emergency_stop(self: &Rc<Self>) {
        // Anything still running was started under the agent's control or the user's; the
        // agent's span ends now.
        if let Some(since) = self.agent_driving_since.take() {
            self.agent_driving_spans.borrow_mut().push((since, crate::procs::now_ticks()));
        }
        // First, and synchronously: no agent input can reach the terminal from here on.
        let first = !self.stopped.replace(true);
        self.cancel_generation.set(self.cancel_generation.get() + 1);
        self.resolve_approval(false);
        self.last_agent_input.set(None);
        if self.auto_switch.is_active() {
            self.quiet_auto_toggle.set(true);
            self.auto_switch.set_active(false);
        }
        self.stopped_banner.set_revealed(true);
        if first {
            // Escape interrupts Claude Code's current turn. Only once: a second Escape opens its
            // rewind menu.
            if self.agent_running.get() {
                self.agent_terminal.feed_child(b"\x1b");
            }
            self.audit("emergency_stop", "", "stopped by user", None);
        }
        if !self.terminating.replace(true) {
            let state = self.clone();
            glib::spawn_future_local(async move {
                let message = state.terminate_foreground().await;
                if let Some(message) = message {
                    state.stop_agent_background(message).await;
                }
                state.terminating.set(false);
            });
        }
    }

    /// Kills Claude Code and disconnects it from this window until it is closed.
    pub(super) fn hard_stop(self: &Rc<Self>) {
        if self.hard_stopped.replace(true) {
            return;
        }
        if let Some(server) = self.mcp_server.take() {
            server.disconnect();
        }
        let _ = std::fs::remove_file(&self.mcp_config);
        self.restart_agent.set(false);
        if let Some(pid) = self.agent_pid.take() {
            // Claude Code leads its own process group; SIGKILL can't be caught or ignored.
            unsafe {
                libc::kill(-pid.0, libc::SIGKILL);
                libc::kill(pid.0, libc::SIGKILL);
            }
        }
        self.agent_button.set_sensitive(false);
        self.agent_button.set_tooltip_text(Some("Disconnected by Hard Stop. Open a new window to use the agent."));
        self.stopped_banner.set_title(
            "Hard stop: the agent is disconnected from this terminal. Open a new window to use it again.",
        );
        self.stopped_banner.set_button_label(None);
        self.stopped_banner.set_revealed(true);
        self.update_stop_buttons();
        self.audit("hard_stop", "", "agent killed and disconnected", None);
        self.toast("Hard stop: Claude Code was killed and disconnected from this terminal");
    }

    pub(super) fn resume_agent(&self) {
        if self.hard_stopped.get() {
            return;
        }
        self.stopped.set(false);
        self.stopped_banner.set_revealed(false);
        self.audit("resume", "", "resumed by user", None);
        self.toast("Agent resumed. Automatic mode is off, so Claude asks before typing.");
    }

    /// Terminates the terminal's foreground job: Ctrl-C, then SIGTERM, then SIGKILL. Returns
    /// what happened, or `None` if survivors were reported instead.
    async fn terminate_foreground(self: &Rc<Self>) -> Option<String> {
        let (Some(shell), Some(pty)) = (self.shell_pid.get(), self.terminal.pty()) else { return None };
        let pgrp = unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) };
        if pgrp <= 0 || pgrp == shell.0 {
            // At the prompt: Ctrl-C clears anything the agent left half-typed.
            self.terminal.feed_child(b"\x03");
            return Some("Emergency stop: the agent is stopped. Nothing was running in the foreground.".into());
        }
        let command = procs::in_group(pgrp)
            .into_iter()
            .min_by_key(|p| p.pid)
            .map_or_else(|| "the running command".to_owned(), |p| p.command);

        // Ctrl-C through the terminal: the kernel signals the whole foreground job, including
        // root-owned processes Bishop can't signal itself, and SSH forwards it to the remote host.
        self.terminal.feed_child(b"\x03");
        if is_remote_client(&command) {
            if self.remote_prompt_returned(REMOTE_GRACE).await {
                return Some(format!("Emergency stop: stopped the remote command in `{command}` with Ctrl-C"));
            }
        } else if group_gone(pgrp, CTRL_C_GRACE).await {
            return Some(format!("Emergency stop: `{command}` was stopped with Ctrl-C"));
        }

        // SIGTERM to the process group (SIGCONT too, in case the job is suspended), and Ctrl-\
        // (SIGQUIT) through the terminal for processes Bishop may not signal directly.
        unsafe {
            libc::kill(-pgrp, libc::SIGTERM);
            libc::kill(-pgrp, libc::SIGCONT);
        }
        self.terminal.feed_child(b"\x1c");
        if group_gone(pgrp, TERM_GRACE).await {
            return Some(format!("Emergency stop: `{command}` was terminated"));
        }

        // SIGKILL can't be caught, blocked, or ignored.
        unsafe { libc::kill(-pgrp, libc::SIGKILL) };
        if group_gone(pgrp, KILL_GRACE).await {
            return Some(if is_remote_client(&command) {
                format!(
                    "Emergency stop: closed the connection `{command}` because the remote command \
                     ignored Ctrl-C. The remote host sends its processes a hangup signal."
                )
            } else {
                format!("Emergency stop: `{command}` was force-killed")
            });
        }
        let survivors = procs::in_group(pgrp);
        if survivors.is_empty() {
            return Some(format!("Emergency stop: `{command}` was force-killed"));
        }
        self.show_survivors(&survivors);
        None
    }

    /// Kills background processes the agent started (they'd otherwise outlive the stop), then
    /// reports. Background jobs the user started are left alone, with an offer to stop them.
    async fn stop_agent_background(self: &Rc<Self>, message: String) {
        let Some(shell) = self.shell_pid.get().map(|pid| pid.0) else { return };
        let agents: Vec<Process> = procs::alongside(shell)
            .into_iter()
            .filter(|p| self.started_by_agent(p))
            .collect();
        let mut message = message;
        if !agents.is_empty() {
            let survivors = kill_escalating(&agents).await;
            if !survivors.is_empty() {
                self.show_survivors(&survivors);
                return;
            }
            message = format!(
                "{message}. Also stopped {} background process{} the agent started.",
                agents.len(),
                if agents.len() == 1 { "" } else { "es" }
            );
        }
        self.finish_stop(&message, shell);
    }

    /// Over SSH the local foreground process is the client; the remote command is done when
    /// output settles on something that looks like a prompt.
    async fn remote_prompt_returned(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < timeout {
            glib::timeout_future(Duration::from_millis(100)).await;
            if self.last_output.get().elapsed() >= Duration::from_millis(400)
                && super::agent::looks_like_prompt(&self.cursor_line())
            {
                return true;
            }
        }
        false
    }

    /// Reports the outcome, offering to stop any background jobs left in the session.
    fn finish_stop(self: &Rc<Self>, message: &str, shell: i32) {
        let background: Vec<Process> = procs::alongside(shell);
        self.audit("emergency_stop", message, "command stopped", None);
        let message = message.trim_end_matches('.');
        let toast = adw::Toast::builder().title(message).timeout(8).build();
        if !background.is_empty() {
            toast.set_title(&format!(
                "{message}. {} background process{} still running in this terminal.",
                background.len(),
                if background.len() == 1 { " is" } else { "es are" }
            ));
            toast.set_button_label(Some("Stop Them"));
            let state = Rc::downgrade(self);
            toast.connect_button_clicked(move |_| {
                if let Some(state) = state.upgrade() {
                    glib::spawn_future_local(async move { state.stop_background(shell).await });
                }
            });
        }
        self.toasts.add_toast(toast);
    }

    async fn stop_background(self: &Rc<Self>, shell: i32) {
        let background: Vec<Process> = procs::alongside(shell);
        let survivors = kill_escalating(&background).await;
        if survivors.is_empty() {
            self.toast("Background processes stopped");
        } else {
            self.show_survivors(&survivors);
        }
    }

    /// Something outlived SIGKILL: either it belongs to another user (root, via sudo), or
    /// it's stuck in the kernel. Say so plainly, and offer administrator rights for the first.
    fn show_survivors(self: &Rc<Self>, survivors: &[Process]) {
        let me = unsafe { libc::getuid() };
        let foreign = survivors.iter().any(|p| p.uid != me);
        let list: String = survivors
            .iter()
            .take(8)
            .map(|p| {
                let owner = if p.uid == 0 { "root".to_owned() } else { format!("uid {}", p.uid) };
                format!("\n• {} ({owner}, pid {})", p.command, p.pid)
            })
            .collect();
        let reason = if foreign {
            "These belong to another user (usually root, through sudo), so your account isn't \
             allowed to stop them. They have already been sent Ctrl-C through the terminal."
        } else {
            "They ignored SIGKILL, which only happens while a process is stuck inside the kernel (for \
             example waiting on a hung network drive). They will exit as soon as that wait ends."
        };
        self.audit("emergency_stop", &list, "processes survived", None);

        let dialog = adw::AlertDialog::builder()
            .heading("The command is still running")
            .body(format!("The agent is stopped, but these processes are still running:{list}\n\n{reason}"))
            .build();
        dialog.add_response("close", "Close");
        if foreign {
            dialog.add_response("force", "Force Stop as Administrator");
            dialog.set_response_appearance("force", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("force"));
        }
        let pids: Vec<i32> = survivors.iter().map(|p| p.pid).collect();
        let state = Rc::downgrade(self);
        dialog.connect_response(Some("force"), move |_, _| {
            if let Some(state) = state.upgrade() {
                let pids = pids.clone();
                glib::spawn_future_local(async move { state.force_kill_as_admin(&pids).await });
            }
        });
        dialog.present(Some(&self.window));
    }

    /// SIGKILLs processes owned by other users, after the system asks for an administrator
    /// password (polkit, via pkexec).
    async fn force_kill_as_admin(self: &Rc<Self>, pids: &[i32]) {
        let Some(kill) = glib::find_program_in_path("kill") else {
            self.toast("Could not find the `kill` program");
            return;
        };
        let mut argv = vec!["pkexec".to_owned(), kill.to_string_lossy().into_owned(), "-KILL".into(), "--".into()];
        argv.extend(pids.iter().map(i32::to_string));
        let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
        let result = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
            Ok(process) => process.wait_future().await.map(|()| process.exit_status()),
            Err(e) => Err(e),
        };
        glib::timeout_future(Duration::from_millis(300)).await;
        let alive: Vec<i32> = pids.iter().copied().filter(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists()).collect();
        let message = match result {
            _ if alive.is_empty() => "The remaining processes were force-killed",
            Ok(126) | Ok(127) => "Administrator authentication was cancelled; the processes are still running",
            _ => "Some processes are still running; they're stuck in the kernel and will exit when it returns",
        };
        self.audit("emergency_stop", &format!("{pids:?}"), message, None);
        self.toast(message);
    }
}

/// SIGTERM, then SIGKILL, each process; returns any still alive afterwards.
async fn kill_escalating(processes: &[Process]) -> Vec<Process> {
    let alive = |processes: &[Process]| -> Vec<Process> {
        let live = procs::all();
        processes.iter().filter(|p| live.iter().any(|l| l.pid == p.pid && l.start == p.start)).cloned().collect()
    };
    for p in processes {
        unsafe {
            libc::kill(p.pid, libc::SIGTERM);
            libc::kill(p.pid, libc::SIGCONT);
        }
    }
    let started = Instant::now();
    while started.elapsed() < TERM_GRACE && !alive(processes).is_empty() {
        glib::timeout_future(Duration::from_millis(50)).await;
    }
    for p in alive(processes) {
        unsafe { libc::kill(p.pid, libc::SIGKILL) };
    }
    let started = Instant::now();
    while started.elapsed() < KILL_GRACE && !alive(processes).is_empty() {
        glib::timeout_future(Duration::from_millis(50)).await;
    }
    alive(processes)
}

fn is_remote_client(command: &str) -> bool {
    let program = command.split_whitespace().next().unwrap_or_default();
    let name = program.rsplit('/').next().unwrap_or_default();
    REMOTE_CLIENTS.contains(&name)
}

/// Waits up to `timeout` for every process in group `pgrp` to exit.
async fn group_gone(pgrp: i32, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        if procs::in_group(pgrp).is_empty() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        glib::timeout_future(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::is_remote_client;

    #[test]
    fn recognizes_remote_clients() {
        assert!(is_remote_client("ssh web01"));
        assert!(is_remote_client("/usr/bin/ssh -p 2222 admin@db1"));
        assert!(is_remote_client("mosh-client -# web01"));
        assert!(!is_remote_client("sshd: kim"));
        assert!(!is_remote_client("sleep 100"));
    }
}
