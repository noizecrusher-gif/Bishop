//! Minimal MCP server (newline-delimited JSON-RPC) on a per-tab unix socket.
//!
//! Socket threads never touch GTK: tool calls are forwarded to the window's main-thread
//! handler as `ToolRequest`s and the answer comes back over a std channel.

use std::fs::{self, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use gtk::glib;
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "Bishop is the user's terminal emulator. These tools read and type \
into the user's live Bishop terminal session, exactly as the user would. When the user refers to \
their terminal, screen, shell, or session, they mean this one: call get_session_info and \
read_screen to see it before answering or acting. The session may be connected over SSH to another \
host. Never type passwords; ask the user to enter them.";

// Long enough for the user to read and approve a `send_keys` request.
const TOOL_TIMEOUT: Duration = Duration::from_secs(600);

pub struct ToolRequest {
    pub name: String,
    pub args: Value,
    pub reply: mpsc::Sender<Result<String, String>>,
}

/// `$XDG_RUNTIME_DIR/bishop`, private to the user.
pub fn runtime_dir() -> PathBuf {
    let dir = glib::user_runtime_dir().join("bishop");
    let _ = fs::create_dir_all(&dir);
    let _ = fs::set_permissions(&dir, Permissions::from_mode(0o700));
    remove_stale_files(&dir);
    dir
}

/// Files are named `<pid>-<window>-<tab>.*`; drop those left behind by processes that died
/// without closing their windows.
fn remove_stale_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.split('-').next()) else { continue };
        if pid.parse::<u32>().is_ok() && !Path::new("/proc").join(pid).exists() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Longest request line accepted; a client sending more is disconnected.
const MAX_LINE: u64 = 1024 * 1024;
/// Requests one connection may have in flight at once; reading pauses beyond this.
const MAX_IN_FLIGHT: usize = 32;
/// How long a connecting client may wait for Bishop to learn Claude's PID after starting it.
const AGENT_PID_WAIT: Duration = Duration::from_secs(5);
/// Set in Bishop's environment (only by the packaging smoke test) to accept clients that
/// aren't descended from the tab's Claude Code.
const ANY_CLIENT_FOR_TESTS: &str = "BISHOP_TEST_ANY_MCP_CLIENT";

/// A running server. `disconnect` cuts the agent off for good.
///
/// Only the tab's own agent may connect: the peer (from `SO_PEERCRED`) must run as this user
/// and descend from the Claude Code process Bishop started for the tab. Other processes of
/// the same user, even ones the kernel's ptrace protection keeps away from this terminal,
/// can't use the socket to read the screen or type.
pub struct Server {
    path: PathBuf,
    tx: async_channel::Sender<ToolRequest>,
    disconnected: Arc<AtomicBool>,
    connections: Connections,
    agent: Arc<AtomicI32>,
}

/// Open connections by id, so Hard Stop can cut them off.
type Connections = Arc<Mutex<Vec<(u64, UnixStream)>>>;

impl Server {
    /// The PID of the Claude Code process the tab started; only its descendants may connect.
    pub fn set_agent(&self, pid: i32) {
        self.agent.store(pid, Ordering::SeqCst);
    }

    /// Refuses all further requests, closes every open connection, and removes the socket so
    /// nothing can reconnect.
    pub fn disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        self.tx.close();
        if let Ok(connections) = self.connections.lock() {
            for (_, conn) in connections.iter() {
                let _ = conn.shutdown(Shutdown::Both);
            }
        }
        // Wake the accept loop so it sees the flag and drops the listener.
        let _ = UnixStream::connect(&self.path);
        let _ = fs::remove_file(&self.path);
    }
}

pub fn serve(path: &Path, tx: async_channel::Sender<ToolRequest>) -> std::io::Result<Server> {
    let _ = fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    let server = Server {
        path: path.to_owned(),
        tx: tx.clone(),
        disconnected: Arc::new(AtomicBool::new(false)),
        connections: Arc::new(Mutex::new(Vec::new())),
        agent: Arc::new(AtomicI32::new(0)),
    };
    let (disconnected, connections, agent) =
        (server.disconnected.clone(), server.connections.clone(), server.agent.clone());
    let any_client = std::env::var_os(ANY_CLIENT_FOR_TESTS).is_some();
    std::thread::spawn(move || {
        for (id, conn) in (0u64..).zip(listener.incoming().flatten()) {
            if disconnected.load(Ordering::SeqCst) {
                break;
            }
            if let (Ok(clone), Ok(mut list)) = (conn.try_clone(), connections.lock()) {
                list.push((id, clone));
            }
            let (tx, disconnected, connections, agent) =
                (tx.clone(), disconnected.clone(), connections.clone(), agent.clone());
            std::thread::spawn(move || {
                if any_client || is_agent(&conn, &agent) {
                    handle_connection(conn, tx, disconnected);
                } else {
                    let _ = conn.shutdown(Shutdown::Both);
                }
                if let Ok(mut list) = connections.lock() {
                    list.retain(|(other, _)| *other != id);
                }
            });
        }
    });
    Ok(server)
}

