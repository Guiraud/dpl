use crate::plan::Bin;
use crate::scan::Kind;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// One row of the manifest, produced after a bin has been packed and written.
#[derive(Debug, Serialize, Clone)]
pub struct ChunkReport {
    pub id: usize,
    pub archive: String,
    pub file_count: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub blake3: String,
    pub elapsed_secs: f64,
}

/// Sink that tees bytes to an inner writer and a blake3 hasher.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
    bytes_written: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
        }
    }

    /// Consume and return (inner writer, final hash, bytes written).
    pub fn into_parts(self) -> (W, blake3::Hash, u64) {
        let hash = self.hasher.finalize();
        (self.inner, hash, self.bytes_written)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
            self.bytes_written += n as u64;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Pack a single bin into any byte sink as a `tar.zst` stream.
///
/// Pipeline: tar::Builder -> zstd::Encoder(multithreaded) -> HashingWriter -> `W`.
/// Blake3 hashes the *compressed* bytes so resume/verify can check what landed.
/// Returns the inner writer (so the caller can flush/sync/close it) plus the
/// report. This is the shared core behind both the local-file and S3 sinks.
pub fn pack_bin_into<W: Write>(
    bin: &Bin,
    writer: W,
    zstd_level: i32,
    zstd_threads: u32,
    mut on_byte: impl FnMut(u64),
) -> Result<(W, ChunkReport)> {
    let started = std::time::Instant::now();

    let hashing = HashingWriter::new(writer);
    let mut encoder = zstd::Encoder::new(hashing, zstd_level)?;
    if zstd_threads > 1 {
        encoder.multithread(zstd_threads)?;
    }

    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    // tar::HeaderMode::Complete preserves all metadata (perms, mtime, uid/gid).
    builder.mode(tar::HeaderMode::Complete);

    let mut file_count: u64 = 0;
    let mut uncompressed: u64 = 0;

    for entry in &bin.entries {
        let name = &entry.rel;
        let abs = &entry.abs;
        match entry.kind {
            Kind::Dir => {
                builder
                    .append_dir(name, abs)
                    .with_context(|| format!("append_dir {}", abs.display()))?;
            }
            Kind::GitDir => {
                builder
                    .append_dir_all(name, abs)
                    .with_context(|| format!("append_dir_all {}", abs.display()))?;
                file_count += count_files_under(abs);
            }
            Kind::File | Kind::Symlink | Kind::AppleDouble => {
                builder
                    .append_path_with_name(abs, name)
                    .with_context(|| format!("append_path_with_name {}", abs.display()))?;
                file_count += 1;
            }
        }
        uncompressed += entry.size;
        on_byte(entry.size);
    }

    // Unwrap layers, finalize hash.
    let encoder = builder.into_inner().context("tar::Builder::into_inner")?;
    let hashing = encoder.finish().context("zstd::Encoder::finish")?;
    let (writer, hash, compressed) = hashing.into_parts();

    let report = ChunkReport {
        id: bin.id,
        archive: bin.archive.clone(),
        file_count,
        uncompressed_bytes: uncompressed,
        compressed_bytes: compressed,
        blake3: hash.to_hex().to_string(),
        elapsed_secs: started.elapsed().as_secs_f64(),
    };
    Ok((writer, report))
}

/// Pack a single bin into its `tar.zst` archive at a local `archive_path`,
/// fsync'ing the result (tolerating ENOTSUP on SMB/NFS/exFAT).
pub fn pack_bin(
    bin: &Bin,
    archive_path: &Path,
    zstd_level: i32,
    zstd_threads: u32,
    on_byte: impl FnMut(u64),
) -> Result<ChunkReport> {
    let file = File::create(archive_path)
        .with_context(|| format!("creating {}", archive_path.display()))?;
    let buf_writer = BufWriter::with_capacity(8 * 1024 * 1024, file);

    let (buf_writer, report) = pack_bin_into(bin, buf_writer, zstd_level, zstd_threads, on_byte)?;

    let file = buf_writer
        .into_inner()
        .map_err(|e| anyhow::anyhow!("BufWriter into_inner: {}", e.into_error()))?;
    // Durably flush. Many SMB/NFS mounts and exFAT reject fsync with ENOTSUP/
    // EOPNOTSUPP — bytes are still written, so tolerate it instead of aborting.
    // SMB is dpl's primary target. (Rust's std does not map these errnos to
    // ErrorKind::Unsupported, so match the raw OS error directly.)
    if let Err(e) = file.sync_all() {
        if !fsync_unsupported(&e) {
            return Err(anyhow::Error::new(e).context("fsync"));
        }
    }

    Ok(report)
}

/// True when an fsync error means "this filesystem doesn't support fsync"
/// rather than a real write failure. SMB/NFS/exFAT return ENOTSUP (45 on
/// macOS, 95 on Linux) or EOPNOTSUPP (102 on macOS); Rust's std does not map
/// these to `ErrorKind::Unsupported`, so check the raw errno too.
pub(crate) fn fsync_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    matches!(e.raw_os_error(), Some(45) | Some(95) | Some(102))
}

/// Count files reachable under `p` (for accurate per-bin file_count when a
/// bin holds a .git atomic unit and `entry.size` was a recursive sum).
fn count_files_under(p: &Path) -> u64 {
    walkdir::WalkDir::new(p)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .count() as u64
}
