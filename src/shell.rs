//! Bishop's own shell integration, so `run_command` knows exactly when a command finished and
//! its exit status on every distro, whatever VTE version it has and however its shells are set
//! up.
//!
//! Each tab's shell is started with a small hook for bash, zsh or fish. After every command it
//! writes `precmd <status>` to the tab's events FIFO (`$BISHOP_EVENTS`); Bishop reads it on the
//! main loop. Other shells, and remote shells over SSH, get no hook; `run_command` then falls back
//! to waiting for output to go quiet.

use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use gtk::glib;

const BASH_RC: &str = r#"# Bishop shell integration for bash. Bishop starts bash with `--rcfile` pointing here, in
# place of ~/.bashrc, which is loaded first exactly as usual.
[ -r ~/.bashrc ] && . ~/.bashrc

if [ -n "$BISHOP_EVENTS" ] && [ -z "$__bishop_loaded" ]; then
    __bishop_loaded=1
    # Keep the channel to this shell: programs it runs don't inherit it, so they can't fake events.
    export -n BISHOP_EVENTS
    __bishop_precmd() {
        local __bishop_status=$?
        [ -p "$BISHOP_EVENTS" ] && printf 'precmd %d\n' "$__bishop_status" >> "$BISHOP_EVENTS" 2>/dev/null
        return $__bishop_status
    }
    # First, so it sees the command's exit status (and passes it on unchanged).
    if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
        PROMPT_COMMAND=(__bishop_precmd "${PROMPT_COMMAND[@]}")
    else
        PROMPT_COMMAND="__bishop_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
    fi
fi
"#;

const ZSH_ENV: &str = r#"# Bishop shell integration for zsh. Bishop points ZDOTDIR here so zsh reads this first; it
# puts ZDOTDIR back and loads the user's own .zshenv, and zsh then reads the rest as usual.
if [[ -n ${BISHOP_ZDOTDIR+x} ]]; then
    ZDOTDIR=$BISHOP_ZDOTDIR
else
    unset ZDOTDIR
fi
unset BISHOP_ZDOTDIR
[[ -r ${ZDOTDIR:-$HOME}/.zshenv ]] && source ${ZDOTDIR:-$HOME}/.zshenv

if [[ -o interactive && -n $BISHOP_EVENTS ]]; then
    # Keep the channel to this shell: programs it runs don't inherit it, so they can't fake events.
    typeset +x BISHOP_EVENTS
    __bishop_precmd() {
        local __bishop_status=$?
        [[ -p $BISHOP_EVENTS ]] && print -r -- "precmd $__bishop_status" >> $BISHOP_EVENTS 2>/dev/null
        return $__bishop_status
    }
    autoload -Uz add-zsh-hook
    add-zsh-hook precmd __bishop_precmd
fi
"#;

const FISH_INIT: &str = r#"# Bishop shell integration for fish, loaded with `--init-command` after the usual config.
if set -q BISHOP_EVENTS; and status is-interactive
    # Keep the channel to this shell: programs it runs don't inherit it, so they can't fake events.
    set -gu BISHOP_EVENTS $BISHOP_EVENTS
    function __bishop_postexec --on-event fish_postexec
        set -l last $status
        test -p "$BISHOP_EVENTS"; and echo "precmd $last" >> "$BISHOP_EVENTS" 2>/dev/null
    end
    # fish_postexec only follows a command, so say the hook is here before the first one.
    test -p "$BISHOP_EVENTS"; and echo ready >> "$BISHOP_EVENTS" 2>/dev/null
end
"#;

/// Where the hook files live: `$XDG_RUNTIME_DIR/bishop/shell`, rewritten at startup.
fn hooks_dir() -> PathBuf {
    crate::mcp::runtime_dir().join("shell")
}

/// Writes the hook files. Called once at startup.
pub fn install_hooks() -> std::io::Result<()> {
    let dir = hooks_dir();
    fs::create_dir_all(dir.join("zsh"))?;
    fs::write(dir.join("bishop.bash"), BASH_RC)?;
    fs::write(dir.join("zsh").join(".zshenv"), ZSH_ENV)?;
    fs::write(dir.join("bishop.fish"), FISH_INIT)?;
    Ok(())
}

/// The shell command line and extra environment to start `argv` with Bishop's hook, writing
/// events to `events`. Shells Bishop has no hook for are started unchanged.
pub fn with_hooks(argv: &[String], events: &Path) -> (Vec<String>, Vec<String>) {
    let mut argv = argv.to_vec();
    let mut env = vec![format!("BISHOP_EVENTS={}", events.display())];
    let dir = hooks_dir();
    let name = argv.first().and_then(|a| Path::new(a).file_name()).and_then(|n| n.to_str()).unwrap_or("");
    match name {
        // `bash -l` and the like read other startup files; leave those alone.
        "bash" if argv.len() == 1 => {
            argv.extend(["--rcfile".to_owned(), dir.join("bishop.bash").display().to_string()]);
        }
        "zsh" => {
            if let Ok(original) = std::env::var("ZDOTDIR") {
                env.push(format!("BISHOP_ZDOTDIR={original}"));
            }
            env.push(format!("ZDOTDIR={}", dir.join("zsh").display()));
        }
        "fish" => {
            let path = dir.join("bishop.fish").display().to_string().replace('\'', "\\'");
            argv.extend(["--init-command".to_owned(), format!("source '{path}'")]);
        }
        _ => env.clear(),
    }
    (argv, env)
}

