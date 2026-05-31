#!/usr/bin/env bash
#
# bench_csv.sh — run the dpl-vs-rsync-vs-cp-vs-mv comparison for every
# source;destination pair listed in folders.csv.
#
# For each pair it times two operations:
#   COPY : src is left in place, copied to a fresh dest
#            - dpl   : dpl -a src dest_dpl   (writes tar.zst chunks)
#            - rsync : rsync -a src/ dest_rsync/
#            - cp    : cp -R src dest_cp
#   MOVE : a *staged copy* of src (made on the source volume) is mv'd to dest.
#          The original src is NEVER moved or deleted — only the throwaway
#          stage is consumed by mv. Cross-filesystem mv == copy + unlink, so
#          this measures a real data move, not an inode rename.
#
# Safety:
#   - Originals in the CSV are only ever READ.
#   - This script does NOT delete anything (no rm). Every output goes into a
#     timestamped run dir so reruns never collide. Clean up manually after.
#
# Usage:
#   bench_transfer/bench_csv.sh [CSV] [RUNS]
#     CSV   default: bench_transfer/folders.csv  (header + "src;dest" lines)
#     RUNS  repetitions per tool/op; median reported. Default: 1.
#
# Env:
#   DPL_BIN  dpl binary (default: target/release/dpl, then debug).
#   PURGE=1  `sudo purge` before each timed op (cold cache). Off by default.
#   SKIP_MOVE=1  only run the COPY comparison (no staging, no mv).
#
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CSV="${1:-$here/bench_transfer/folders.csv}"
RUNS="${2:-1}"

DPL_BIN="${DPL_BIN:-}"
if [[ -z "$DPL_BIN" ]]; then
  if   [[ -x "$here/target/release/dpl" ]]; then DPL_BIN="$here/target/release/dpl"
  elif [[ -x "$here/target/debug/dpl"   ]]; then DPL_BIN="$here/target/debug/dpl"
  else echo "error: no dpl binary; cargo build --release or set DPL_BIN" >&2; exit 1
  fi
fi
[[ -f "$CSV" ]] || { echo "error: CSV not found: $CSV" >&2; exit 1; }

now()    { python3 -c 'import time;print(f"{time.time():.6f}")'; }
median() { python3 -c 'import sys;v=sorted(float(x) for x in sys.stdin.read().split());n=len(v);print(f"{(v[n//2] if n%2 else (v[n//2-1]+v[n//2])/2):.3f}")'; }
maybe_purge() { [[ "${PURGE:-0}" == 1 ]] && sudo purge 2>/dev/null || true; }

stamp="$(date +%Y%m%d_%H%M%S)"
RESULTS="$here/bench_transfer/results_${stamp}.md"

echo "═══════════════════════════════════════════════════════════════"
echo " dpl CSV benchmark — copy & move"
echo "═══════════════════════════════════════════════════════════════"
echo "  dpl bin : $DPL_BIN"
echo "  csv     : $CSV"
echo "  runs    : $RUNS   purge: ${PURGE:-0}   skip_move: ${SKIP_MOVE:-0}"
echo "  results : $RESULTS"
echo

{
  echo "# dpl benchmark — $stamp"
  echo
  echo "dpl: \`$DPL_BIN\` | runs: $RUNS (median) | purge: ${PURGE:-0}"
  echo
  echo "| repo | files | size | op | dpl (s) | rsync (s) | cp (s) | tarpipe (s) | tarzst (s) | mv (s) |"
  echo "| ---- | ----: | ---: | -- | ------: | --------: | -----: | ----------: | ---------: | -----: |"
} > "$RESULTS"

# time one operation RUNS times -> median seconds (or "FAIL"/"skip")
# $1 = command template, $DEST is provided to the template per run, fresh each time.
time_op() {
  local tmpl="$1" runroot="$2" times=() t0 t1 dest i
  for i in $(seq 1 "$RUNS"); do
    dest="${runroot}.r${i}"
    maybe_purge
    t0=$(now)
    if ! DEST="$dest" bash -c "$tmpl" >/dev/null 2>&1; then echo "FAIL"; return 0; fi
    t1=$(now)
    times+=("$(python3 -c "print(f'{$t1-$t0:.6f}')")")
  done
  printf '%s\n' "${times[@]}" | median
}

