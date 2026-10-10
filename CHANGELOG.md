# Changelog

本项目的版本记录。ResiliSSH 以 **v1.0.0** 作为首个对外正式版。

## [Unreleased]

### Added
- Host key verification: unknown servers require fingerprint confirmation; changed keys are refused. Trusted keys are stored in the app's own `known_hosts`; `~/.ssh/known_hosts` is read, never modified.
- Passwords and key passphrases are stored in the macOS Keychain (existing plaintext entries in `profiles.json` are migrated automatically) and read only when needed.
- New sidebar interface (Transfer / Servers / History / Activity / Settings) with a live block map, result page and unfinished-transfer list.
- Activity log: connection, auth, host key, network, permission, transfer, verification and command events, filterable and copyable.
- Bookmarked and recent remote folders per server.
- Post-transfer command with safely shell-quoted variables (`{remote_dir}` `{file_name}` `{remote_path}` `{files}`).
- Multi-file upload queue; the post-transfer command runs once after all files succeed.
- English UI and language setting; backend messages follow the UI language.
- Diagnostic log file under `~/Library/Logs/com.resiliss.desktop/`.

### Changed
- Network errors are retried without limit; only errors that reconnecting cannot fix (permissions, paths, host keys, disk full) stop a transfer.
- Long-running commands no longer block the UI thread; TCP connect has a 20 s timeout; SSH keepalive is sent while hashing large files.

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

[1.0.0]: https://github.com/dushuning/ResiliSSH/releases/tag/v1.0.0
