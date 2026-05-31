#!/usr/bin/env bash
#
# bench.sh — compare dpl vs rsync vs mv/cp for moving a directory.
#
# Measures wall-clock time to transfer SRC -> a fresh destination using:
#   - dpl   (chunked tar.zst archives at dest/.dpl/)
#   - rsync -a
#   - cp -R           (mv same-filesystem is a rename ≈ 0s; cp is the honest
#                      "copy the bytes" baseline. Cross-fs mv == cp, see below)
#   - mv              (only timed when SRC and DST are on DIFFERENT filesystems,
#                      otherwise it's an instant inode rename and meaningless)
#
# IMPORTANT — the comparison is not symmetric:
#   dpl writes COMPRESSED ARCHIVES (chunk_*.tar.zst). rsync/cp/mv write the
#   real file tree. dpl's win is eliminating per-file round-trips on slow
#   destinations (SMB/NFS/USB); on a fast local disk rsync/cp can win because
#   there is no per-file penalty to amortize. Run this on the destination you
#   actually care about.
#
# Usage:
#   bench_transfer/bench.sh [SRC] [DST_PARENT] [RUNS]
#
#   SRC         directory to transfer. If omitted, a synthetic dataset is
#               generated under a temp dir (many small files + a few big ones).
#   DST_PARENT  parent dir where each tool's destination is created.
#               Default: $TMPDIR. Point this at your SMB/NFS/USB mount to
#               measure the case dpl is built for.
#   RUNS        repetitions per tool; the median is reported. Default: 3.
#
# Env:
#   DPL_BIN     path to dpl binary. Default: target/release/dpl then
#               target/debug/dpl.
#   PURGE=1     run `sudo purge` (macOS) before each run to drop page cache
#               for cold, comparable numbers. Needs sudo; off by default.
#   KEEP=1      keep destinations + synthetic data for inspection.
#
set -euo pipefail

# ── locate dpl ────────────────────────────────────────────────────────────
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DPL_BIN="${DPL_BIN:-}"
if [[ -z "$DPL_BIN" ]]; then
  if   [[ -x "$here/target/release/dpl" ]]; then DPL_BIN="$here/target/release/dpl"
  elif [[ -x "$here/target/debug/dpl"   ]]; then DPL_BIN="$here/target/debug/dpl"
  else echo "error: no dpl binary; run 'cargo build --release' or set DPL_BIN" >&2; exit 1
  fi
fi

RUNS="${3:-3}"
DST_PARENT="${2:-${TMPDIR:-/tmp}}"
DST_PARENT="${DST_PARENT%/}"
SRC="${1:-}"

mkdir -p "$DST_PARENT"

# bypass any user `rm`/`cp` alias or shell function (e.g. rm -> gio trash)
RM() { command rm "$@"; }

# portable device-id of a path's filesystem (macOS: stat -f, Linux: stat -c)
dev_of() {
  if stat -f '%d' "$1" >/dev/null 2>&1; then stat -f '%d' "$1"
  else stat -c '%d' "$1"; fi
}

# portable sub-second clock (BSD date has no %N) ──────────────────────────
now() { python3 -c 'import time;print(f"{time.time():.6f}")'; }

# median of stdin numbers ─────────────────────────────────────────────────
median() {
  python3 -c 'import sys;v=sorted(float(x) for x in sys.stdin.read().split());n=len(v);print(f"{(v[n//2] if n%2 else (v[n//2-1]+v[n//2])/2):.3f}")'
}

human() { # bytes -> human
  python3 -c 'import sys;b=float(sys.argv[1])
for u in "B","KiB","MiB","GiB","TiB":
 if b<1024:print(f"{b:.1f}{u}");break
 b/=1024' "$1"
}

# ── synthetic dataset if no SRC ───────────────────────────────────────────
CLEAN_DIRS=()
cleanup() {
  [[ "${KEEP:-0}" == 1 ]] && return
  for d in "${CLEAN_DIRS[@]:-}"; do [[ -n "$d" && -e "$d" ]] && RM -rf "$d"; done
}
trap cleanup EXIT

