# Changelog

本项目的版本记录。ResiliSSH 以 **v1.0.0** 作为首个对外正式版。

## [Unreleased]

### Added
- Parallel transfers: large files (32 MB and up) are split across several SSH connections, 4 by default and configurable from 1 to 8 in Settings. Each connection takes the next unfinished piece, so a slow or dropped connection only delays its own piece. Progress is now recorded as a set of completed ranges; checkpoints from earlier versions still resume.
- Only changed blocks are transferred when an older version of the file already exists at the destination (uploads copy the old file on the server and compare block hashes; downloads do the same locally). Needs command execution on the server; otherwise the file is transferred in full as before.
- Verification on the server: per-block checks in Reliable mode and the final whole-file SHA-256 are computed by the server (`sha256sum`, `shasum` or `openssl`) instead of reading the data back, roughly halving the traffic of a verified transfer. SFTP-only servers fall back to read-back automatically.
- `resiliss` command-line tool (`put`, `get`, `servers`) built on the same engine, with its own per-task checkpoints so that re-running a command resumes it.
- A network change (new Wi‑Fi, VPN reconnect) is noticed within a couple of seconds and the transfer reconnects immediately instead of waiting for a timeout.
- A batch can be continued after quitting the app.

### Changed
- Much less waiting between "Test connection", pressing Start and seeing progress: the app now keeps the SSH connection it just used for up to 45 seconds and hands it to the next step (preflight, remote browser, the transfer itself, the next file of a batch, the post-transfer command) instead of doing a full handshake for each. A batch of many small files runs over one connection.
- Pressing Start gives immediate feedback ("Connecting to the server…", "Checking the remote file…"); leaving a path field no longer triggers a duplicate preflight.
- Inputs stay locked for a whole batch and while a post-transfer command runs, and a batch can be cancelled between files.
- A transfer whose data is complete but was interrupted before verification or the final replace (cancel, quit, network) now only finishes those steps instead of starting over. Network errors during final verification are retried automatically.
- A rejected password or key now stops the transfer instead of retrying forever (which could trigger fail2ban or an account lockout); timeouts during authentication are still retried.
- Reliable mode skips the final whole-file hash only if every block was read back; a transfer that used Fast mode for part of the file is hashed in full.
- Saving a server updates the one being edited (renaming keeps its bookmarks and commands); a name that is already taken is refused instead of silently overwriting the other server.
- Zero-byte files can be downloaded (folders often contain `__init__.py`, `.gitkeep`).

### Fixed
- Transfers were reconnecting needlessly: with libssh2's keepalive enabled, any pause that happened to span a keepalive deadline was reported as a timeout. Keepalive is now only used where the connection is otherwise idle.
- A file modified while it was being transferred (local file during upload, remote file during download or between resume attempts) is now detected and reported instead of producing a file stitched from two versions.
- Servers whose `known_hosts` entry uses a different key type than the default (e.g. only ECDSA or RSA) were reported as "host key changed"; the known type is now negotiated first. A host on a non-default port is no longer compared against the port-22 entry of the same name.
- Preflight could offer to resume when the same file was re-sent to a parent folder of the previous target.
- Switching a server from password to key login left the old password in the Keychain.
- Concurrent writes to the history or server list could lose an update or corrupt the file; an unreadable file is now moved aside instead of blocking every later operation.
- Clearing a checkpoint while its transfer is running is refused; a transfer thread that crashes can no longer leave the app stuck in "a transfer is already in progress".
- Remote browser: symlinks to folders can be entered, selected symlinks are downloaded, and a network error while checking for existing files is reported instead of being read as "nothing exists".
- The checkpoint panel no longer claims "resumable" for a different file or server; switching direction no longer overwrites the form; reusing a retried batch keeps its folder structure; two confirmation dialogs in quick succession no longer mix up their answers.
- Dark mode: the command console is distinguishable from the window background.
- Uploading to an existing remote directory entered without a trailing `/` (e.g. `/opt/app`) reported success but left the file as a hidden `.app.resiliss-part` inside that directory. The target is now resolved after connecting, before the temporary file is created.
- A post-transfer command that stayed silent for more than about 30 seconds was reported as "no output for 10 minutes". The 10-minute idle limit is now measured as documented.
- Post-transfer command variables could be expanded twice when a file name itself contained a placeholder such as `{files}`, breaking the shell quoting. Command output containing multi-byte characters is no longer garbled at read boundaries.
- Every successful transfer emitted two "completed" events, which could shift batch results by one file and run the post-transfer command twice.
- Preflight could report "only verification will be retried" (and skip the overwrite confirmation) because of a failed checkpoint that belonged to a different file. Existing 0-byte remote files now also ask for confirmation.
- Relative remote paths (e.g. `uploads/app.jar`) had their parent folders created under `/` instead of the SFTP start directory.
- Remote paths containing words such as `auth` could turn a transient error into a permanent failure.
- A hung SSH handshake (server accepts the TCP connection but never answers) now times out after 90 s and is retried instead of blocking forever.
- Resume verification on very slow links no longer trips the stall watchdog in a loop.
- The reconnect delay now backs off (1 s → 15 s) when the server accepts connections but no data gets through, instead of reconnecting every second.
- A lost reply from the atomic-replace `mv` is no longer treated as success.
- Private key paths typed as `~/.ssh/…` are expanded when connecting; `Match` blocks in `~/.ssh/config` no longer overwrite the preceding host's settings on import.
- Closing the window during a transfer now actually closes it after confirmation (missing window permission).
- Cancelling a batch no longer replaces the destination folder field with the current file's full path.
- After stopping a post-transfer command, the Cancel button of the next transfer stayed disabled; ⌘↩ on a success page re-ran the transfer and its command; local file errors were shown as "remote path error"; speeds below 1 KB/s showed unrounded decimals.

