#!/bin/sh
# 拒绝在「目标磁盘映像正被进程使用」时重建 / 重签它。
#
# 用法（被调用，不是被 source）：
#     sh scripts/guard-running-image.sh <路径> [<路径>...] || exit 1
# 路径可以是 .app 目录，也可以是单个可执行文件。
# 退出码：0 = 放行（无占用 / 已被显式覆盖 / 无法检测）；1 = 拒绝。
#
# ---------------------------------------------------------------------------
# 为什么存在（2026-08-01 实际事故，docs/progress.md 已归档）
#
# 一个 agent 为了看新界面重建并用 sign-dev.sh 重签了 AudioHub.app —— 而 daemon
# 正从该 bundle 运行。磁盘映像与代码身份被就地替换，运行中进程的「本地网络」
# (Local Network) TCC 授权随即失配，26 次重连全部失败、两端互判离线、会话归零。
#
# 最阴险的地方是它**不报权限错误**：macOS 在本地网络权限不足时伪造
# EHOSTUNREACH(errno 65)，日志只会刷 `No route to host`，看起来像网络故障。
# 当时 shell 里 `nc -z` 连同一 host:port 是通的、ping 0.66ms 正常。
#
# 所以这道防线放在**任何会写这些映像的脚本最开头**，早于第一次写操作。
# ---------------------------------------------------------------------------
set -eu

ALLOW_VAR=AUDIOHUB_ALLOW_REBUILD_WHILE_RUNNING
say() { printf '%s\n' "$*" >&2; }

[ "$#" -gt 0 ] || { say "[guard] 用法: guard-running-image.sh <路径>..."; exit 2; }

# 解析成物理路径（吃掉路径里的符号链接）。
# 必须做：`ps -o comm=` 报的是**物理路径**，而调用方传进来的往往是逻辑路径。
# 实测中 /tmp 是 /private/tmp 的符号链接，逻辑路径做前缀匹配直接漏检；真实仓库
# 若位于任何软链之下（/Users 在部分机型上就指向 /System/Volumes/Data/Users）
# 会踩同一个坑 —— 漏检的防线比没有防线更糟。
phys_path() {
  p=$1
  if [ -d "$p" ]; then
    (cd "$p" 2>/dev/null && pwd -P) || printf '%s' "$p"
  else
    d=$(dirname "$p"); b=$(basename "$p")
    d=$( (cd "$d" 2>/dev/null && pwd -P) || printf '%s' "$d" )
    printf '%s/%s' "$d" "$b"
  fi
}

# 把 .app 目录展开成它内部真正的可执行文件；普通文件用自身。
# lsof 是按 inode 匹配的，必须给它具体文件，给目录只会匹配到「把该目录当 cwd」
# 的进程，毫无用处。
image_files() {
  d=$1
  if [ -d "$d/Contents/MacOS" ]; then
    for f in "$d"/Contents/MacOS/*; do [ -f "$f" ] && printf '%s\n' "$f"; done
  elif [ -f "$d" ]; then
    printf '%s\n' "$d"
  fi
}

# 按「可执行映像路径」找占用进程 —— 不是按命令行找。
#
# 为什么不用 `pgrep -f <路径>`：pgrep -f 匹配的是**整条命令行**，任何仅仅
# **提到**该路径的进程都会命中。实测（写这道防线时）：一条命令行里含有该 bundle
# 路径的普通 shell 自己就被 `pgrep -f AudioHub.app` 命中了，产生一个转瞬即逝的
# 幽灵 pid。用它做判据会把 `grep`、编辑器、另一个 agent 的 shell 全部误判成
# 「daemon 在跑」，防线一旦开始狼来了就会被人直接关掉。
#
# 这里用 `ps -o comm=`：macOS 上它给出进程**真正的可执行文件全路径**，与 argv[0]
# 无关、无法被参数伪造。匹配规则是「等于目标」或「位于目标目录之下」，而不是子串，
# 否则目标 .../audiohub 会顺带匹配 .../audiohubd。
pids_by_image() {
  t=$1
  ps -Ao pid=,comm= 2>/dev/null | awk -v t="$t" '
    {
      pid = $1
      comm = substr($0, index($0, $2))          # 路径可能含空格，取第一字段之后的全部
      if (comm == t || substr(comm, 1, length(t) + 1) == t "/") print pid
    }
  ' || true
}

# lsof 按 inode 精确匹配，是首选判据：进程会把自己的可执行文件作为 txt 持有。
pids_by_lsof() {
  command -v lsof >/dev/null 2>&1 || return 0
  lsof -t -- "$@" 2>/dev/null || true
}

# 自己和自己的祖先链要排除：本脚本的 argv 里就带着目标路径，调用它的构建脚本同理。
# comm 匹配基本已经免疫，但 lsof 也可能把「持有该文件的 shell」算进来，便宜的保险。
self_chain() {
  p=$$
  i=0
  while [ "$p" -gt 1 ] && [ "$i" -lt 24 ]; do
    printf '%s\n' "$p"
    p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ') || break
    [ -n "${p:-}" ] || break
    i=$((i + 1))
  done
}

have_ps=0
ps -Ao pid=,comm= >/dev/null 2>&1 && have_ps=1
have_lsof=0
command -v lsof >/dev/null 2>&1 && have_lsof=1

# CI / 精简容器里可能两样都没有。此时**放行并告警**，绝不因为检测工具缺失而让
# 构建崩掉（真正有 daemon 常驻的只有开发机，那里 ps/lsof 一定在）。
if [ "$have_ps" -eq 0 ] && [ "$have_lsof" -eq 0 ]; then
  say "[guard] 警告: 本机既无可用的 ps 也无 lsof，跳过「映像是否在运行」检测。"
  say "[guard]        若这是开发机，请手动确认 daemon 未从目标 bundle 运行。"
  exit 0
fi

EXCLUDE=$(self_chain 2>/dev/null || true)

BUSY_TARGETS=""
BUSY_LINES=""
HINT_CLI=""

for target in "$@"; do
  [ -e "$target" ] || continue                  # 首次构建：产物还不存在，自然无人占用

  files=$(image_files "$target")
  target_phys=$(phys_path "$target")

  found=$(
    {
      # 判据一：inode 精确 —— 谁把这些文件作为可执行映像打开着
      if [ "$have_lsof" -eq 1 ] && [ -n "$files" ]; then
        printf '%s\n' "$files" | while IFS= read -r f; do
          [ -n "$f" ] || continue
          pids_by_lsof "$f"
        done
      fi
      # 判据二：可执行路径前缀 —— 覆盖 inode 已被上一次构建替换掉的情况
      # （旧进程还映射着已 unlink 的 inode，按路径查 lsof 就找不到它了）。
      # 逻辑路径与物理路径都比一遍，两者可能不同；相同就只比一遍。
      if [ "$have_ps" -eq 1 ]; then
        pids_by_image "$target"
        if [ "$target_phys" != "$target" ]; then
          pids_by_image "$target_phys"
        fi
      fi
    } | sort -u
  )

  for pid in $found; do
    case "$pid" in ''|*[!0-9]*) continue ;; esac
    echo "$EXCLUDE" | grep -qx "$pid" && continue        # 自己 / 祖先
    line=$(ps -p "$pid" -o pid=,args= 2>/dev/null || true)
    [ -n "$line" ] || continue                           # 竞态：刚查到就退出了
    BUSY_LINES="${BUSY_LINES}    $(printf '%s' "$line" | sed 's/^ *//')
"
    # 一律显示绝对路径：调用方常传相对路径，而这段提示是要被复制粘贴执行的，
    # 相对路径换个 cwd 就跑不通了。
    case "$BUSY_TARGETS" in
      *"$target_phys"*) ;;
      *) BUSY_TARGETS="${BUSY_TARGETS}    $target_phys