if [[ -z "$SRC" ]]; then
  SRC="$(mktemp -d "${DST_PARENT}/dpl_bench_src.XXXXXX")"
  CLEAN_DIRS+=("$SRC")
  echo "→ generating synthetic dataset in $SRC ..."
  # 5000 small text files across 50 dirs
  for d in $(seq 1 50); do
    mkdir -p "$SRC/small/d$d"
    for f in $(seq 1 100); do
      head -c "$((RANDOM % 4096 + 256))" /dev/urandom | base64 > "$SRC/small/d$d/f$f.txt"
    done
  done
  # a few big binaries
  mkdir -p "$SRC/big"
  for f in 1 2 3; do
    dd if=/dev/urandom of="$SRC/big/blob$f.bin" bs=1m count=64 status=none
  done
  echo "  done."
fi

[[ -d "$SRC" ]] || { echo "error: SRC not a directory: $SRC" >&2; exit 1; }

# source stats
SRC_BYTES=$(du -sk "$SRC" | awk '{print $1*1024}')
SRC_FILES=$(find "$SRC" -type f | wc -l | tr -d ' ')

echo
echo "═══════════════════════════════════════════════════════════════"
echo " dpl transfer benchmark"
echo "═══════════════════════════════════════════════════════════════"
echo "  dpl bin   : $DPL_BIN"
echo "  source    : $SRC"
echo "  size      : $(human "$SRC_BYTES")  ($SRC_FILES files)"
echo "  dst parent: $DST_PARENT"
echo "  runs      : $RUNS  (median reported)"
echo "  purge     : ${PURGE:-0}"
echo

# detect whether SRC and DST_PARENT share a filesystem (for mv relevance)
src_dev=$(dev_of "$SRC")
dst_dev=$(dev_of "$DST_PARENT")
SAME_FS=0; [[ "$src_dev" == "$dst_dev" ]] && SAME_FS=1

maybe_purge() { [[ "${PURGE:-0}" == 1 ]] && sudo purge 2>/dev/null || true; }

# run a tool RUNS times, print "name median_s throughput". $1=name $2=cmd-template
# the template uses $DEST as the destination path.
bench_tool() {
  local name="$1" tmpl="$2" times=() t0 t1 dest
  for i in $(seq 1 "$RUNS"); do
    dest="$(mktemp -d "${DST_PARENT}/dpl_bench_${name}.XXXXXX")"
    RM -rf "$dest"            # tools want to create it themselves
    maybe_purge
    t0=$(now)
    DEST="$dest" bash -c "$tmpl" >/dev/null 2>&1 || { echo "  ! $name failed"; RM -rf "$dest"; return 1; }
    t1=$(now)
    times+=("$(python3 -c "print(f'{$t1-$t0:.6f}')")")
    RM -rf "$dest"
  done
  local med; med=$(printf '%s\n' "${times[@]}" | median)
  local thr; thr=$(python3 -c "print(f'{$SRC_BYTES/$med/1048576:.1f}' if $med>0 else 'inf')")
  printf "  %-12s %8ss   %8s MiB/s\n" "$name" "$med" "$thr"
}

echo "── results ────────────────────────────────────────────────────"

bench_tool "dpl"   "'$DPL_BIN' -a '$SRC' \"\$DEST\""
bench_tool "rsync" "rsync -a '$SRC/' \"\$DEST/\""
bench_tool "cp-R"  "cp -R '$SRC' \"\$DEST\""

if [[ "$SAME_FS" == 1 ]]; then
  echo "  mv           skipped — SRC and DST share a filesystem (rename ≈ 0s,"
  echo "               not a copy). Point DST_PARENT at another disk/mount to"
  echo "               benchmark a real cross-filesystem mv."
else
  # cross-fs mv consumes SRC; copy SRC to a throwaway first each run
  bench_tool "mv" "cp -R '$SRC' \"\$DEST.stage\" && mv \"\$DEST.stage\" \"\$DEST\""
fi

echo "───────────────────────────────────────────────────────────────"
echo "note: dpl output is compressed tar.zst (dest/.dpl/), not an extracted"
echo "tree. On fast local disks rsync/cp may win; dpl pulls ahead as the"
echo "destination gets slower per-file (SMB/NFS/USB/WAN)."
