#!/usr/bin/env bash
# 构建通用（Apple Silicon + Intel）安装包。
#
# 包里有两个可执行文件：图形界面和命令行 resiliss。`tauri build --target universal-apple-darwin`
# 只会把主程序合并成通用二进制，打包时却要把两个都拷进 .app，找不到通用版的 resiliss 就会失败。
# 所以先自己把两个架构的 resiliss 构建出来并用 lipo 合并，放到打包器会去找的位置，再交给 tauri。
# 结果是命令行工具随应用一起分发：/Applications/ResiliSSH.app/Contents/MacOS/resiliss。
set -euo pipefail

cd "$(dirname "$0")/.."
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
OUT="src-tauri/target/universal-apple-darwin/release"

# 生成前端：cargo 编译时需要 dist/ 存在。
npm run build
for target in "${TARGETS[@]}"; do
  cargo build --release --bin resiliss --target "$target" --manifest-path src-tauri/Cargo.toml
done
mkdir -p "$OUT"
lipo -create -output "$OUT/resiliss" \
  "src-tauri/target/${TARGETS[0]}/release/resiliss" \
  "src-tauri/target/${TARGETS[1]}/release/resiliss"

npx tauri build --target universal-apple-darwin
