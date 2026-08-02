#!/bin/zsh
# 带防线的 AudioHub.app 构建入口 —— 先确认没有进程正从产物运行，再转交
# app/build-app.sh 做真正的构建。
#
#     zsh scripts/build-app.sh [参数...]      # 参数原样透传
#
# ---------------------------------------------------------------------------
# 为什么多这一层
#
# 真正的构建脚本是 app/build-app.sh。它第 4 步 `cargo tauri build` 会**就地重写**
# app/src-tauri/target/release/bundle/macos/AudioHub.app —— 如果 daemon 正从该
# bundle 运行，这一步就会替换运行中进程的磁盘映像与代码身份，其「本地网络」
# (Local Network) TCC 授权随即失配。2026-08-01 事故即此，详见
# scripts/guard-running-image.sh 与 docs/progress.md。
#
# 注意检测必须发生在**构建之前**：app/build-app.sh 末尾才调用 sign-dev.sh，
# 而 sign-dev.sh 里的同一道防线那时已经太晚 —— bundle 早在第 4 步被覆盖了。
#
# 已知缺口：直接执行 `zsh app/build-app.sh` 会绕过这一层。彻底封堵只需在
# app/build-app.sh 的 `[[ "$(uname -s)" == "Darwin" ]] || die ...` 之后加一行：
#
#     sh "$APP_DIR/../scripts/guard-running-image.sh" \
#       "$TAURI_DIR/target/release/bundle/macos/AudioHub.app" || exit 1
#
# 该文件属 app/ 范围，未在本次改动范围内，故先以本包装器兜住常用入口。
# ---------------------------------------------------------------------------
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REAL="$ROOT/app/build-app.sh"

[[ -f "$REAL" ]] || { print -ru2 -- "[audiohub] ERROR: 找不到 $REAL"; exit 1; }

# 这一趟构建会写到的三个映像：bundle 由 cargo tauri build 重写，两个裸二进制由
# 第 1 步的 cargo build --release 重写。任何一个正被执行，都拒绝。
sh "$ROOT/scripts/guard-running-image.sh" \
  "$ROOT/app/src-tauri/target/release/bundle/macos/AudioHub.app" \
  "$ROOT/target/release/audiohubd" \
  "$ROOT/target/release/audiohub" || exit 1

exec zsh "$REAL" "$@"
