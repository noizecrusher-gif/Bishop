# Bishop

A Linux terminal with a built-in AI assistant. Claude Code runs in a side panel and works in your
terminal session the way you would: it reads what's on screen and types commands, using your shell,
your environment, and whatever host you're connected to over SSH. It has no access of its own.

## Features

- **Terminal:** GTK4/libadwaita with VTE. Themes that include colors and font, saved custom themes,
  full 16-color palette editing, background opacity, zoom, full screen, tabs (each with its own agent) and multiple windows.
- **Agent panel:** Claude Code in a resizable side panel, connected to the terminal through a
  built-in MCP server. It can read the screen and scrollback, run commands (with exit codes, using
  shell integration), wait for output, and send keys.
- **Approvals:** with automatic mode off, you approve everything the agent types. Destructive
  commands (`rm -rf`, `dd`, `mkfs`, `reboot`, `curl | sh`, …) always need approval.
- **Guardrails:** the agent can't type at password prompts, your keypress takes control back, and
  every action is recorded in `~/.local/state/bishop/audit.log`.
- **Emergency Stop:** stops the agent and terminates what it's running (Ctrl-C, then SIGTERM, then
  SIGKILL, including background processes it started). Three quick clicks is a Hard Stop, which
  kills the agent and disconnects it from the tab.

## Supported systems

Bishop needs GTK 4.14, libadwaita 1.5 and VTE 0.76 or newer, so it runs on:

| Distribution | Versions |
|---|---|
| Ubuntu (and Kubuntu, Xubuntu…) | 24.04 LTS and newer |
| Linux Mint, Zorin OS, Pop!_OS, elementary OS | releases based on Ubuntu 24.04 or newer |
| Debian | 13 (trixie) and newer |
| Fedora | 41 and newer |
| Arch Linux, Manjaro, EndeavourOS, CachyOS | current |
| openSUSE | Tumbleweed |

Newer widgets are used automatically where the system has them. Exact command completion and exit
codes come from Bishop's own shell integration, which works with **bash, zsh and fish** on every
distribution; other shells, and remote shells over SSH, still work but without exit codes.

The agent needs [Claude Code](https://claude.com/claude-code) installed and signed in.

## Install

On Ubuntu, Debian and their derivatives, download the `.deb` from the
[releases page](https://github.com/noizecrusher-gif/Bishop/releases), then:

```sh
sudo apt install ./bishop_0.10.0-1_amd64.deb
```

Packages for other distributions are coming; until then, build from source.

## Build from source

You need Rust 1.88 or newer (from [rustup](https://rustup.rs); many distributions ship an older one)
and the development files for GTK 4, libadwaita and VTE:

```sh
# Ubuntu, Debian, Mint
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev
# Fedora
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel vte291-gtk4-devel
# Arch
sudo pacman -S --needed base-devel gtk4 libadwaita vte4
# openSUSE
sudo zypper install gcc pkgconf gtk4-devel libadwaita-devel vte-devel
```

Then `cargo build --release` builds `target/release/bishop`. On Debian-based systems,
`cargo install cargo-deb && cargo deb` builds an installable `.deb` instead.

For development, `cargo run` starts Bishop from the source tree.

## Files

| Path | Contents |
|---|---|
| `~/.config/bishop/config.toml` | Settings |
| `~/.config/bishop/themes/` | Saved themes |
| `~/.local/share/bishop/agent/` | The agent's workspace; put your own instructions for it in `CLAUDE.local.md` |
| `~/.local/state/bishop/audit.log` | Everything the agent typed |
| `$XDG_RUNTIME_DIR/bishop/` | Each tab's agent socket and shell-integration channel, and the shell hooks |

## License

MIT. See [LICENSE](LICENSE).
