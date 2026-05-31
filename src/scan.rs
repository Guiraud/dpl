use crate::cli::SrcSpec;
use anyhow::Result;
use globset::{Glob, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What kind of source entry we found.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Kind {
    /// Directory entry. Preserves empty directories during restore.
    Dir,
    /// Regular file with byte size.
    File,
    /// Symlink.
    Symlink,
    /// A `.git` directory — treated as one atomic unit downstream.
    GitDir,
    /// AppleDouble (`._*`, `.DS_Store`) — usually skipped.
    AppleDouble,
}

/// One scanned entry, ready for planning.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Path relative to the source root (the SrcSpec.path).
    pub rel: PathBuf,
    /// Absolute path on disk.
    pub abs: PathBuf,
    /// Size in bytes. For `GitDir` this is the recursive sum.
    pub size: u64,
    pub kind: Kind,
}

/// Scan options derived from the CLI.
pub struct ScanOpts {
    pub excludes: Vec<String>,
    pub includes: Vec<String>,
    pub skip_appledouble: bool,
    pub git_atomic: bool,
    pub one_fs: bool,
    pub threads: usize,
    /// rsync `-u`: skip a source file when the destination counterpart
    /// (`dest_root/<rel>`) has a strictly newer mtime.
    pub update: bool,
    /// Destination root, used only for the `update` mtime comparison.
    pub dest_root: PathBuf,
}

/// Scan every source spec, returning a flat list of [`Entry`].
pub fn scan_all(srcs: &[SrcSpec], opts: &ScanOpts) -> Result<Vec<Entry>> {
    let exc = build_globset(&opts.excludes)?;
    let inc = build_globset(&opts.includes)?;
    let has_inc = !opts.includes.is_empty();

    let mut out = Vec::new();
    for spec in srcs {
        scan_one(spec, &exc, &inc, has_inc, opts, &mut out)?;
    }
    Ok(out)
}

/// Per-worker scratch buffers. Each parallel walker thread accumulates into
/// its own thread-local `Vec`, then flushes to the shared sinks exactly once
/// on `Drop` (thread end). This turns ~N per-entry mutex acquisitions into
/// one per worker thread — the lock no longer serializes the walk.
struct Batch<'a> {
    entries: Vec<Entry>,
    git_roots: Vec<PathBuf>,
    entry_sink: &'a Mutex<Vec<Entry>>,
    git_sink: &'a Mutex<Vec<PathBuf>>,
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        if !self.entries.is_empty() {
            // Recover from a poisoned lock instead of re-panicking (which would
            // abort the process); a sibling thread's panic is already reported.
            let mut sink = self.entry_sink.lock().unwrap_or_else(|e| e.into_inner());
            sink.append(&mut self.entries);
        }
        if !self.git_roots.is_empty() {
            let mut sink = self.git_sink.lock().unwrap_or_else(|e| e.into_inner());
            sink.append(&mut self.git_roots);
        }
    }
}

