#!/usr/bin/env bash
#
# bench_extract.sh — benchmark the RETURN path: archives on the (slow)
# destination volume -> extracted tree on a local disk.
#
# This is the mirror of bench_csv.sh. A dpl transfer leaves
# <DST>/.dpl/chunk_*.tar.zst on the SMB share; getting the data back means
# reading those chunks over the wire and decompressing them locally.
#
# Compared:
#   dpl     : dpl --extract <archive_dir> <out>   (parallel chunk decode, -Tn)
#   zstdcat : single chunk -> zstd -d | tar -x    (one stream, single thread)
#             (only meaningful when there is exactly ONE chunk; for multi-chunk
#              sets we loop every chunk sequentially, which is the honest shell
#              equivalent of "unpack all archives")
#
# Usage:
#   bench_extract.sh <DPL_DEST_DIR> [OUT_PARENT] [RUNS]
#     DPL_DEST_DIR : a dir that contains chunk_*.tar.zst directly, or a parent
#                    holding .dpl/ (what a transfer writes). REQUIRED.
#     OUT_PARENT   : where extracted trees are written (local = fast). Default $TMPDIR.
#     RUNS         : repetitions; median. Default 3.
#
# Env: DPL_BIN, PURGE=1 (cold cache before each run), KEEP=1 (don't note cleanup).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="${1:?usage: bench_extract.sh <DPL_DEST_DIR> [OUT_PARENT] [RUNS]}"
OUT_PARENT="${2:-${TMPDIR:-/tmp}}"; OUT_PARENT="${OUT_PARENT%/}"
RUNS="${3:-3}"
mkdir -p "$OUT_PARENT"

DPL_BIN="${DPL_BIN:-}"
if [[ -z "$DPL_BIN" ]]; then
  if   [[ -x "$here/target/release/dpl" ]]; then DPL_BIN="$here/target/release/dpl"
  elif [[ -x "$here/target/debug/dpl"   ]]; then DPL_BIN="$here/target/debug/dpl"
  else echo "error: no dpl binary; cargo build --release or set DPL_BIN" >&2; exit 1; fi
fi

# resolve to the dir that holds the chunks
if   ls "$SRC_DIR"/chunk_*.tar.zst >/dev/null 2>&1; then CH="$SRC_DIR"
elif ls "$SRC_DIR"/.dpl/chunk_*.tar.zst >/dev/null 2>&1; then CH="$SRC_DIR/.dpl"
else echo "error: no chunk_*.tar.zst in $SRC_DIR or $SRC_DIR/.dpl" >&2; exit 1; fi

n_chunks=$(ls "$CH"/chunk_*.tar.zst | wc -l | tr -d ' ')
comp_kib=$(du -sk "$CH" | cut -f1)

now()    { python3 -c 'import time;print(f"{time.time():.6f}")'; }
median() { python3 -c 'import sys;v=sorted(float(x) for x in sys.stdin.read().split());n=len(v);print(f"{(v[n//2] if n%2 else (v[n//2-1]+v[n//2])/2):.3f}")'; }
maybe_purge() { [[ "${PURGE:-0}" == 1 ]] && sudo purge 2>/dev/null || true; }
RM() { command rm "$@"; }

echo "═══════════════════════════════════════════════════════════════"
echo " dpl extract benchmark (archives -> local tree)"
echo "═══════════════════════════════════════════════════════════════"
echo "  chunks   : $CH"
echo "  count    : $n_chunks chunk(s), $(python3 -c "print(f'{$comp_kib/1024:.1f}MiB')") compressed"
echo "  out      : $OUT_PARENT  (local)"
echo "  runs     : $RUNS  purge: ${PURGE:-0}"
echo

# ---- dpl --extract ----
d_times=()
for i in $(seq 1 "$RUNS"); do
  out="$OUT_PARENT/.extract_dpl.r${i}.$$"; RM -rf "$out"
  maybe_purge
  t0=$(now); "$DPL_BIN" --extract -q "$CH" "$out" >/dev/null 2>&1; t1=$(now)
  d_times+=("$(python3 -c "print(f'{$t1-$t0:.6f}')")"); RM -rf "$out"
done
d_med=$(printf '%s\n' "${d_times[@]}" | median)

# ---- shell: zstd -d | tar -x, looped over every chunk ----
s_times=()
for i in $(seq 1 "$RUNS"); do
  out="$OUT_PARENT/.extract_sh.r${i}.$$"; RM -rf "$out"; mkdir -p "$out"
  maybe_purge
  t0=$(now)
  for c in "$CH"/chunk_*.tar.zst; do zstd -d -q -c "$c" | tar -C "$out" -xf -; done
  t1=$(now)
  s_times+=("$(python3 -c "print(f'{$t1-$t0:.6f}')")"); RM -rf "$out"
done
s_med=$(printf '%s\n' "${s_times[@]}" | median)

echo "── results ────────────────────────────────────────────────────"
printf "  dpl --extract     %8ss\n" "$d_med"
printf "  zstd -d | tar -x  %8ss   (%d chunk(s), single thread)\n" "$s_med" "$n_chunks"
echo "───────────────────────────────────────────────────────────────"
echo "note: dpl decodes chunks in parallel (-Tn); the shell loop is one"
echo "      chunk at a time, single-thread. Edge grows with chunk count."
