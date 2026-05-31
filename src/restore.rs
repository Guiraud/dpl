//! Read-side operations over the `chunk_*.tar.zst` archives a transfer wrote:
//! `extract` (restore the tree), `list` (enumerate stored paths) and `grep`
//! (search file contents). All three locate the archive directory, then fan
//! the chunks out across worker threads — chunk decode is the cost and chunks
//! are independent, so a work-stealing pool keeps every core busy even when
//! chunk sizes are wildly uneven (one 3 GiB oversize bin next to a 35 KiB
//! leftover).

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Options shared by the read-modes.
pub struct RestoreOpts {
    pub threads: usize,
    /// Verify each chunk's BLAKE3 against the manifest before use.
    pub verify: bool,
    pub quiet: bool,
}

// ── manifest (only the fields the read side needs) ───────────────────────
#[derive(Deserialize)]
struct ManifestLite {
    chunks: Vec<ChunkLite>,
}
#[derive(Deserialize)]
struct ChunkLite {
    archive: String,
    blake3: String,
}

/// Resolve a user-supplied path to the directory that actually holds the
/// `chunk_*.tar.zst` files. Accepts either that directory directly or a parent
/// containing a `.dpl/` (the layout a transfer writes under `<DST>/.dpl`).
fn resolve_archive_dir(p: &Path) -> Result<PathBuf> {
    if has_chunks(p) {
        return Ok(p.to_path_buf());
    }
    let dotdpl = p.join(".dpl");
    if has_chunks(&dotdpl) {
        return Ok(dotdpl);
    }
    bail!(
        "no chunk_*.tar.zst found in {} or {}/.dpl",
        p.display(),
        p.display()
    );
}

fn is_chunk(name: &str) -> bool {
    name.starts_with("chunk_") && name.ends_with(".tar.zst")
}

fn has_chunks(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().pipe(|n| is_chunk(&n)))
        })
        .unwrap_or(false)
}

/// Sorted list of chunk archives in `dir` (stable order = manifest order).
fn list_chunks(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(is_chunk)
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    if v.is_empty() {
        bail!("no chunks in {}", dir.display());
    }
    Ok(v)
}

/// Open a chunk as a streaming `tar::Archive` over a zstd decoder.
fn open_chunk(path: &Path) -> Result<tar::Archive<zstd::Decoder<'static, BufReader<File>>>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let dec = zstd::Decoder::new(f).with_context(|| format!("zstd decode {}", path.display()))?;
    Ok(tar::Archive::new(dec))
}

/// BLAKE3 of a file's bytes, streamed (no full read into memory).
fn hash_file(path: &Path) -> Result<String> {
    let mut f = BufReader::new(File::open(path)?);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Verify every chunk against the manifest in `dir` (if present). Errors on the
/// first mismatch. A missing manifest is a hard error here because the caller
/// explicitly asked for verification.
fn verify_chunks(dir: &Path, chunks: &[PathBuf], quiet: bool) -> Result<()> {
    let mpath = dir.join("manifest.json");
    let txt = std::fs::read_to_string(&mpath)
        .with_context(|| format!("--verify needs a manifest, none at {}", mpath.display()))?;
    let manifest: ManifestLite =
        serde_json::from_str(&txt).with_context(|| format!("parsing {}", mpath.display()))?;
    let want: HashMap<&str, &str> = manifest
        .chunks
        .iter()
        .map(|c| (c.archive.as_str(), c.blake3.as_str()))
        .collect();

    for c in chunks {
        let name = c.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        match want.get(name) {
            Some(expected) => {
                let got = hash_file(c)?;
                if &got != expected {
                    bail!("blake3 mismatch for {name}: manifest {expected}, file {got}");
                }
            }
            None => bail!("{name} not in manifest"),
        }
    }
    if !quiet {
        eprintln!("verify: {} chunk(s) match manifest blake3", chunks.len());
    }
    Ok(())
}

/// Run `f` over every chunk across `threads` workers (work-stealing). The first
/// error wins and is returned after all workers drain.
fn par_chunks<F>(chunks: &[PathBuf], threads: usize, f: F) -> Result<()>
where
    F: Fn(&Path) -> Result<()> + Sync,
{
    let next = AtomicUsize::new(0);
    let err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let n = threads.max(1).min(chunks.len().max(1));

    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= chunks.len() || err.lock().unwrap().is_some() {
                    break;
                }
                if let Err(e) = f(&chunks[i]) {
                    *err.lock().unwrap() = Some(e);
                    break;
                }
            });
        }
    });

    match err.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Build a matcher from a single user pattern. A pattern without `/` also
/// matches by basename anywhere in the tree (mirrors the scan-side excludes).
fn matcher(pattern: &str) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    b.add(Glob::new(pattern)?);
    if !pattern.contains('/') {
        b.add(Glob::new(&format!("**/{pattern}"))?);
    }
    Ok(b.build()?)
}

// ── extract ──────────────────────────────────────────────────────────────

