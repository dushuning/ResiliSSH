# ResiliSSH

**macOS 上的弱网 SFTP 单文件传输工具** — 大文件断了接着传，传完自动校验。

**当前版本：v1.0.0**（首个正式版）

> 产品定位：弱网环境下，大文件 SFTP 上传/下载能稳定续传，传完能信得过。  
> 刻意不做：双栏文件管理器、SSH 终端、批量队列。

个人开源项目，主要在 macOS 弱网环境下传大文件使用。欢迎 Star / Issue / PR，不保证及时回复。

**GitHub 仓库：** [dushuning/ResiliSSH](https://github.com/dushuning/ResiliSSH)（公开 · MIT）

---

## 功能特性

### 传输

- 单文件 **上传 / 下载**，支持切换方向
- **1MB 块对齐**断点续传（避免半块脏数据）
- 弱网 **自动重连重试**（指数退避 1s → 15s，直到完成或用户取消）
- 45s 无进度 **卡住检测** + socket 中断重连
- 传输模式：**弱网可靠**（逐块读回校验）/ **快速传输**
- 可选 **强制从头覆盖**（忽略断点）
- 传输中可 **取消**；关闭窗口会提示确认

### 完整性

- 传完 **SHA-256 全文件校验**（本地 vs 远端）
- 弱网可靠模式下每块写完后读回校验，维护 `verified_bytes`
- 续传前块边界内容校验；本地文件变更会提示清断点
- 完成后显示 **校验摘要**

### 连接

- 密码 / SSH 私钥认证
- 读取 `~/.ssh/config` 中的 Host，一键填充
- **保存连接**（`profiles.json`，不存密码）
- **测试连接**（SSH + SFTP）
- 远端 **目录 / 文件浏览**（选择保存路径）

### 体验

- 拖拽选文件、精确字节进度、实时网速与 ETA
- 状态徽章（上传中 / 重连中 / 等待响应 / 校验中等）
- 传输历史（最近 30 条）
- 完成 / 失败 **macOS 系统通知**
- **深色模式**（跟随系统 / 浅色 / 深色）
- 连接区、传输选项可折叠

---

## 系统要求

- **macOS**（Apple Silicon / Intel）
- 暂不支持 Windows / Linux 安装包（核心 Rust 代码可移植，但未测试发布）

---

## 安装

### 方式一：从源码构建

```bash
git clone https://github.com/dushuning/ResiliSSH.git
cd ResiliSSH
npm install
npm run app          # Release 构建
# 或
npm run app:debug    # Debug 构建（更快，体积更大）
```

构建产物：

```text
src-tauri/target/release/bundle/macos/ResiliSSH.app
src-tauri/target/release/bundle/dmg/ResiliSSH_*.dmg
```

将 `ResiliSSH.app` 拖入「应用程序」即可。未签名的本地构建首次打开若被拦截，请 **右键 → 打开**。

### 方式二：GitHub Releases

在 [Releases](https://github.com/dushuning/ResiliSSH/releases) 下载最新 `.dmg`（如 `ResiliSSH_1.0.0_aarch64.dmg`），打开后拖入「应用程序」。

未签名构建首次打开若被拦截，请 **右键 → 打开**。

---

## 开发

### 环境

- [Rust](https://www.rust-lang.org/tools/install)（stable）
- Node.js 18+

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### 常用命令

```bash
npm install
npm run tauri dev    # 开发模式（热更新）
npm run build        # 仅构建前端
npm run app          # 打包 macOS 应用
npm run icon         # 从 logo 重新生成图标（需项目内脚本环境）
```

---

## 使用说明

1. 展开 **连接设置**：从 `~/.ssh/config` 选 Host，或手动填写主机 / 端口 / 用户名
2. 选择认证方式（私钥或密码），可 **测试连接**
3. 选择 **上传** 或 **下载**
4. 填写本地路径与远端路径（目录以 `/` 结尾时会自动拼接文件名）
5. 在 **传输选项** 中选择模式（弱网可靠 / 快速）与外观
6. 点击 **开始上传** 或 **开始下载**
7. 网络中断时会自动重试续传；点击 **取消** 可停止（已传部分保留为断点）
8. 完成后查看 **校验摘要**；失败或取消后断点保留，下次需重新输入密码 / 私钥口令

---

## 本地数据文件

数据目录（macOS）：

```text
~/Library/Application Support/sshutil/
```

| 文件 | 说明 |
|------|------|
| `upload-checkpoint.json` | 上传断点 |
| `download-checkpoint.json` | 下载断点 |
| `profiles.json` | 已保存的连接（不含密码） |
| `upload-history.json` | 传输历史（最近 30 条） |

---

## 技术栈

| 层 | 技术 |
|----|------|
| 桌面壳 | [Tauri 2](https://tauri.app/) |
| 后端 | Rust · [ssh2](https://crates.io/crates/ssh2)（SFTP）·  vendored OpenSSL |
| 前端 | TypeScript · Vite |
| 存储 | JSON 文件（无 SQLite） |

---

## 与 FileZilla / scp / rsync 的区别

| | ResiliSSH | scp | rsync -P | 通用 SFTP 客户端 |
|---|-----------|-----|----------|------------------|
| 弱网自动重试 | ✅ 核心能力 | ❌ | 需自己脚本 | 体验因工具而异 |
| 块级 verified 续传 | ✅ | ❌ | 部分 | 少见 |
| 传完 SHA-256 校验 | ✅ | ❌ | 需自行校验 | 一般无 |
| 上手成本 | 填表即用 | 命令行 | 命令行 | 功能多、偏重 |

---

## 路线图

详见 [ROADMAP.md](./ROADMAP.md)。版本历史见 [CHANGELOG.md](./CHANGELOG.md)。

---

## 许可证

[MIT](./LICENSE)

---

## 说明

- 应用显示名：**ResiliSSH**（`com.resiliss.desktop`）
- 本地开发目录若仍为 `sshutil`，与 GitHub 仓库名 `ResiliSSH` 不一致无影响