## [1.2.0] - 2026-10-10

### Added
- First-run guidance on the Transfer page: hosts found in `~/.ssh/config` are offered for one-click import (the form is pre-filled for you to check and save), or “Add server” when there are none.
- Transfer progress on the Dock icon (overall progress for batches), so long transfers can run in the background.
- Transfer receipt on the result page: copy or save a plain-text record of a fully successful transfer (server and jump hosts, start/finish time with time zone, verification method, reconnects, and per-file local/remote paths, size and SHA-256). Not offered when any file failed, so it cannot be mistaken for proof of a partial run.
- The Mac no longer idle-sleeps while an upload or download is running (including while waiting to reconnect). Closing the lid or choosing Sleep still works, and the assertion ends with the app even if it crashes.
- Safe replace for uploads: data goes to a hidden `.name.resiliss-part` file in the same folder and replaces the target only after verification (atomic `mv` when the server allows commands, otherwise delete + rename). A failed or interrupted upload never leaves a half-written file at the target path, and the replaced file keeps its permissions. Checkpoints from earlier versions still resume in place.
- Downloads use the same safe replace locally: data goes to `.name.resiliss-part` next to the destination and is renamed into place only after verification, so the Downloads folder never shows a half-downloaded file.
- Jump hosts (like `ssh -J`): pick another saved server under “Via” to reach internal servers through a bastion. Multi-hop chains (up to 3), `ProxyJump` from `~/.ssh/config` on import, host keys confirmed hop by hop, and the usual unlimited reconnect/resume. A jump host that forbids forwarding is reported instead of retried forever.

### Changed
- Download mode lists the remote file before the save location (source → destination, like uploads); the Servers form gives the host more room, puts “Via” next to host and user, and no longer repeats the “password not saved” notice; History shows the Reuse button over the status on hover instead of reserving space that truncated the details.
- UI polish: batches that partly succeeded show an amber “Partly done” in History instead of a red “Failed”; the paths on the Settings page open in Finder; the About text matches the current positioning; the Activity category column is narrower in Chinese; the resume prompt on launch no longer wraps awkwardly.
- The remote browser hides dotfiles by default, with a remembered “Show hidden files” toggle and a count of what is hidden.
- Batch transfers show overall progress — bytes done of the total and an estimate for the whole batch — next to “3 / 238”.
- Batch transfers ask once before starting, with a count of new files and files that will replace existing ones, instead of asking for every existing file (re-uploading a `dist/` folder used to mean hundreds of prompts).

