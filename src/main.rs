mod archive;
mod cli;
mod plan;
mod restore;
mod s3;
mod scan;

use anyhow::{Context, Result};
use archive::{pack_bin, ChunkReport};
use clap::Parser;
use cli::{Cli, Mode};
use humansize::{format_size, BINARY};
use indicatif::{ProgressBar, ProgressStyle};
use plan::{plan as build_plan, Plan, PlanOpts};
use restore::RestoreOpts;
use scan::{scan_all, ScanOpts};
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Manifest written after a successful run: the original plan plus the
/// per-chunk results (hash, sizes, elapsed).
#[derive(Debug, Serialize)]
struct Manifest {
    plan: Plan,
    chunks: Vec<ChunkReport>,
    schema_version: u32,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    // Read-modes operate on archives a previous transfer wrote; they never
    // touch the live source tree, so dispatch before scan/plan.
    match args.mode()? {
        Mode::Transfer => {}
        Mode::Extract { archive, out } => {
            let opts = RestoreOpts {
                threads: args.effective_threads(),
                verify: args.verify,
                quiet: args.quiet,
            };
            return restore::extract(&archive, &out, &opts);
        }
        Mode::List { archive, pattern } => {
            let opts = RestoreOpts {
                threads: args.effective_threads(),
                verify: args.verify,
                quiet: args.quiet,
            };
            return restore::list(&archive, pattern.as_deref(), &opts);
        }
        Mode::Grep { archive, term } => {
            let opts = RestoreOpts {
                threads: args.effective_threads(),
                verify: args.verify,
                quiet: args.quiet,
            };
            return restore::grep(&archive, &term, &opts);
        }
    }

    let (srcs, dst) = args.srcs_and_dst();

    // ── scan ────────────────────────────────────────────────────────────
    let mut excludes = args.exclude.clone();
    if let Some(path) = &args.exclude_from {
        let txt = fs::read_to_string(path)
            .with_context(|| format!("reading --exclude-from {}", path.display()))?;
        for line in txt.lines() {
            let l = line.trim();
            if !l.is_empty() && !l.starts_with('#') {
                excludes.push(l.to_string());
            }
        }
    }

    let scan_opts = ScanOpts {
        excludes,
        includes: args.include.clone(),
        skip_appledouble: args.effective_skip_appledouble(),
        git_atomic: args.git_atomic,
        one_fs: args.one_fs,
        threads: args.effective_threads(),
        update: args.update,
        dest_root: dst.clone(),
    };

    let t_scan = std::time::Instant::now();
    let entries = scan_all(&srcs, &scan_opts)?;
    let scan_secs = t_scan.elapsed().as_secs_f64();

    // ── plan ────────────────────────────────────────────────────────────
    let chunk_size = cli::parse_size(&args.chunk_size)?;
    let plan_opts = PlanOpts {
        chunk_size,
        strategy: args.chunk_strategy.clone(),
        source_roots: srcs.iter().map(|s| s.path.clone()).collect(),
        dest_root: dst.clone(),
    };
    let t_plan = std::time::Instant::now();
    let plan = build_plan(entries, &plan_opts);
    let plan_secs = t_plan.elapsed().as_secs_f64();

    let dry = args.dry_run || args.list_only;

    // S3 destination: route before any local path handling. The raw last
    // positional is the dest; `PathBuf::from("s3://…")` would otherwise be
    // treated as a local path (creating a bogus `s3:/…` dir on disk).
    let dst_raw = args
        .paths_raw
        .last()
        .map(|s| s.as_str())
        .unwrap_or_default();
    if s3::is_s3(dst_raw) {
        if dry {
            // No upload on a dry-run; just emit the plan locally.
            let json = serde_json::to_string_pretty(&plan)?;
            write_atomic(&PathBuf::from("transfer_plan.json"), json.as_bytes())?;
            if !args.quiet {
                eprintln!("dry-run: plan written to transfer_plan.json, no upload.");
            }
            return Ok(());
        }
        return run_s3(&args, plan, dst_raw);
    }