fn handle_connection(conn: UnixStream, tx: async_channel::Sender<ToolRequest>, disconnected: Arc<AtomicBool>) {
    let Ok(out) = conn.try_clone() else { return };
    let out = Arc::new(Mutex::new(out));
    let mut requests: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut reader = BufReader::new(conn);
    loop {
        let mut raw = Vec::new();
        match (&mut reader).take(MAX_LINE + 1).read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) if raw.len() as u64 > MAX_LINE => break,
            Ok(_) => {}
        }
        if disconnected.load(Ordering::SeqCst) {
            break;
        }
        let Ok(line) = String::from_utf8(raw) else { continue };
        if line.trim().is_empty() {
            continue;
        }
        // One thread per request: a send_keys awaiting approval must not block reads.
        let (tx, out) = (tx.clone(), out.clone());
        requests.retain(|request| !request.is_finished());
        while requests.len() >= MAX_IN_FLIGHT {
            let _ = requests.remove(0).join();
        }
        requests.push(std::thread::spawn(move || {
            if let Some(response) = handle_message(&line, &tx)
                && let Ok(mut out) = out.lock()
            {
                let _ = writeln!(out, "{response}");
            }
        }));
    }
    // The client is done sending: answer what it already asked, then hang up.
    for request in requests {
        let _ = request.join();
    }
    if let Ok(out) = out.lock() {
        let _ = out.shutdown(Shutdown::Both);
    }
}

/// Whether the connecting process runs as this user and descends from the tab's Claude Code.
fn is_agent(conn: &UnixStream, agent: &AtomicI32) -> bool {
    let Some((pid, uid)) = peer(conn) else { return false };
    if uid != unsafe { libc::getuid() } {
        return false;
    }
    // Claude can start the bridge before Bishop has recorded its PID.
    let deadline = std::time::Instant::now() + AGENT_PID_WAIT;
    loop {
        let agent = agent.load(Ordering::SeqCst);
        if agent > 0 {
            return crate::procs::descends_from(pid, agent);
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The connecting process's PID and user, as the kernel recorded them at `connect`.
fn peer(conn: &UnixStream) -> Option<(i32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    } == 0;
    (ok && cred.pid > 0).then_some((cred.pid, cred.uid))
}

fn handle_message(line: &str, tx: &async_channel::Sender<ToolRequest>) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(msg) => msg,
        Err(e) => {
            return Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": e.to_string() },
            }));
        }
    };
    // Notifications carry no id and get no response.
    let id = msg.get("id")?.clone();
    let method = msg["method"].as_str().unwrap_or_default();
    let params = &msg["params"];

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "bishop", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => Ok(call_tool(params, tx)),
        _ => Err(json!({ "code": -32601, "message": format!("method not found: {method}") })),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    })
}

fn call_tool(params: &Value, tx: &async_channel::Sender<ToolRequest>) -> Value {
    let (reply, rx) = mpsc::channel();
    let request = ToolRequest {
        name: params["name"].as_str().unwrap_or_default().to_owned(),
        args: params.get("arguments").cloned().unwrap_or_else(|| json!({})),
        reply,
    };
    let outcome = if tx.send_blocking(request).is_err() {
        Err("the Bishop tab for this session is closed".to_owned())
    } else {
        rx.recv_timeout(TOOL_TIMEOUT)
            .unwrap_or_else(|_| Err("timed out waiting for the terminal".to_owned()))
    };
    match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "read_screen",
            "description": "Read the currently visible screen of the user's terminal as plain text, \
                            including cursor position. Use this for full-screen programs (vim, htop, less) \
                            and to see the current prompt.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "read_scrollback",
            "description": "Read the last N lines of the user's terminal, including scrollback history.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lines": { "type": "integer", "minimum": 1, "maximum": 5000, "default": 200 },
                },
            },
        },
        {
            "name": "get_session_info",
            "description": "Get details about the user's terminal session: title, working directory, \
                            foreground process (e.g. `ssh host` when connected remotely), size, and whether \
                            a password prompt appears to be active.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "run_command",
            "description": "Type a shell command into the user's terminal, press Enter, and wait for it \
                            to finish. Returns the exit code and output when the user's local shell \
                            reports it; over SSH or other shells without integration, it waits for \
                            output to go quiet and a prompt to appear, and the exit code is unknown. \
                            Prefer this over send_keys for running commands. One command per call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 600000, "default": 30000 },
                },
                "required": ["command"],
            },
        },
        {
            "name": "wait_for",
            "description": "Wait until new terminal output (from the current line onward) matches a \
                            regular expression, e.g. a log line, `Done`, or a prompt. Returns the screen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 100, "maximum": 600000, "default": 30000 },
                },
                "required": ["pattern"],
            },
        },
        {
            "name": "send_keys",
            "description": "Type into the user's terminal exactly as if the user typed it: for \
                            interactive programs, prompts, and full-screen apps. `text` is typed first, \
                            then each named key in `keys`. Returns the screen after `wait_ms`. Key names: \
                            enter, tab, escape, backspace, delete, space, up, down, left, right, home, end, \
                            pageup, pagedown, ctrl-a .. ctrl-z.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "keys": { "type": "array", "items": { "type": "string" } },
                    "wait_ms": { "type": "integer", "minimum": 0, "maximum": 10000, "default": 500 },
                },
            },
        },
    ])
}