" ;;
    esac
  done

  # 提示用户用 bundle 自带的 CLI 停 daemon，路径准确、不依赖 PATH
  [ -z "$HINT_CLI" ] && [ -x "$target/Contents/MacOS/audiohub" ] && \
    HINT_CLI="$target_phys/Contents/MacOS/audiohub"
done

[ -n "$BUSY_LINES" ] || exit 0                  # 无人占用，放行

[ -n "$HINT_CLI" ] || HINT_CLI="audiohub"

say ""
say "[guard] ============================================================"
say "[guard] 拒绝执行：目标磁盘映像正在被进程使用"
say "[guard] ============================================================"
say "[guard]"
say "[guard] 目标："
printf '%s' "$BUSY_TARGETS" >&2
say "[guard] 占用它的进程："
printf '%s' "$BUSY_LINES" >&2
say "[guard]"
say "[guard] 为什么拒绝（2026-08-01 实际事故，docs/progress.md 已归档）："
say "[guard]   重建或重签一个**正在运行**的 bundle，会就地替换该进程的磁盘映像与"
say "[guard]   代码身份；macOS 记录在旧身份上的「本地网络」(Local Network) TCC 授权"
say "[guard]   随即失配。daemon 不会收到任何权限错误，而是**每一次外连都失败**，"
say "[guard]   两端互判离线、会话归零。当时 26 次重连无一成功。"
say "[guard]"
say "[guard] 识别特征（以后排障先看这一条）："
say "[guard]   若 daemon 日志刷 \`No route to host (os error 65)\`，而同机其它进程"
say "[guard]   （nc / ping / 另一个进程）连得通**同一 host:port** —— 那就是本问题，"
say "[guard]   不是网络故障。macOS 在本地网络权限不足时不报权限错误，而是伪造"
say "[guard]   EHOSTUNREACH(errno 65)，所以日志看起来完全像是网络不通。"
say "[guard]"
say "[guard] 正确做法："
say "[guard]   1) 先停掉 daemon（会话会正常收尾）："
say "[guard]        '$HINT_CLI' ctl shutdown"
say "[guard]   2) 再执行本次构建 / 签名；"
say "[guard]   3) 构建完由 app 重新拉起 daemon，它会自己 ensure_daemon —— 代码身份"
say "[guard]      稳定时不会再弹权限对话框。"
say "[guard]"
say "[guard] 确实需要强行继续（已明白上述后果）："
say "[guard]   $ALLOW_VAR=1 <原命令>"
say "[guard] ============================================================"
say ""

# 覆盖开关放在最后判断：即使强行继续，上面那一整段也照样打印出来，
# 让人留下记录，而不是一个被静默吞掉的 -f。
# 直接引用变量名（不用 eval 间接取值）—— 这里没有动态性可言，eval 只会带来风险。
if [ "${AUDIOHUB_ALLOW_REBUILD_WHILE_RUNNING:-}" = "1" ]; then
  say "[guard] $ALLOW_VAR=1 —— 已显式覆盖，继续执行。出事请回看上面这段。"
  exit 0
fi

exit 1
