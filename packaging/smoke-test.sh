#!/bin/sh
# Smoke test for an installed Bishop: launches it on a virtual X display, then runs a command
# through a tab's agent socket and checks the output and exit code, which exercises GTK, VTE,
# the MCP server and the shell integration end to end.
#
# Needs: bishop on PATH, Xvfb, dbus-run-session, bash. Run as any user.
set -eu

fail() { echo "SMOKE TEST FAILED: $*" >&2; [ -f "$work/log" ] && sed 's/^/  bishop: /' "$work/log" >&2; exit 1; }

work=$(mktemp -d)
xvfb= app= socket=
cleanup() {
    # Bishop's PID is the first part of its socket's name.
    pid=$(basename "${socket:-x}" | cut -d- -f1)
    for p in $pid $app $xvfb; do kill "$p" 2>/dev/null || true; done
    sleep 1
    rm -rf "$work" 2>/dev/null || true
}
trap cleanup EXIT

bishop --version || fail "bishop --version"

export XDG_RUNTIME_DIR="$work/run" XDG_CONFIG_HOME="$work/config" XDG_STATE_HOME="$work/state"
export SHELL=/bin/bash HOME="$work/home"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/bishop" "$HOME"
chmod 700 "$XDG_RUNTIME_DIR"
printf '[agent]\nautomatic_mode = true\n' > "$XDG_CONFIG_HOME/bishop/config.toml"

Xvfb :87 -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &
xvfb=$!
export DISPLAY=:87 GDK_BACKEND=x11 GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 GTK_A11Y=none
sleep 1

# The test client is not Claude Code, so let it connect (see BISHOP_TEST_ANY_MCP_CLIENT in src/mcp.rs).
BISHOP_TEST_ANY_MCP_CLIENT=1 dbus-run-session -- bishop >"$work/log" 2>&1 &
app=$!

for _ in $(seq 1 60); do
    socket=$(ls "$XDG_RUNTIME_DIR"/bishop/*.sock 2>/dev/null | head -n 1 || true)
    [ -n "$socket" ] && break
    kill -0 $app 2>/dev/null || fail "bishop exited during startup"
    sleep 0.5
done
[ -n "$socket" ] || fail "no agent socket after 30s"
sleep 2  # let the shell reach its first prompt

reply=$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run_command","arguments":{"command":"echo smoke-$((6*7)); (exit 3)"}}}' \
    | timeout 60 bishop mcp-bridge "$socket") || fail "mcp-bridge"

echo "$reply" | grep -q '"serverInfo"' || fail "no initialize reply: $reply"
echo "$reply" | grep -q 'smoke-42' || fail "command output missing: $reply"
echo "$reply" | grep -q 'Exit code: 3' || fail "exit code missing (shell integration): $reply"
echo "SMOKE TEST PASSED: $(bishop --version)"
