use clap::{ArgAction, Parser};
use std::path::PathBuf;

/// Chunked archive transfer — rsync-compatible CLI.
#[derive(Parser, Debug)]
#[command(name = "dpl", version, about, long_about = None)]
pub struct Cli {
    // ── rsync-compatible subset ──────────────────────────────────────────
    /// Archive mode (perms + times + symlinks).
    #[arg(short = 'a', long, action = ArgAction::SetTrue, default_value_t = true)]
    pub archive: bool,

    /// Verbose (repeat for more).
    #[arg(short = 'v', action = ArgAction::Count)]
    pub verbose: u8,

    /// Suppress non-error output.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Show plan without transferring.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Same as --partial --progress.
    #[arg(short = 'P')]
    pub partial_progress: bool,

    /// Show progress bar.
    #[arg(long)]
    pub progress: bool,

    /// Print transfer statistics.
    #[arg(long)]
    pub stats: bool,

    /// Exclude pattern (glob). Repeatable.
    #[arg(long = "exclude", value_name = "PAT")]
    pub exclude: Vec<String>,

    /// Read exclude patterns from FILE.
    #[arg(long = "exclude-from", value_name = "FILE")]
    pub exclude_from: Option<PathBuf>,

    /// Include pattern (glob). Repeatable.
    #[arg(long = "include", value_name = "PAT")]
    pub include: Vec<String>,

    /// Read file list from FILE.
    #[arg(long = "files-from", value_name = "FILE")]
    pub files_from: Option<PathBuf>,

    /// Scan + manifest, no transfer.
    #[arg(long = "list-only")]
    pub list_only: bool,

    /// Recurse into directories.
    #[arg(short = 'r', long, action = ArgAction::SetTrue, default_value_t = true)]
    pub recursive: bool,

    /// Verify with checksums (blake3).
    #[arg(short = 'c', long)]
    pub checksum: bool,

    /// Compress (always-on in chunked mode).
    #[arg(short = 'z', long, action = ArgAction::SetTrue, default_value_t = true)]
    pub compress: bool,

    /// Compression level (1-19, zstd).
    #[arg(long = "compress-level", value_name = "N", default_value_t = 3)]
    pub compress_level: i32,

    /// One filesystem.
    #[arg(long = "one-file-system")]
    pub one_fs: bool,

    // ── restore / inspect modes (read existing archives at <ARCHIVE>) ─────
    /// Restore: extract archives back into a directory tree.
    /// Usage: dpl -x <ARCHIVE> <OUT>
    #[arg(short = 'x', long = "extract", action = ArgAction::SetTrue)]
    pub extract: bool,

    /// List files stored in transferred archives (optional glob filter).
    /// Usage: dpl -l <ARCHIVE> [PATTERN]
    #[arg(short = 'l', long = "list", action = ArgAction::SetTrue)]
    pub list: bool,

    /// Grep: search a literal term inside transferred files.
    /// Usage: dpl -g <TERM> <ARCHIVE>
    #[arg(short = 'g', long = "grep", value_name = "TERM")]
    pub grep: Option<String>,

    /// skip files that are newer on the receiver
    #[arg(short = 'u', long)]
    pub update: bool,

    /// Delete extraneous files on dest.
    #[arg(long)]
    pub delete: bool,

    // ── dpl-specific ─────────────────────────────────────────────────────
    /// Target compressed chunk size (e.g. 512M, 1G).
    #[arg(long = "chunk-size", value_name = "SIZE", default_value = "512M")]
    pub chunk_size: String,

    /// Bin-packing strategy: bfd | locality | mixed.
    #[arg(long = "chunk-strategy", default_value = "bfd")]
    pub chunk_strategy: String,

    /// Treat each .git directory as one atomic chunk.
    #[arg(long = "git-atomic", action = ArgAction::SetTrue, default_value_t = true)]
    pub git_atomic: bool,

    /// Manifest output path (default: <dst>/.dpl/manifest.json).
    #[arg(long = "manifest", value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Resume from existing manifest.
    #[arg(long)]
    pub resume: bool,

    /// Verify chunk blake3 hashes against manifest.
    #[arg(long)]
    pub verify: bool,

    /// Worker threads (default num_cpus/2).
    #[arg(long = "threads", value_name = "N")]
    pub threads: Option<usize>,

    /// Skip AppleDouble (._*) and .DS_Store.
    #[arg(long = "skip-appledouble", action = ArgAction::SetTrue, default_value_t = true)]
    pub skip_appledouble: bool,

    /// Disable AppleDouble skipping.
    #[arg(long = "no-skip-appledouble", action = ArgAction::SetTrue)]
    pub no_skip_appledouble: bool,

    // ── positional (preserved as raw strings for trailing-slash semantics) ─
    /// Transfer: SRC... DST. Restore: ARCHIVE OUT. List: ARCHIVE [PATTERN].
    /// Grep: ARCHIVE. (Per-mode arity is checked in `Cli::mode`.)
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    pub paths_raw: Vec<String>,
}

/// Which operation this invocation performs. Selected by `-x`/`-l`/`-g`;
/// default is a transfer. The three read-modes operate on archives that a
/// previous transfer wrote, never on the live source tree.
#[derive(Debug, Clone)]
pub enum Mode {
    /// SRC... DST — scan, pack, write archives + manifest.
    Transfer,
    /// ARCHIVE OUT — decompress every chunk back into OUT.
    Extract { archive: PathBuf, out: PathBuf },
    /// ARCHIVE [PATTERN] — list stored paths, optional glob filter.
    List {
        archive: PathBuf,
        pattern: Option<String>,
    },
    /// ARCHIVE — search `term` inside every stored file.
    Grep { archive: PathBuf, term: String },
}

