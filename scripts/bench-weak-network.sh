#!/usr/bin/env bash
# 弱网对比测试：在本机用 scripts/netem-proxy.py 模拟高延迟、限速和随机断线，
# 比较 resiliss（不同连接数 / 模式）与 scp、rsync 传同一个文件的耗时。
# 不需要管理员权限，所有文件都在 /tmp 下，不读写 ~/.ssh。
#
#   cargo build --release --bin resiliss --manifest-path src-tauri/Cargo.toml
#   scripts/bench-weak-network.sh            # 结果打印成表格
#
# 可用环境变量调整：SIZE_MB（文件大小）、DELAY_MS（单向延迟）、RATE_KBPS（每条连接的带宽上限）、
# KILL_EVERY（断线场景里每条连接平均存活秒数）、TIMEOUT_SECS（单项上限）。
# 结果的解读见 netem-proxy.py 开头的说明：这是对弱网的近似，不是真实网络上的成绩。
set -euo pipefail

SIZE_MB="${SIZE_MB:-48}"
DELAY_MS="${DELAY_MS:-50}"
RATE_KBPS="${RATE_KBPS:-8000}"
KILL_EVERY="${KILL_EVERY:-20}"
TIMEOUT_SECS="${TIMEOUT_SECS:-600}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/src-tauri/target/release/resiliss"
SSHD_DIR="${RESILISS_SSHD_DIR:-/tmp/resiliss-sshd}"
KEY="$SSHD_DIR/client_ed25519"
WORK="$(mktemp -d /tmp/resiliss-bench.XXXXXX)"
SSHD_PORT=2299        # 正常的测试 sshd（允许执行命令）
SFTP_ONLY_PORT=2298   # 只开放 SFTP 的 sshd：校验只能靠把数据读回来
PROXY_PORT=2300       # → SSHD_PORT，延迟 + 限速
PROXY_SFTP_ONLY=2301  # → SFTP_ONLY_PORT，延迟 + 限速
PROXY_FLAKY=2302      # → SSHD_PORT，延迟 + 限速 + 随机断线
SSHD="$(command -v sshd || echo /usr/sbin/sshd)"
PIDS=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  # sshd 是 e2e-sshd.sh exec 出来的，按它自己的 pid 文件结束更可靠。
  kill "$(cat "$SSHD_DIR/sshd.pid" 2>/dev/null)" "$(cat "$WORK/sshd-sftp.pid" 2>/dev/null)" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

[[ -x "$BIN" ]] || { echo "先构建命令行工具：cargo build --release --bin resiliss --manifest-path src-tauri/Cargo.toml"; exit 1; }

# 两个 sshd：复用 e2e 的密钥，第二个强制只提供 SFTP。
RESILISS_SSHD_DIR="$SSHD_DIR" RESILISS_E2E_PORT="$SSHD_PORT" "$ROOT/scripts/e2e-sshd.sh" >"$WORK/sshd.log" 2>&1 &
PIDS+=($!)
sleep 1
sed -e "s/^Port .*/Port $SFTP_ONLY_PORT/" -e "s#^PidFile .*#PidFile $WORK/sshd-sftp.pid#" "$SSHD_DIR/sshd_config" >"$WORK/sshd_sftp_only"
echo "ForceCommand internal-sftp" >>"$WORK/sshd_sftp_only"
"$SSHD" -D -e -f "$WORK/sshd_sftp_only" >"$WORK/sshd-sftp.log" 2>&1 &
PIDS+=($!)

proxy() {
  python3 "$ROOT/scripts/netem-proxy.py" --listen "$1" --target "$2" --delay-ms "$DELAY_MS" --rate-kbps "$RATE_KBPS" \
    --kill-every "${3:-0}" 2>>"$WORK/proxy.log" &
  PIDS+=($!)
  disown $!
}
proxy "$PROXY_PORT" "$SSHD_PORT"
proxy "$PROXY_SFTP_ONLY" "$SFTP_ONLY_PORT"
proxy "$PROXY_FLAKY" "$SSHD_PORT" "$KILL_EVERY"
sleep 1