    let manifest_path: PathBuf = args.manifest.clone().unwrap_or_else(|| {
        if dry && !dst.exists() {
            PathBuf::from("transfer_plan.json")
        } else {
            dst.join(".dpl/manifest.json")
        }
    });
    if let Some(parent) = manifest_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating manifest dir {}", parent.display()))?;
        }
    }

    if !args.quiet {
        print_summary(&plan, scan_secs, plan_secs, &manifest_path, &args)?;
    }

    if dry {
        let json = serde_json::to_string_pretty(&plan)?;
        write_atomic(&manifest_path, json.as_bytes())
            .with_context(|| format!("writing plan to {}", manifest_path.display()))?;
        if !args.quiet {
            eprintln!("dry-run: plan written, no transfer.");
        }
        return Ok(());
    }

    // ── execute (local destination) ──────────────────────────────────────
    let archive_dir = dst.join(".dpl");
    fs::create_dir_all(&archive_dir)
        .with_context(|| format!("creating archive dir {}", archive_dir.display()))?;

    let total_bytes = plan.stats.total_bytes;
    let pb = if args.show_progress() {
        let pb = ProgressBar::new(total_bytes);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .unwrap()
            .progress_chars("█▓▒░"),
        );
        Some(pb)
    } else {
        None
    };

    let zstd_threads = args.effective_threads().max(1) as u32;
    let zstd_level = args.compress_level;
    let mut reports: Vec<ChunkReport> = Vec::with_capacity(plan.bins.len());

    let t_run = std::time::Instant::now();
    for bin in &plan.bins {
        let archive_path = archive_dir.join(&bin.archive);
        let pb_for_bin = pb.clone();
        let report = pack_bin(bin, &archive_path, zstd_level, zstd_threads, |n| {
            if let Some(p) = &pb_for_bin {
                p.inc(n);
            }
        })
        .with_context(|| format!("packing bin #{} ({})", bin.id, bin.archive))?;

        if args.verbose >= 1 && !args.quiet {
            eprintln!(
                "  chunk #{:>5}  {} files  {} -> {}  ({:.2}s, ratio {:.2})",
                report.id,
                report.file_count,
                format_size(report.uncompressed_bytes, BINARY),
                format_size(report.compressed_bytes, BINARY),
                report.elapsed_secs,
                if report.compressed_bytes > 0 {
                    report.uncompressed_bytes as f64 / report.compressed_bytes as f64
                } else {
                    0.0
                },
            );
        }
        reports.push(report);
    }
    if let Some(p) = &pb {
        p.finish_with_message("done");
    }
    let run_secs = t_run.elapsed().as_secs_f64();

    let manifest = Manifest {
        plan,
        chunks: reports,
        schema_version: 1,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    write_atomic(&manifest_path, json.as_bytes())
        .with_context(|| format!("writing manifest {}", manifest_path.display()))?;

    if !args.quiet {
        let total_compressed: u64 = manifest.chunks.iter().map(|c| c.compressed_bytes).sum();
        let total_uncompressed: u64 = manifest.chunks.iter().map(|c| c.uncompressed_bytes).sum();
        eprintln!("── done ───────────────────────────────────────────");
        eprintln!("  bins written  : {}", manifest.chunks.len());
        eprintln!(
            "  uncompressed  : {}",
            format_size(total_uncompressed, BINARY)
        );
        eprintln!(
            "  compressed    : {} (ratio {:.2})",
            format_size(total_compressed, BINARY),
            if total_compressed > 0 {
                total_uncompressed as f64 / total_compressed as f64
            } else {
                0.0
            }
        );
        eprintln!("  pack+write    : {run_secs:.2}s");
        if run_secs > 0.0 {
            eprintln!(
                "  wire avg      : {}",
                format_size((total_compressed as f64 / run_secs) as u64, BINARY)
            );
        }
        eprintln!("  manifest      : {}", manifest_path.display());
    }

    Ok(())
}