/// A source path plus whether the user wrote a trailing slash.
#[derive(Debug, Clone)]
pub struct SrcSpec {
    pub path: PathBuf,
    pub trailing_slash: bool,
}

impl Cli {
    /// Split `paths_raw` into N sources + 1 destination.
    /// Trailing slash on src means "copy contents", not the dir itself
    /// (rsync semantics).
    pub fn srcs_and_dst(&self) -> (Vec<SrcSpec>, PathBuf) {
        let (dst_raw, srcs_raw) = self
            .paths_raw
            .split_last()
            .expect("clap guarantees >= 2 paths");
        let srcs = srcs_raw
            .iter()
            .map(|s| SrcSpec {
                path: PathBuf::from(s.trim_end_matches('/')),
                trailing_slash: s.ends_with('/'),
            })
            .collect();
        (srcs, PathBuf::from(dst_raw.trim_end_matches('/')))
    }

    /// Effective skip-AppleDouble after no-skip override.
    pub fn effective_skip_appledouble(&self) -> bool {
        self.skip_appledouble && !self.no_skip_appledouble
    }

    /// Effective compression threads.
    pub fn effective_threads(&self) -> usize {
        self.threads
            .unwrap_or_else(|| (num_cpus::get_physical() / 2).max(1))
    }

    /// Resolve the operation mode and validate per-mode positional arity.
    /// The three read-modes are mutually exclusive; `-x` wins over `-l` wins
    /// over `-g` if more than one is (mis)specified.
    pub fn mode(&self) -> anyhow::Result<Mode> {
        let p = &self.paths_raw;
        let path = |s: &String| PathBuf::from(s.trim_end_matches('/'));

        if self.extract {
            if p.len() != 2 {
                anyhow::bail!("--extract needs exactly: <ARCHIVE> <OUT>");
            }
            return Ok(Mode::Extract {
                archive: path(&p[0]),
                out: path(&p[1]),
            });
        }
        if self.list {
            if p.is_empty() || p.len() > 2 {
                anyhow::bail!("--list needs: <ARCHIVE> [PATTERN]");
            }
            return Ok(Mode::List {
                archive: path(&p[0]),
                pattern: p.get(1).cloned(),
            });
        }
        if let Some(term) = &self.grep {
            if p.len() != 1 {
                anyhow::bail!("--grep needs: <ARCHIVE> (term is the -g value)");
            }
            return Ok(Mode::Grep {
                archive: path(&p[0]),
                term: term.clone(),
            });
        }
        if p.len() < 2 {
            anyhow::bail!("transfer needs at least: <SRC> <DST>");
        }
        Ok(Mode::Transfer)
    }
}

/// Parse a size string like "512M", "1G", "256K" into bytes.
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('T') | Some('t') => (&s[..s.len() - 1], 1024_u64.pow(4)),
        Some(c) if c.is_ascii_digit() => (s, 1u64),
        _ => anyhow::bail!("invalid size: {s}"),
    };
    let n: u64 = num.trim().parse()?;
    Ok(n * mul)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_avzp() {
        let c = Cli::try_parse_from(["dpl", "-avzP", "src/", "dst/"]).unwrap();
        assert!(c.archive && c.verbose >= 1 && c.compress && c.partial_progress);
    }

    #[test]
    fn trailing_slash_is_preserved() {
        let c = Cli::try_parse_from(["dpl", "/a/b/", "/c/"]).unwrap();
        let (srcs, _) = c.srcs_and_dst();
        assert!(srcs[0].trailing_slash);
    }

    #[test]
    fn excludes_multi() {
        let c = Cli::try_parse_from(["dpl", "--exclude=node_modules", "--exclude=*.tmp", "s", "d"])
            .unwrap();
        assert_eq!(c.exclude, vec!["node_modules", "*.tmp"]);
    }

    #[test]
    fn extract_mode_uses_archive_and_out() {
        let c = Cli::try_parse_from(["dpl", "-x", "dst", "restore"]).unwrap();
        match c.mode().unwrap() {
            Mode::Extract { archive, out } => {
                assert_eq!(archive, PathBuf::from("dst"));
                assert_eq!(out, PathBuf::from("restore"));
            }
            other => panic!("unexpected mode: {other:?}"),
        }
    }

    #[test]
    fn list_mode_accepts_optional_pattern() {
        let c = Cli::try_parse_from(["dpl", "-l", "dst", "*.rs"]).unwrap();
        match c.mode().unwrap() {
            Mode::List { archive, pattern } => {
                assert_eq!(archive, PathBuf::from("dst"));
                assert_eq!(pattern.as_deref(), Some("*.rs"));
            }
            other => panic!("unexpected mode: {other:?}"),
        }
    }

    #[test]
    fn grep_mode_uses_term_flag_and_archive_path() {
        let c = Cli::try_parse_from(["dpl", "-g", "needle", "dst"]).unwrap();
        match c.mode().unwrap() {
            Mode::Grep { archive, term } => {
                assert_eq!(archive, PathBuf::from("dst"));
                assert_eq!(term, "needle");
            }
            other => panic!("unexpected mode: {other:?}"),
        }
    }

    #[test]
    fn transfer_still_needs_src_and_dst() {
        let c = Cli::try_parse_from(["dpl", "only-src"]).unwrap();
        assert!(c.mode().is_err());
    }

    #[test]
    fn size_parser() {
        assert_eq!(parse_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("256K").unwrap(), 256 * 1024);
        assert_eq!(parse_size("42").unwrap(), 42);
    }
}