mkdir -p "$WORK/data" "$WORK/remote"
SRC="$WORK/payload.bin"
head -c $((SIZE_MB * 1024 * 1024)) /dev/urandom >"$SRC"
export RESILISS_DATA_DIR="$WORK/data"
unset SSH_AUTH_SOCK
SSH_OPTS=(-i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o LogLevel=ERROR)

# 运行一项并计时；超过 TIMEOUT_SECS 记为未完成。结果文件必须与源文件逐字节一致才算数。
run() {
  local label="$1"; shift
  local dest="$WORK/remote/payload.bin"
  rm -f "$dest" "$WORK/remote/".payload.bin.* 2>/dev/null || true
  rm -rf "$WORK/data/cli"
  local start; start=$(date +%s)
  ( "$@" ) >"$WORK/run.log" 2>&1 &
  local pid=$!
  ( sleep "$TIMEOUT_SECS"; kill "$pid" 2>/dev/null ) 2>/dev/null &
  local watchdog=$!
  local status=0
  wait "$pid" 2>/dev/null || status=$?
  # 先 disown 再 kill：否则 shell 会为每个被结束的后台任务打印一行 Terminated。
  disown "$watchdog" 2>/dev/null || true
  kill "$watchdog" 2>/dev/null || true
  local secs=$(( $(date +%s) - start ))
  if [[ $status -eq 0 ]] && cmp -s "$SRC" "$dest"; then
    printf '| %-46s | %5d s | %6.2f MB/s |\n' "$label" "$secs" "$(echo "$SIZE_MB / ($secs + 0.001)" | bc -l)"
  else
    printf '| %-46s | %7s | %11s |\n' "$label" "未完成" "-"
  fi
}

resiliss() { local port="$1"; shift; "$BIN" put --accept-new -i "$KEY" "$@" "$SRC" "127.0.0.1:$port:$WORK/remote/"; }
# 断线后重跑直到成功：rsync 靠 --partial --append 接着传，scp 只能从头再来。
until_ok() { until "$@"; do sleep 1; done; }
rsync_put() { rsync --partial --append -e "ssh -p $1 ${SSH_OPTS[*]}" "$SRC" "127.0.0.1:$WORK/remote/"; }
scp_put() { scp -q -P "$1" "${SSH_OPTS[@]}" "$SRC" "127.0.0.1:$WORK/remote/"; }

echo "文件 ${SIZE_MB} MB · 往返延迟 $((DELAY_MS * 2)) ms · 每条连接 ${RATE_KBPS} kbps"
echo
echo "| 稳定但慢的链路                                 |    耗时 |        速度 |"
echo "|------------------------------------------------|---------|-------------|"
run "scp"                                         scp_put "$PROXY_PORT"
run "rsync"                                       rsync_put "$PROXY_PORT"
run "resiliss 1 连接 · 可靠（服务器端校验）"      resiliss "$PROXY_PORT" -c 1
run "resiliss 1 连接 · 可靠（只有 SFTP，读回）"   resiliss "$PROXY_SFTP_ONLY" -c 1
run "resiliss 1 连接 · 快速"                      resiliss "$PROXY_PORT" -c 1 --fast
run "resiliss 4 连接 · 可靠"                      resiliss "$PROXY_PORT" -c 4
run "resiliss 8 连接 · 可靠"                      resiliss "$PROXY_PORT" -c 8
echo
echo "| 每条连接平均 ${KILL_EVERY} 秒断一次                      |    耗时 |        速度 |"
echo "|------------------------------------------------|---------|-------------|"
run "scp（断了从头再来）"                         until_ok scp_put "$PROXY_FLAKY"
run "rsync --partial --append（断了接着传）"      until_ok rsync_put "$PROXY_FLAKY"
run "resiliss 1 连接 · 可靠"                      resiliss "$PROXY_FLAKY" -c 1
run "resiliss 4 连接 · 可靠"                      resiliss "$PROXY_FLAKY" -c 4
