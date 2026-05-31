use crate::scan::{Entry, Kind};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One archive chunk = many entries packed together.
#[derive(Debug, Serialize)]
pub struct Bin {
    pub id: usize,
    /// Filename for the future archive (e.g. `chunk_00001.tar.zst`).
    pub archive: String,
    /// Estimated uncompressed bytes (sum of entry sizes).
    pub est_size: u64,
    /// Why this bin is closed: capacity hit, atomic unit, or oversize.
    pub reason: &'static str,
    /// Entries placed in this bin, sorted by rel path for stable manifest.
    pub entries: Vec<Entry>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub source_roots: Vec<PathBuf>,
    pub dest_root: PathBuf,
    pub chunk_size_bytes: u64,
    pub strategy: String,
    pub bins: Vec<Bin>,
    pub stats: PlanStats,
}

#[derive(Debug, Serialize)]
pub struct PlanStats {
    pub total_files: u64,
    pub total_bytes: u64,
    pub total_bins: usize,
    pub dirs: usize,
    pub git_dirs: usize,
    pub symlinks: usize,
    pub appledouble: usize,
    pub biggest_files: Vec<(PathBuf, u64)>,
    pub avg_bin_size: u64,
    pub max_bin_size: u64,
}

pub struct PlanOpts {
    pub chunk_size: u64,
    pub strategy: String,
    pub source_roots: Vec<PathBuf>,
    pub dest_root: PathBuf,
}

/// Bin-pack `entries` into chunks.
///
/// Best-Fit-Decreasing: entries are sorted descending by size, then each is
/// placed in the open bin whose remaining capacity is the *smallest* that
/// still fits. An index of open bins keyed by remaining capacity
/// (`open: BTreeMap<remaining, Vec<bin_idx>>`) makes each placement
/// O(log bins) instead of the O(bins) linear scan a naive first-fit needs —
/// the difference between seconds and minutes at million-file scale. Packing
/// is at least as dense as first-fit. `.git` dirs become atomic bins (one per
/// bin). Files >= chunk_size also get their own bin ("oversize").
pub fn plan(mut entries: Vec<Entry>, opts: &PlanOpts) -> Plan {
    let mut git_dirs = 0usize;
    let mut dirs = 0usize;
    let mut symlinks = 0usize;
    let mut appledouble = 0usize;
    for e in &entries {
        match e.kind {
            Kind::Dir => dirs += 1,
            Kind::GitDir => git_dirs += 1,
            Kind::Symlink => symlinks += 1,
            Kind::AppleDouble => appledouble += 1,
            Kind::File => {}
        }
    }

    // FFD: sort entries descending by size.
    entries.sort_by_key(|e| std::cmp::Reverse(e.size));

    let mut bins: Vec<Bin> = Vec::new();
    let mut next_id = 1usize;

    // Pre-compute totals before consuming entries.
    let total_files = entries
        .iter()
        .filter(|e| !matches!(e.kind, Kind::Dir))
        .count() as u64;
    let total_bytes: u64 = entries.iter().map(|e| e.size).sum();
    let mut biggest = entries
        .iter()
        .take(10)
        .map(|e| (e.rel.clone(), e.size))
        .collect::<Vec<_>>();
    biggest.sort_by_key(|e| std::cmp::Reverse(e.1));

    // remaining capacity -> indices of open "capacity" bins with that slack.
    // Atomic/oversize bins are never inserted here, so they're never reused.
    let mut open: BTreeMap<u64, Vec<usize>> = BTreeMap::new();

    for e in entries {
        let atomic = matches!(e.kind, Kind::GitDir) || e.size >= opts.chunk_size;
        if atomic {
            let reason = if matches!(e.kind, Kind::GitDir) {
                "git-atomic"
            } else {
                "oversize"
            };
            let est_size = e.size;
            bins.push(Bin {
                id: next_id,
                archive: format!("chunk_{:05}.tar.zst", next_id),
                est_size,
                reason,
                entries: vec![e],
            });
            next_id += 1;
            continue;
        }

        // Best fit: smallest remaining capacity >= e.size.
        let best = open.range(e.size..).next().map(|(&rem, _)| rem);
        match best {
            Some(rem) => {
                let idx = {
                    let v = open.get_mut(&rem).unwrap();
                    let idx = v.pop().unwrap();
                    if v.is_empty() {
                        open.remove(&rem);
                    }
                    idx
                };
                let b = &mut bins[idx];
                b.est_size += e.size;
                b.entries.push(e);
                open.entry(opts.chunk_size - b.est_size)
                    .or_default()
                    .push(idx);
            }
            None => {
                let idx = bins.len();
                let est_size = e.size;
                bins.push(Bin {
                    id: next_id,
                    archive: format!("chunk_{:05}.tar.zst", next_id),
                    est_size,
                    reason: "capacity",
                    entries: vec![e],
                });
                next_id += 1;
                open.entry(opts.chunk_size - est_size)
                    .or_default()
                    .push(idx);
            }
        }
    }

    // Stable order inside each bin.
    for b in &mut bins {
        b.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    }
    bins.sort_by_key(|b| b.id);

    let total_bins = bins.len();
    let max_bin_size = bins.iter().map(|b| b.est_size).max().unwrap_or(0);
    let avg_bin_size = if total_bins == 0 {
        0
    } else {
        total_bytes / total_bins as u64
    };

    Plan {
        source_roots: opts.source_roots.clone(),
        dest_root: opts.dest_root.clone(),
        chunk_size_bytes: opts.chunk_size,
        strategy: opts.strategy.clone(),
        bins,
        stats: PlanStats {
            total_files,
            total_bytes,
            total_bins,
            dirs,
            git_dirs,
            symlinks,
            appledouble,
            biggest_files: biggest,
            avg_bin_size,
            max_bin_size,
        },
    }
}
