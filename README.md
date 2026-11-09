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

Download the package for your system from the
[releases page](https://github.com/noizecrusher-gif/Bishop/releases). Each release has x86_64 and
aarch64 (ARM) builds.

| System | Package | Install with |
|---|---|---|
| Ubuntu, Debian, Mint, Pop!_OS, Zorin, elementary | `bishop_<version>-1_amd64.deb` (`_arm64.deb` on ARM) | `sudo apt install ./bishop_*.deb` |
| Fedora | `bishop-<version>-1.x86_64.rpm` | `sudo dnf install ./bishop-*.rpm` |
| openSUSE Tumbleweed | `bishop-<version>-1.x86_64.rpm` | `sudo zypper install --allow-unsigned-rpm ./bishop-*.rpm` |
| Arch, Manjaro, EndeavourOS, CachyOS | `PKGBUILD` (AUR: `bishop-bin`) | `makepkg -si` in a folder with the `PKGBUILD` |
| Anything else | `bishop-<version>-<arch>-linux.tar.gz` | extract it, then `./install.sh` (yourself) or `sudo ./install.sh` (everyone) |

Every package is tested on its distribution before a release is published: it is installed in a
fresh container, started, and made to run a command through the agent connection. Each release
also lists the packages' SHA-256 checksums in `SHA256SUMS`.

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

## Security

Bishop gives an AI agent your terminal, so it's built to keep that contained:

- **Only that tab's Claude can drive it.** Each tab's agent socket is private to your account
  and only accepts the bridge started by that tab's own Claude Code process; other programs,
  even ones running as you, are refused.
- **Claude Code runs locked down:** no tools of its own, only Bishop's MCP server, only your
  user-level settings (never project settings), and no hooks.
- **Approvals:** with automatic mode off you approve everything the agent types. Destructive
  commands, anything run as root (`sudo`…), commands recalled from history or scheduled for
  later, and changes to shell startup files, SSH keys or Bishop's and Claude's own settings
  always need approval. Approvals show hidden characters, always show the end of long input,
  apply to one input at a time, and are re-checked before typing. These rules are a safety net,
  not a guarantee: read what you approve.
- **Secrets:** recognizable private keys, tokens and passwords on screen are hidden before the
  agent sees them (Agent Configuration → Hide Secrets from Claude). The agent can never type at
  a password prompt.
- **Audit log:** everything the agent types is logged to `~/.local/state/bishop/audit.log`
  (private to you) and to the system journal (`journalctl --user -t bishop` or
  `journalctl -t bishop`), which programs running as you can't edit.
- **Emergency Stop** terminates what the agent started; "Force Stop as Administrator" runs only
  root-owned system programs and re-checks each process just before killing it.
- **Releases** are built by GitHub Actions with pinned actions, smoke-tested on each distribution,
  and carry signed build provenance: `gh attestation verify <file> --repo noizecrusher-gif/Bishop`.

Report security problems privately through GitHub's
[security advisories](https://github.com/noizecrusher-gif/Bishop/security/advisories/new).

## Files

| Path | Contents |
|---|---|
| `~/.config/bishop/config.toml` | Settings |
| `~/.config/bishop/themes/` | Saved themes |
| `~/.local/share/bishop/agent/` | The agent's workspace; put your own instructions for it in `CLAUDE.local.md` |
| `~/.local/state/bishop/audit.log` | Everything the agent typed (secrets hidden) |
| `$XDG_RUNTIME_DIR/bishop/` | Each tab's agent socket and shell-integration channel, and the shell hooks |

## License

MIT. See [LICENSE](LICENSE).
