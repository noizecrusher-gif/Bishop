# Bishop

A Linux terminal with a built-in AI assistant. Claude Code runs in a side panel and works in your
terminal session the way you would: it reads what's on screen and types commands, using your shell,
your environment, and whatever host you're connected to over SSH. It has no access of its own.

## Features

- **Terminal:** GTK4/libadwaita with VTE. Themes that include colors and font, saved custom themes,
  full 16-color palette editing, background opacity, zoom, full screen, multiple windows.
- **Agent panel:** Claude Code in a resizable side panel, connected to the terminal through a
  built-in MCP server. It can read the screen and scrollback, run commands (with exit codes, using
  shell integration), wait for output, and send keys.
- **Approvals:** with automatic mode off, you approve everything the agent types. Destructive
  commands (`rm -rf`, `dd`, `mkfs`, `reboot`, `curl | sh`, …) always need approval.
- **Guardrails:** the agent can't type at password prompts, your keypress takes control back, and
  every action is recorded in `~/.local/state/bishop/audit.log`.
- **Emergency Stop:** stops the agent and terminates what it's running (Ctrl-C, then SIGTERM, then
  SIGKILL, including background processes it started). Three quick clicks is a Hard Stop, which
  kills the agent and disconnects it from the window.

## Install (Ubuntu)

Download the `.deb` from the releases page, or build it (below), then:

```sh
sudo apt install ./bishop_0.10.0-1_amd64.deb
```

The agent needs [Claude Code](https://claude.com/claude-code) installed and signed in.

## Build

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev
cargo install cargo-deb
cargo deb            # writes target/debian/bishop_<version>_amd64.deb
```

For development, `cargo run` starts Bishop from the source tree.

## Files

| Path | Contents |
|---|---|
| `~/.config/bishop/config.toml` | Settings |
| `~/.config/bishop/themes/` | Saved themes |
| `~/.local/share/bishop/agent/` | The agent's workspace; put your own instructions for it in `CLAUDE.local.md` |
| `~/.local/state/bishop/audit.log` | Everything the agent typed |

## License

MIT. See [LICENSE](LICENSE).