/// Execute a transfer whose destination is an `s3://bucket/prefix` URI.
///
/// Each bin is packed to a `tar.zst` chunk and uploaded under the prefix; the
/// manifest is PUT last (`<prefix>/.dpl/manifest.json`). Two modes:
///   - default: pack to a local temp file, then multipart-upload it (stable
///     source allows retry; needs ~one-chunk of scratch space).
///   - `--nodisk`: pipe pack -> multipart upload with no local spill.
fn run_s3(args: &Cli, plan: Plan, dst_raw: &str) -> Result<()> {
    let dest = s3::parse(dst_raw)?;
    let zstd_threads = args.effective_threads().max(1) as u32;
    let zstd_level = args.compress_level;

    if !args.quiet {
        eprintln!("── dpl -> S3 ───────────────────────────────────────");
        eprintln!("  dest          : {}", dest.uri(".dpl/"));
        eprintln!("  bins          : {}", plan.bins.len());
        eprintln!(
            "  mode          : {}",
            if args.nodisk {
                "streaming (--nodisk)"
            } else {
                "temp-local then upload"
            }
        );
    }

    // Progress bar tracks uploaded (compressed) bytes — unlike the local
    // path which counts uncompressed pack bytes, here the upload is the wire
    // cost, so the total is unknown up front (spinner + bytes, no ETA).
    let pb = if args.show_progress() {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] uploaded {bytes} ({bytes_per_sec})",
            )
            .unwrap(),
        );
        Some(pb)
    } else {
        None
    };

    // Scratch dir for the temp-local path (skipped under --nodisk).
    let tmp_dir = std::env::temp_dir();
    let mut reports: Vec<ChunkReport> = Vec::with_capacity(plan.bins.len());

    let t_run = std::time::Instant::now();
    for bin in &plan.bins {
        let pb_for_bin = pb.clone();
        let on_upload = move |n: u64| {
            if let Some(p) = &pb_for_bin {
                p.inc(n);
            }
        };
        let report = if args.nodisk {
            dest.put_bin_streaming(bin, zstd_level, zstd_threads, on_upload)?
        } else {
            // Pack to a uniquely-named temp file, upload, remove.
            let tmp = tmp_dir.join(format!(".dpl-{}-{}", std::process::id(), bin.archive));
            let r = (|| -> Result<ChunkReport> {
                let report = pack_bin(bin, &tmp, zstd_level, zstd_threads, |_| {})?;
                dest.put_file(&bin.archive, &tmp, on_upload)?;
                Ok(report)
            })();
            let _ = fs::remove_file(&tmp); // best-effort cleanup, even on error
            r?
        };

        if args.verbose >= 1 && !args.quiet {
            eprintln!(
                "  chunk #{:>5}  {} files  {} -> {}",
                report.id,
                report.file_count,
                format_size(report.uncompressed_bytes, BINARY),
                dest.uri(&report.archive),
            );
        }
        reports.push(report);
    }
    if let Some(p) = &pb {
        p.finish_with_message("done");
    }
    let run_secs = t_run.elapsed().as_secs_f64();

    // Manifest last, so its presence means the whole set landed.
    let manifest = Manifest {
        plan,
        chunks: reports,
        schema_version: 1,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    dest.put_bytes(".dpl/manifest.json", json.as_bytes())?;

    if !args.quiet {
        let total_compressed: u64 = manifest.chunks.iter().map(|c| c.compressed_bytes).sum();
        let total_uncompressed: u64 = manifest.chunks.iter().map(|c| c.uncompressed_bytes).sum();
        eprintln!("── done ───────────────────────────────────────────");
        eprintln!("  bins uploaded : {}", manifest.chunks.len());
        eprintln!(
            "  uncompressed  : {}",
            format_size(total_uncompressed, BINARY)
        );
        eprintln!(
            "  compressed    : {} (ratio {:.2})",
            format_size(total_compressed, BINARY),
            if total_compressed > 0 {
                total_uncompressed as f64 / total_compressed as f64
            } else {
                0.0
            }
        );
        eprintln!("  upload time   : {run_secs:.2}s");
        if run_secs > 0.0 {
            eprintln!(
                "  wire avg      : {}",
                format_size((total_compressed as f64 / run_secs) as u64, BINARY)
            );
        }
        eprintln!("  manifest      : {}", dest.uri(".dpl/manifest.json"));
    }
    Ok(())
}