### Fixed
- A batch transfer no longer posts a system notification for every file; one summary notification is shown when the batch ends.
- Folder uploads and downloads no longer pick up `.resiliss-part` temporary files left by an unfinished transfer, and the remote browser never lists them.
- A batch transfer no longer floods the history (which keeps the last 30 entries): the whole batch is recorded as one entry — e.g. “dist/ · 238 files” with how many succeeded — and Reuse re-expands the original selection, picking up files added since.
- Clearing a checkpoint, or starting a different transfer that replaces it, now deletes the abandoned `.resiliss-part` temporary file (locally for downloads, on the server for uploads when connected to that server). If it cannot be removed, the checkpoint is still cleared and the leftover path is shown and logged.
- “New” server and “Import from ~/.ssh/config” no longer carry over the previous server's authentication type and private key path.

## [1.1.0] - 2026-10-10

### Added
- Host key verification: unknown servers require fingerprint confirmation; changed keys are refused. Trusted keys are stored in the app's own `known_hosts`; `~/.ssh/known_hosts` is read, never modified.
- Passwords and key passphrases are stored in the macOS Keychain (existing plaintext entries in `profiles.json` are migrated automatically) and read only when needed.
- New sidebar interface (Transfer / Servers / History / Activity / Settings) with a live block map, result page and unfinished-transfer list.
- Activity log: connection, auth, host key, network, permission, transfer, verification and command events, filterable and copyable.
- Bookmarked and recent remote folders per server.
- Post-transfer command with safely shell-quoted variables (`{remote_dir}` `{file_name}` `{remote_path}` `{files}`).
- Multi-file upload queue; the post-transfer command runs once after all files succeed.
- Batch download (multi-select files and folders in the remote browser) and folder upload; both keep the directory structure. Folders that cannot be read are reported instead of silently skipped.
- Preflight before every transfer: refuse early when the destination disk (remote for uploads, local for downloads) lacks space; skip files whose SHA-256 already matches.
- Offer to resume unfinished transfers on launch.
- Per-server option to not store the password in the Keychain (kept in memory for the current session only).
- English UI and language setting; backend messages follow the UI language.
- Diagnostic log file under `~/Library/Logs/com.resiliss.desktop/`.

### Changed
- Network errors are retried without limit; only errors that reconnecting cannot fix (permissions, paths, host keys, disk full) stop a transfer.
- Long-running commands no longer block the UI thread; TCP connect has a 20 s timeout; SSH keepalive is sent while hashing large files.
- Faster transfers on high-latency links: the remote file stays open across blocks, and write slices adapt to the measured speed (about 3× faster at 100 ms RTT).
- The webview runs under a strict Content Security Policy.
- Frontend split from a single `main.ts` into focused modules under `src/app/`.
- CI on every push: type check, translations, unit tests and end-to-end transfer tests against a real `sshd`.

### Fixed
- A single failed `stat` could reset upload progress to zero or permanently fail a download.
- "Retry verification only" re-transferred the whole file.
- Transfer direction could be switched mid-transfer, and double-clicking Start could mark a running transfer as failed.
- Repeated Keychain authorization prompts on launch.

## [1.0.0] - 2026-07-07

首个正式版（macOS）。

### 传输

- 单文件 SFTP 上传 / 下载，支持方向切换
- 1MB 块对齐断点续传
- 弱网自动重连重试（指数退避 1s → 15s）
- 45s 卡住检测与无响应提示
- 弱网可靠 / 快速传输两种模式
- 强制覆盖、传输中取消、关窗确认

### 完整性

- 传完 SHA-256 全文件校验
- 弱网可靠模式逐块读回校验（verified_bytes）
- 完成后校验摘要

### 连接与体验

- 密码 / 私钥认证，`~/.ssh/config` Host 填充
- 保存连接、测试连接、远端目录/文件浏览
- 拖拽选文件、精确进度、网速与 ETA
- 传输历史（最近 30 条）、系统通知
- 深色模式（系统 / 浅色 / 深色）
- 连接区与传输选项折叠

### 平台

- macOS（Apple Silicon / Intel）
- 构建：`npm run app` → `ResiliSSH.app` / `.dmg`

[Unreleased]: https://github.com/dushuning/ResiliSSH/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/dushuning/ResiliSSH/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/dushuning/ResiliSSH/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/dushuning/ResiliSSH/releases/tag/v1.0.0