/// The user's shell: Bishop's setting, else `$SHELL`, else the login shell from the user
/// database, else `/bin/sh`.
pub fn user_shell(setting: &str) -> Vec<String> {
    let command = Some(setting.trim().to_owned())
        .filter(|shell| !shell.is_empty())
        .or_else(|| std::env::var("SHELL").ok().filter(|shell| !shell.is_empty()))
        .or_else(login_shell)
        .unwrap_or_else(|| "/bin/sh".into());
    command.split_whitespace().map(str::to_owned).collect()
}

fn login_shell() -> Option<String> {
    let entry = unsafe { libc::getpwuid(libc::getuid()) };
    if entry.is_null() {
        return None;
    }
    let shell = unsafe { std::ffi::CStr::from_ptr((*entry).pw_shell) };
    Some(shell.to_str().ok()?.to_owned()).filter(|s| !s.is_empty())
}

/// What a hook reported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// The hook is loaded (sent by shells whose hook doesn't run before the first prompt).
    Ready,
    /// A command finished with this exit status; the prompt comes next.
    Precmd(Option<u64>),
}

fn parse(line: &str) -> Option<Event> {
    let mut words = line.split_whitespace();
    match words.next()? {
        "ready" => Some(Event::Ready),
        "precmd" => Some(Event::Precmd(words.next().and_then(|s| s.parse().ok()))),
        _ => None,
    }
}

/// A tab's events FIFO, read for as long as this lives.
pub struct Events {
    path: PathBuf,
    /// Our own write end, to wake the reader thread when the tab closes.
    writer: fs::File,
}

/// Events are a few bytes; longer lines are ignored.
const MAX_LINE: u64 = 256;

/// Written by `Events` itself to stop its reader thread.
const QUIT: &[u8] = b"bishop-quit\n";

impl Events {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the FIFO at `path` and calls `on_event` on the main loop for each event written
    /// to it.
    pub fn listen(path: PathBuf, on_event: impl Fn(Event) + 'static) -> std::io::Result<Self> {
        let _ = fs::remove_file(&path);
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Read-write, so there is always a writer: reads block between commands instead of
        // seeing end-of-file, and opening never blocks.
        let reader = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let writer = reader.try_clone()?;

        let (tx, rx) = async_channel::unbounded::<Event>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let mut line = Vec::new();
                match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                // Over-long lines (only something misbehaving writes those) are skipped whole.
                if line.last() != Some(&b'\n') {
                    loop {
                        let mut rest = Vec::new();
                        match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut rest) {
                            Ok(0) | Err(_) => break,
                            Ok(_) if rest.last() == Some(&b'\n') => break,
                            Ok(_) => {}
                        }
                    }
                    continue;
                }
                line.pop();
                if line == QUIT[..QUIT.len() - 1] {
                    break;
                }
                if let Some(event) = parse(&String::from_utf8_lossy(&line))
                    && tx.send_blocking(event).is_err()
                {
                    break;
                }
            }
        });
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                on_event(event);
            }
        });
        Ok(Self { path, writer })
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = self.writer.write_all(QUIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events() {
        assert_eq!(parse("precmd 0\n"), Some(Event::Precmd(Some(0))));
        assert_eq!(parse("precmd 130"), Some(Event::Precmd(Some(130))));
        assert_eq!(parse("precmd"), Some(Event::Precmd(None)));
        assert_eq!(parse("ready"), Some(Event::Ready));
        assert_eq!(parse("bogus 1"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn hooks_known_shells_only() {
        let events = Path::new("/run/x.events");
        let (argv, env) = with_hooks(&["/bin/bash".into()], events);
        assert_eq!(argv[1], "--rcfile");
        assert_eq!(env, vec!["BISHOP_EVENTS=/run/x.events".to_owned()]);
        // A login shell reads other files; it's left alone (but still told where to write).
        let (argv, _) = with_hooks(&["/bin/bash".into(), "-l".into()], events);
        assert_eq!(argv.len(), 2);
        let (_, env) = with_hooks(&["/usr/bin/zsh".into()], events);
        assert!(env.iter().any(|e| e.starts_with("ZDOTDIR=")));
        let (argv, _) = with_hooks(&["fish".into()], events);
        assert_eq!(argv[1], "--init-command");
        let (argv, env) = with_hooks(&["/bin/dash".into()], events);
        assert_eq!((argv.len(), env.len()), (1, 0));
    }
}
