#!/bin/sh
# Reclaim regenerable build output, refusing to touch an image a live process is
# running from.
#
#     sh scripts/clean-artifacts.sh          # report only, delete nothing
#     sh scripts/clean-artifacts.sh --yes    # actually delete
#
# ---------------------------------------------------------------------------
# Why this exists (2026-08-10)
#
# target/debug had reached 14G, 12G of it 758,195 loose *.rcgu.o files sitting
# directly in deps/. On macOS the dev profile defaults to
# split-debuginfo = "unpacked": the linked binary does not carry its own debug
# info, it points at the codegen-unit object files, so rustc leaves them in
# deps/ — and cargo never reclaims the generation that a recompile orphans.
# Every edit mints a fresh set of CGU hashes and abandons the previous set
# forever.
#
# The three manifests now pin split-debuginfo = "packed", which caps that
# particular growth. This script is for what cargo still does not collect:
# whole debug trees per workspace, the Windows cross-build tree, and the
# target/ inside each abandoned agent worktree.
#
# Deliberately NOT removed:
#   app/src-tauri/target/release  the AudioHub.app bundle runs from here
#   target/release, test/target/release
#   target-v3                     regress/m8-p3-protover.sh's prebuilt v3 peer,
#                                 a cache that costs a full release build to
#                                 regenerate
# ---------------------------------------------------------------------------
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1

APPLY=0
[ "${1:-}" = "--yes" ] && APPLY=1

say() { printf '%s\n' "$*" >&2; }

# Is any running process's executable image inside this directory?
#
# Matched against `ps -o comm=`, which reports the *physical* executable path —
# not the command line. Matching command lines instead would flag this very
# script (its argv mentions every path it is about to consider), and a guard
# that cries wolf is a guard someone turns off.
in_use() {
	dir=$(cd "$1" 2>/dev/null && pwd -P) || return 1
	ps -Ao comm= 2>/dev/null | grep -q "^$dir/" && return 0
	return 1
}

TARGETS="
target/debug
test/target/debug
app/src-tauri/target/debug
app/src-tauri/target/x86_64-pc-windows-gnu
"
# Agent worktrees are throwaway; their target/ is pure duplicate build output.
for wt in .claude/worktrees/*/target; do
	[ -d "$wt" ] && TARGETS="$TARGETS$wt
"
done

FOUND=0
for p in $TARGETS; do
	[ -d "$p" ] || continue
	FOUND=1
	size=$(du -sh "$p" 2>/dev/null | cut -f1)
	if in_use "$p"; then
		say "SKIP  $p ($size) — a running process's image is inside it"
		continue
	fi
	if [ "$APPLY" -eq 1 ]; then
		say "rm    $p ($size)"
		rm -rf -- "$p"
	else
		say "would remove  $p ($size)"
	fi
done

[ "$FOUND" -eq 0 ] && say "nothing to reclaim"
[ "$APPLY" -eq 1 ] || say "(report only — pass --yes to delete)"
exit 0