# trim leading/trailing whitespace + CR
trim() { printf '%s' "$1" | tr -d '\r' | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//'; }

# read CSV robustly even if the last line has no trailing newline
while IFS=';' read -r src dst || [[ -n "$src" ]]; do
  src="$(trim "$src")"; dst="$(trim "$dst")"
  [[ -z "$src" || "$src" == "source" ]] && continue
  if [[ ! -d "$src" ]]; then echo "  ! skip (missing): $src"; continue; fi

  repo="$(basename "$src")"
  files=$(find "$src" -type f 2>/dev/null | wc -l | tr -d ' ')
  kib=$(du -sk "$src" 2>/dev/null | cut -f1); sz="$(python3 -c "print(f'{$kib/1024:.1f}MiB')")"
  dst_parent="$(dirname "$dst")"
  mkdir -p "$dst_parent"

  # per-repo run dir on the destination volume (timestamped, never reused)
  outroot="${dst_parent}/.bench_${stamp}/${repo}"
  mkdir -p "$outroot"

  echo "── $repo  ($files files, $sz) ───────────────────────────────"

  # ---- COPY ----
  #   dpl     : chunked tar.zst archives (multithread zstd, manifest, resume)
  #   rsync   : per-file protocol
  #   cp -R   : per-file copy
  #   tarpipe : tar -cf - | tar -xf -   (one stream, EXTRACTED tree, no compress)
  #   tarzst  : tar -cf - | zstd -3 -T0 (one SINGLE compressed archive, no manifest)
  c_dpl=$(time_op   "'$DPL_BIN' -a '$src' \"\$DEST\""        "${outroot}/copy_dpl")
  c_rsync=$(time_op "rsync -a '$src/' \"\$DEST/\""           "${outroot}/copy_rsync")
  c_cp=$(time_op    "cp -R '$src' \"\$DEST\""                "${outroot}/copy_cp")
  c_tarp=$(time_op  "mkdir -p \"\$DEST\" && tar -C '$src' -cf - . | tar -C \"\$DEST\" -xf -" "${outroot}/copy_tarpipe")
  c_tarz=$(time_op  "mkdir -p \"\$DEST\" && tar -C '$src' -cf - . | zstd -3 -T0 -q -o \"\$DEST/archive.tar.zst\"" "${outroot}/copy_tarzst")
  printf "  copy   dpl=%-8s rsync=%-8s cp=%-8s tarpipe=%-8s tarzst=%-8s\n" \
         "$c_dpl" "$c_rsync" "$c_cp" "$c_tarp" "$c_tarz"
  echo "| $repo | $files | $sz | copy | $c_dpl | $c_rsync | $c_cp | $c_tarp | $c_tarz | — |" >> "$RESULTS"

  # ---- MOVE (on a staged copy; originals untouched) ----
  if [[ "${SKIP_MOVE:-0}" == 1 ]]; then
    echo "  move   skipped (SKIP_MOVE=1)"
    echo "| $repo | $files | $sz | move | — | — | — | — | — | skipped |" >> "$RESULTS"
  else
    # Stage lives on the SOURCE volume; dest lives on the DESTINATION volume
    # (CSV target's parent) so the timed mv is a real cross-filesystem move
    # = copy + unlink, NOT an instant same-fs inode rename.
    src_vol_parent="$(dirname "$src")"
    stageroot="${src_vol_parent}/.bench_stage_${stamp}/${repo}"
    mkdir -p "$stageroot"

    # only mv is a true move; dpl/rsync/cp have no move semantics of their own.
    m_times=()
    for i in $(seq 1 "$RUNS"); do
      stage="${stageroot}.r${i}.stage"
      dest="${outroot}/move_mv.r${i}"      # on destination volume -> cross-fs
      cp -R "$src" "$stage"        # build throwaway copy on source vol (not timed)
      maybe_purge
      t0=$(now)
      mv "$stage" "$dest"          # timed: cross-fs move = copy + unlink
      t1=$(now)
      m_times+=("$(python3 -c "print(f'{$t1-$t0:.6f}')")")
    done
    m_mv=$(printf '%s\n' "${m_times[@]}" | median)
    printf "  move   mv=%-8s (cross-fs; staged copy consumed, original intact)\n" "$m_mv"
    echo "| $repo | $files | $sz | move | — | — | — | — | — | $m_mv |" >> "$RESULTS"
  fi
  echo
done < "$CSV"

echo "───────────────────────────────────────────────────────────────"
echo "results table -> $RESULTS"
echo
echo "NOTE: dpl writes compressed tar.zst (dest/.dpl/), not an extracted tree."
echo "      copy outputs + staged move copies were left on disk (no auto-rm)."
echo "      clean up:  the .bench_${stamp} dirs under each dest parent and"
echo "                 .bench_stage_${stamp} dirs under each source parent."
