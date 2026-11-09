//! Reading the process table from /proc, for Emergency Stop.

use std::fs;

#[derive(Clone, Debug, PartialEq)]
pub struct Process {
    pub pid: i32,
    pub pgrp: i32,
    pub session: i32,
    /// Real user id.
    pub uid: u32,
    /// When it started, in clock ticks since boot (comparable with `now_ticks`).
    pub start: u64,
    pub command: String,
}

/// Every live (non-zombie) process this user can see.
pub fn all() -> Vec<Process> {
    let Ok(entries) = fs::read_dir("/proc") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(read)
        .collect()
}

pub fn in_group(pgrp: i32) -> Vec<Process> {
    all().into_iter().filter(|p| p.pgrp == pgrp).collect()
}

/// The cgroup a process belongs to. VTE starts each shell in its own systemd scope, and
/// everything started from it stays there, even after `setsid` or daemonizing.
pub fn cgroup(pid: i32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    text.lines().find_map(|line| line.strip_prefix("0::")).map(str::to_owned)
}

/// Every process in the same cgroup as `pid`, except `pid` itself, when that cgroup is a
/// scope of its own (VTE starts each shell in a `vte-spawn-*.scope` under systemd). Otherwise
/// (no systemd, or the scope couldn't be made, leaving the shell in Bishop's own cgroup or the
/// root one) the cgroup holds unrelated processes, so fall back to the shell's session.
pub fn alongside(pid: i32) -> Vec<Process> {
    let own = cgroup(std::process::id() as i32);
    let scope = cgroup(pid).filter(|scope| is_private_scope(scope, own.as_deref()));
    all()
        .into_iter()
        .filter(|p| p.pid != pid)
        .filter(|p| match &scope {
            Some(scope) => cgroup(p.pid).as_ref() == Some(scope),
            None => p.session == pid,
        })
        .collect()
}

fn is_private_scope(scope: &str, bishop: Option<&str>) -> bool {
    let name = scope.rsplit('/').next().unwrap_or_default();
    name.starts_with("vte-spawn-") && name.ends_with(".scope") && Some(scope) != bishop
}

fn read(pid: i32) -> Option<Process> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, pgrp, session, start) = parse_stat(&stat)?;
    if state == 'Z' || state == 'X' {
        return None;
    }
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let command = fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| String::from_utf8_lossy(&raw).trim_end_matches('\0').replace('\0', " "))
        .filter(|cmd| !cmd.is_empty())
        .or_else(|| comm(&stat))?;
    Some(Process { pid, pgrp, session, uid, start, command })
}

/// The parent of `pid`, from `/proc/<pid>/stat`.
pub fn parent(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// Whether `pid` is `ancestor` or one of its descendants.
pub fn descends_from(mut pid: i32, ancestor: i32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        match parent(pid) {
            Some(next) if next > 0 && next != pid => pid = next,
            _ => return false,
        }
    }
    false
}

/// The current time in the clock ticks `/proc` uses for process start times.
pub fn now_ticks() -> u64 {
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    (uptime * hz) as u64
}

/// State, process group, session and start time from `/proc/<pid>/stat`. The command name
/// in parentheses may itself contain spaces and parentheses, so parse from the last `)`.
fn parse_stat(stat: &str) -> Option<(char, i32, i32, u64)> {
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let pgrp = fields.get(2)?.parse().ok()?;
    let session = fields.get(3)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((state, pgrp, session, start))
}

fn comm(stat: &str) -> Option<String> {
    Some(stat[stat.find('(')? + 1..stat.rfind(')')?].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_awkward_names() {
        let stat = "4321 (my (odd) prog) S 100 4321 99 34816 4321 4194560 0 0 0 0 1 2 0 0 20 0 1 0 987654 1000 200";
        assert_eq!(parse_stat(stat), Some(('S', 4321, 99, 987654)));
        assert_eq!(comm(stat).as_deref(), Some("my (odd) prog"));
    }

    #[test]
    fn trusts_only_per_terminal_scopes() {
        let vte = "/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-1f2e.scope";
        let app = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-bishop-42.scope";
        assert!(is_private_scope(vte, Some(app)));
        assert!(!is_private_scope("/", Some("/")));
        assert!(!is_private_scope(app, Some(app)));
        assert!(!is_private_scope(vte, Some(vte)));
        assert!(!is_private_scope("/user.slice/session-2.scope", None));
    }

    #[test]
    fn finds_this_process() {
        let me = std::process::id() as i32;
        let found = all().into_iter().find(|p| p.pid == me).expect("own process listed");
        assert_eq!(found.uid, unsafe { libc::getuid() });
        assert!(in_group(found.pgrp).iter().any(|p| p.pid == me));
        assert!(found.start > 0 && found.start <= now_ticks());
        let parent = parent(me).expect("has a parent");
        assert!(descends_from(me, parent) && descends_from(me, me));
        assert!(!descends_from(parent, me));
    }
}
