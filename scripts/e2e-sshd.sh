#!/usr/bin/env bash
# 启动一个仅供端到端测试使用的 sshd：以当前用户身份运行、只监听 127.0.0.1，
# 使用 /tmp 下独立生成的主机密钥与客户端密钥，不读写 ~/.ssh，也不需要管理员权限。
#
#   scripts/e2e-sshd.sh            # 启动（前台运行，Ctrl+C 结束）
#   然后在另一个终端：
#   RESILISS_E2E_PORT=2299 RESILISS_E2E_KEY=/tmp/resiliss-sshd/client_ed25519 \
#   RESILISS_E2E_HOSTKEY=/tmp/resiliss-sshd/host_ed25519.pub \
#   cargo test --manifest-path src-tauri/Cargo.toml e2e -- --test-threads=1
set -euo pipefail

DIR="${RESILISS_SSHD_DIR:-/tmp/resiliss-sshd}"
PORT="${RESILISS_E2E_PORT:-2299}"
SSHD="$(command -v sshd || echo /usr/sbin/sshd)"

mkdir -p "$DIR"
[[ -f "$DIR/host_ed25519" ]] || ssh-keygen -q -t ed25519 -N '' -f "$DIR/host_ed25519"
[[ -f "$DIR/client_ed25519" ]] || ssh-keygen -q -t ed25519 -N '' -f "$DIR/client_ed25519"
cp "$DIR/client_ed25519.pub" "$DIR/authorized_keys"
chmod 600 "$DIR/authorized_keys"

cat > "$DIR/sshd_config" <<CONF
Port $PORT
ListenAddress 127.0.0.1
HostKey $DIR/host_ed25519
PidFile $DIR/sshd.pid
AuthorizedKeysFile $DIR/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
Subsystem sftp internal-sftp
LogLevel ERROR
CONF

echo "Test sshd on 127.0.0.1:$PORT (keys in $DIR). Ctrl+C to stop."
exec "$SSHD" -D -e -f "$DIR/sshd_config"