/// Decompress every chunk in `archive` back into `out`, rebuilding the original
/// tree (paths in the tar are relative to the source root). Permissions and
/// mtimes are restored; existing files are overwritten. The `tar` crate rejects
/// `..`/absolute members, so a malicious archive can't escape `out`.
pub fn extract(archive: &Path, out: &Path, opts: &RestoreOpts) -> Result<()> {
    let dir = resolve_archive_dir(archive)?;
    let chunks = list_chunks(&dir)?;
    if opts.verify {
        verify_chunks(&dir, &chunks, opts.quiet)?;
    }
    std::fs::create_dir_all(out).with_context(|| format!("creating out dir {}", out.display()))?;

    let files = AtomicUsize::new(0);
    par_chunks(&chunks, opts.threads, |chunk| {
        let mut ar = open_chunk(chunk)?;
        ar.set_preserve_permissions(true);
        ar.set_preserve_mtime(true);
        ar.set_overwrite(true);
        // Unpack entry-by-entry so we can count files and attribute errors.
        for entry in ar
            .entries()
            .with_context(|| format!("reading {}", chunk.display()))?
        {
            let mut e = entry?;
            if e.unpack_in(out)? {
                files.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    })?;

    if !opts.quiet {
        eprintln!(
            "extracted {} entr{} from {} chunk(s) -> {}",
            files.load(Ordering::Relaxed),
            if files.load(Ordering::Relaxed) == 1 {
                "y"
            } else {
                "ies"
            },
            chunks.len(),
            out.display()
        );
    }
    Ok(())
}

// ── list ───────────────────────────────────────────────────────────────────

/// Enumerate stored regular-file paths (optionally glob-filtered), sorted, with
/// sizes. Reads tar headers only — chunk bodies are streamed past, not buffered.
pub fn list(archive: &Path, pattern: Option<&str>, opts: &RestoreOpts) -> Result<()> {
    let dir = resolve_archive_dir(archive)?;
    let chunks = list_chunks(&dir)?;
    if opts.verify {
        verify_chunks(&dir, &chunks, opts.quiet)?;
    }
    let set = pattern.map(matcher).transpose()?;

    let found: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());
    par_chunks(&chunks, opts.threads, |chunk| {
        let mut ar = open_chunk(chunk)?;
        let mut local: Vec<(PathBuf, u64)> = Vec::new();
        for entry in ar.entries()? {
            let e = entry?;
            if e.header().entry_type().is_dir() {
                continue;
            }
            let path = e.path()?.to_path_buf();
            if let Some(s) = &set {
                if !s.is_match(path.to_string_lossy().as_ref()) {
                    continue;
                }
            }
            local.push((path, e.header().size().unwrap_or(0)));
        }
        found.lock().unwrap().extend(local);
        Ok(())
    })?;

    let mut rows = found.into_inner().unwrap();
    rows.sort();
    let out = std::io::stdout();
    let mut w = std::io::BufWriter::new(out.lock());
    for (path, size) in &rows {
        writeln!(w, "{size:>12}  {}", path.display())?;
    }
    if !opts.quiet {
        eprintln!("{} file(s)", rows.len());
    }
    Ok(())
}

// ── grep ─────────────────────────────────────────────────────────────────

/// Search a literal `term` inside every stored file. Binary files (NUL in the
/// first 8 KiB) are skipped, like `grep -I`. Output: `path:lineno: line`.
pub fn grep(archive: &Path, term: &str, opts: &RestoreOpts) -> Result<()> {
    let dir = resolve_archive_dir(archive)?;
    let chunks = list_chunks(&dir)?;
    if opts.verify {
        verify_chunks(&dir, &chunks, opts.quiet)?;
    }
    let needle = term.as_bytes();
    if needle.is_empty() {
        bail!("empty search term");
    }

    let hits = AtomicUsize::new(0);
    let stdout = Mutex::new(std::io::BufWriter::new(std::io::stdout()));

    par_chunks(&chunks, opts.threads, |chunk| {
        let mut ar = open_chunk(chunk)?;
        let mut buf: Vec<u8> = Vec::new();
        let mut out_local = String::new();
        let mut local_hits = 0usize;

        for entry in ar.entries()? {
            let mut e = entry?;
            if !e.header().entry_type().is_file() {
                continue;
            }
            let path = e.path()?.to_path_buf();
            buf.clear();
            e.read_to_end(&mut buf)?;
            // Skip binaries.
            let probe = &buf[..buf.len().min(8192)];
            if probe.contains(&0) {
                continue;
            }
            for (lineno, line) in buf.split(|&b| b == b'\n').enumerate() {
                if find_sub(line, needle).is_some() {
                    let text = String::from_utf8_lossy(line);
                    let text = text.trim_end_matches('\r');
                    let shown: String = text.chars().take(200).collect();
                    out_local.push_str(&format!("{}:{}: {}\n", path.display(), lineno + 1, shown));
                    local_hits += 1;
                }
            }
        }
        if !out_local.is_empty() {
            let mut w = stdout.lock().unwrap();
            w.write_all(out_local.as_bytes())?;
        }
        hits.fetch_add(local_hits, Ordering::Relaxed);
        Ok(())
    })?;

    stdout.into_inner().unwrap().flush()?;
    if !opts.quiet {
        eprintln!("{} match(es)", hits.load(Ordering::Relaxed));
    }
    Ok(())
}

/// First index of `needle` in `hay` (naive; lines are short).
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Tiny `Tap`-style helper so `has_chunks` reads cleanly.
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}