/// Parallel walk one source root using the `ignore` crate (fd-style).
/// `.git` directories are detected during the walk and skipped, then summed
/// in a single sequential pass per `.git` root — net effect: each file is
/// visited at most once.
fn scan_one(
    spec: &SrcSpec,
    exc: &globset::GlobSet,
    inc: &globset::GlobSet,
    has_inc: bool,
    opts: &ScanOpts,
    out: &mut Vec<Entry>,
) -> Result<()> {
    let root = &spec.path;
    // Shared sinks, written once per worker thread via `Batch::drop`.
    let entries: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    let git_roots: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false) // don't skip .git, dotfiles
        .ignore(false) // don't read .ignore
        .git_ignore(false) // don't read .gitignore
        .git_exclude(false)
        .git_global(false)
        .parents(false)
        .follow_links(false)
        .same_file_system(opts.one_fs)
        .threads(opts.threads.max(1));

    builder.build_parallel().run(|| {
        // One batch per worker thread; flushed to the shared sinks on Drop.
        let mut batch = Batch {
            entries: Vec::new(),
            git_roots: Vec::new(),
            entry_sink: &entries,
            git_sink: &git_roots,
        };
        Box::new(move |res| {
            let entry = match res {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("warn: walk error: {e}");
                    return WalkState::Continue;
                }
            };
            let abs = entry.path().to_path_buf();
            let rel = relative_output_path(root, &abs, spec.trailing_slash);
            let file_name = entry.file_name().to_string_lossy();

            let ft = match entry.file_type() {
                Some(t) => t,
                None => return WalkState::Continue, // root entry or symlink-to-nowhere
            };

            // .git directory: register atomic unit, signal walker to skip.
            if opts.git_atomic && ft.is_dir() && file_name == ".git" {
                batch.git_roots.push(abs.clone());
                batch.entries.push(Entry {
                    rel,
                    abs,
                    size: 0, // sized later in second pass (much cheaper than re-walking
                    // from the outer walker; one sequential walk per .git)
                    kind: Kind::GitDir,
                });
                return WalkState::Skip;
            }

            // AppleDouble (._* and .DS_Store).
            let is_appledouble = file_name.starts_with("._") || file_name == ".DS_Store";
            if is_appledouble {
                if opts.skip_appledouble {
                    return WalkState::Continue;
                }
                if ft.is_file() {
                    if let Ok(md) = entry.metadata() {
                        batch.entries.push(Entry {
                            rel,
                            abs,
                            size: md.len(),
                            kind: Kind::AppleDouble,
                        });
                    }
                }
                return WalkState::Continue;
            }

            // Apply include/exclude (relative path match).
            let rel_str = rel.to_string_lossy();
            if exc.is_match(rel_str.as_ref()) {
                return WalkState::Continue;
            }
            if has_inc && !inc.is_match(rel_str.as_ref()) {
                return WalkState::Continue;
            }

            if ft.is_dir() {
                if !rel.as_os_str().is_empty() {
                    batch.entries.push(Entry {
                        rel,
                        abs,
                        size: 0,
                        kind: Kind::Dir,
                    });
                }
            } else if ft.is_symlink() {
                batch.entries.push(Entry {
                    rel,
                    abs,
                    size: 0,
                    kind: Kind::Symlink,
                });
            } else if ft.is_file() {
                let md = entry.metadata().ok();
                if opts.update && dest_is_newer(&opts.dest_root, &rel, md.as_ref()) {
                    return WalkState::Continue; // -u: dest newer, skip
                }
                let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
                batch.entries.push(Entry {
                    rel,
                    abs,
                    size,
                    kind: Kind::File,
                });
            }
            WalkState::Continue
        })
    });

    let mut collected = entries.into_inner().unwrap();
    let git_roots = git_roots.into_inner().unwrap();

    // Second pass: size each .git in one walk. Parallel across multiple
    // .git roots if many exist.
    if !git_roots.is_empty() {
        use std::sync::Arc;
        let sizes: Arc<Mutex<std::collections::HashMap<PathBuf, u64>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        std::thread::scope(|s| {
            for git_root in &git_roots {
                let sizes = Arc::clone(&sizes);
                s.spawn(move || {
                    let size = git_dir_size_once(git_root);
                    sizes.lock().unwrap().insert(git_root.clone(), size);
                });
            }
        });
        let sizes = sizes.lock().unwrap();
        for e in collected.iter_mut() {
            if matches!(e.kind, Kind::GitDir) {
                if let Some(sz) = sizes.get(&e.abs) {
                    e.size = *sz;
                }
            }
        }
    }

    out.extend(collected);
    Ok(())
}

/// Convert a walked absolute path into the tar member path, honoring rsync's
/// trailing-slash convention: `src/ dst` copies contents, `src dst` copies the
/// top-level `src` directory itself.
fn relative_output_path(root: &Path, abs: &Path, trailing_slash: bool) -> PathBuf {
    let inner = abs.strip_prefix(root).unwrap_or(abs);
    if trailing_slash {
        return inner.to_path_buf();
    }
    match root.file_name() {
        Some(name) if inner.as_os_str().is_empty() => PathBuf::from(name),
        Some(name) => PathBuf::from(name).join(inner),
        None => inner.to_path_buf(),
    }
}

/// rsync `-u` predicate: true when `dest_root/rel` exists and its mtime is
/// strictly newer than the source file's. Any metadata/time error is treated
/// as "not newer" so the file is still transferred (safe default).
fn dest_is_newer(dest_root: &Path, rel: &Path, src_md: Option<&std::fs::Metadata>) -> bool {
    let src_mt = match src_md.and_then(|m| m.modified().ok()) {
        Some(t) => t,
        None => return false,
    };
    match std::fs::metadata(dest_root.join(rel)).and_then(|m| m.modified()) {
        Ok(dst_mt) => dst_mt > src_mt,
        Err(_) => false, // dest absent or unreadable -> transfer
    }
}

fn build_globset(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p)?);
        if !p.contains('/') {
            b.add(Glob::new(&format!("**/{p}"))?);
            b.add(Glob::new(&format!("**/{p}/**"))?);
        }
    }
    Ok(b.build()?)
}

/// Sum file sizes under `p` in one walk (uses walkdir for the .git subtree
/// only; outer walker already pruned this path).
fn git_dir_size_once(p: &Path) -> u64 {
    let mut total: u64 = 0;
    for e in walkdir::WalkDir::new(p)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if e.file_type().is_file() {
            if let Ok(md) = e.metadata() {
                total = total.saturating_add(md.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::relative_output_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn trailing_slash_copies_source_contents() {
        assert_eq!(
            relative_output_path(Path::new("/tmp/src"), Path::new("/tmp/src/a/b.txt"), true),
            PathBuf::from("a/b.txt")
        );
    }

    #[test]
    fn no_trailing_slash_preserves_source_directory() {
        assert_eq!(
            relative_output_path(Path::new("/tmp/src"), Path::new("/tmp/src/a/b.txt"), false),
            PathBuf::from("src/a/b.txt")
        );
    }

    #[test]
    fn file_source_uses_file_name() {
        assert_eq!(
            relative_output_path(
                Path::new("/tmp/file.txt"),
                Path::new("/tmp/file.txt"),
                false
            ),
            PathBuf::from("file.txt")
        );
    }
}
