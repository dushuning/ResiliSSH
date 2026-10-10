# Roadmap

> **Positioning:** get files to your server reliably — and know they arrived intact.
>
> Every feature should make transfers *more reliable*, *more trustworthy*, or *easier to get right*.
> Anything that turns ResiliSSH into a file manager, a terminal or a deployment system is out of scope.

## Shipped

### v1.0.0 — 2026-07
Single-file SFTP upload/download, 1 MB block resume, automatic reconnect, SHA-256 verification, saved connections, history, dark mode.

### v1.1.0 — 2026-10
- **Security:** host key verification with fingerprint confirmation; passwords in the macOS Keychain
- **Reliability:** unlimited reconnect for network errors only; non-blocking UI; connect timeout; keepalive during hashing; verify-only resume; diagnostic log file
- **Interface:** sidebar layout, block map, activity log, history with resumable transfers
- **Transfer:** bookmarked/recent remote folders, post-transfer command, multi-file upload queue, batch download, folder upload and download
- **Preflight:** free-space check on the destination (remote or local), skip files that are already identical (SHA-256) in both directions
- **Recovery:** offer to resume unfinished transfers on launch
- **Privacy:** per-server "remember password in Keychain" option
- **i18n:** English and Chinese UI
- **Internals:** persistent remote file handle + adaptive write size (≈3× faster at 100 ms RTT), Content Security Policy, frontend split into modules, end-to-end tests against a real `sshd`, run in CI on every push

### Unreleased
- **Jump hosts** (ProxyJump), multi-hop, imported from `~/.ssh/config`

See [CHANGELOG.md](./CHANGELOG.md) for details.

## Project health (before wider release)

| Item | Why |
|---|---|
| Signed & notarized releases, Homebrew cask | Remove Gatekeeper and repeated Keychain prompts |

## Later / maybe

bandwidth limit · keep running from the menu bar · export a verification report · Windows & Linux builds (Tauri supports them; needs testing and packaging).

## Out of scope

These are deliberate non-goals — good tools already exist for them:

- Dual-pane file manager / FileZilla replacement
- SSH terminal, port forwarding
- Deployment orchestration: multi-server fan-out, pipelines, watching folders for changes
- Two-way sync

Issues and pull requests that fit the positioning are very welcome. For anything on the "Next" list, comment on (or open) an issue so we don't duplicate work.