/// Write `bytes` to `path` atomically: write a sibling temp file, fsync it,
/// then rename over the target. A crash leaves either the previous file intact
/// or nothing — never a truncated/partial manifest that `--resume` might trust.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("manifest.json");
    let tmp = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join(format!(".{fname}.tmp")),
        None => PathBuf::from(format!(".{fname}.tmp")),
    };
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating temp {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing temp {}", tmp.display()))?;
        // SMB/NFS/exFAT may reject fsync (ENOTSUP/EOPNOTSUPP); bytes are still
        // written, so tolerate it rather than failing the manifest write.
        if let Err(e) = f.sync_all() {
            if !archive::fsync_unsupported(&e) {
                return Err(anyhow::Error::new(e).context(format!("fsync temp {}", tmp.display())));
            }
        }
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn print_summary(
    p: &plan::Plan,
    scan_secs: f64,
    plan_secs: f64,
    manifest: &Path,
    args: &Cli,
) -> Result<()> {
    let mut out = std::io::stderr().lock();
    writeln!(out, "── dpl plan ────────────────────────────────────────")?;
    writeln!(
        out,
        "  src           : {}",
        p.source_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(out, "  dst           : {}", p.dest_root.display())?;
    writeln!(
        out,
        "  chunk target  : {} ({})",
        format_size(p.chunk_size_bytes, BINARY),
        p.strategy
    )?;
    writeln!(out, "  scan time     : {scan_secs:.3}s")?;
    writeln!(out, "  plan time     : {plan_secs:.3}s")?;
    writeln!(out, "  threads       : {}", args.effective_threads())?;
    writeln!(out, "  zstd level    : {}", args.compress_level)?;
    writeln!(out, "  manifest      : {}", manifest.display())?;
    writeln!(out, "── stats ───────────────────────────────────────────")?;
    let s = &p.stats;
    writeln!(out, "  files         : {}", s.total_files)?;
    writeln!(
        out,
        "  total size    : {} ({} bytes)",
        format_size(s.total_bytes, BINARY),
        s.total_bytes
    )?;
    writeln!(out, "  git dirs      : {}", s.git_dirs)?;
    writeln!(out, "  dirs          : {}", s.dirs)?;
    writeln!(out, "  symlinks      : {}", s.symlinks)?;
    writeln!(out, "  appledouble   : {}", s.appledouble)?;
    writeln!(out, "  bins          : {}", s.total_bins)?;
    writeln!(
        out,
        "  avg bin size  : {}",
        format_size(s.avg_bin_size, BINARY)
    )?;
    writeln!(
        out,
        "  max bin size  : {}",
        format_size(s.max_bin_size, BINARY)
    )?;
    writeln!(out, "── biggest files ───────────────────────────────────")?;
    for (path, size) in &s.biggest_files {
        writeln!(
            out,
            "  {:>10}  {}",
            format_size(*size, BINARY),
            path.display()
        )?;
    }
    writeln!(out, "── bin distribution (first 10) ─────────────────────")?;
    for b in p.bins.iter().take(10) {
        writeln!(
            out,
            "  #{:>5}  {:>10}  {:>5} entries  [{}]",
            b.id,
            format_size(b.est_size, BINARY),
            b.entries.len(),
            b.reason
        )?;
    }
    if p.bins.len() > 10 {
        writeln!(out, "  ... and {} more bins", p.bins.len() - 10)?;
    }
    writeln!(out, "────────────────────────────────────────────────────")?;
    Ok(())
}
