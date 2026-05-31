//! S3 (and S3-compatible) destination support.
//!
//! A `dpl` transfer can write its `chunk_*.tar.zst` archives + manifest to an
//! `s3://bucket/prefix` target instead of a local mount. Two upload paths:
//!
//!   - **temp-local (default)**: each chunk is packed to a local temp file
//!     (so blake3 + size are known and the upload can be retried from a stable
//!     source), then streamed to S3 with multipart, then the temp is removed.
//!   - **streaming (`--nodisk`)**: the tar+zstd stream is piped straight into
//!     a multipart upload, no local spill. Needs no scratch space; a packer
//!     thread feeds an `os_pipe` that the uploader drains concurrently.
//!
//! Credentials come from the standard chain (env `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY`, `~/.aws/credentials`, then instance metadata).
//! Region from `AWS_REGION` / `AWS_DEFAULT_REGION`; for S3-compatible stores
//! set `AWS_ENDPOINT_URL` (or `S3_ENDPOINT`) and path-style is enabled.

use crate::archive::{pack_bin_into, ChunkReport};
use crate::plan::Bin;
use anyhow::{bail, Context, Result};
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use std::io::Read;
use std::path::Path;
use tokio::runtime::Runtime;

/// A resolved S3 destination: a ready bucket client, the key prefix every
/// chunk/manifest is written under, and a tokio runtime to drive the async
/// S3 client from `dpl`'s synchronous packing loop.
pub struct S3Dest {
    bucket: Box<Bucket>,
    /// Key prefix without trailing slash; may be empty (bucket root).
    prefix: String,
    rt: Runtime,
}

/// True if `uri` names an S3 destination.
pub fn is_s3(uri: &str) -> bool {
    uri.starts_with("s3://")
}

/// Parse `s3://bucket[/prefix...]` and build an authenticated bucket client.
pub fn parse(uri: &str) -> Result<S3Dest> {
    let rest = uri.strip_prefix("s3://").context("not an s3:// uri")?;
    let (bucket_name, prefix) = match rest.split_once('/') {
        Some((b, p)) => (b, p.trim_end_matches('/')),
        None => (rest, ""),
    };
    if bucket_name.is_empty() {
        bail!("s3 uri missing bucket name: {uri}");
    }

    let creds = Credentials::default().context(
        "no AWS credentials found (set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY, \
         or configure ~/.aws/credentials)",
    )?;
    let (region, custom_endpoint) = region_from_env()?;
    let bucket = Bucket::new(bucket_name, region, creds).context("creating S3 bucket client")?;
    // S3-compatible stores (MinIO, Ceph, Backblaze…) generally need path-style
    // addressing; real AWS uses virtual-host style (the default).
    let bucket = if custom_endpoint {
        bucket.with_path_style()
    } else {
        bucket
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building tokio runtime for S3")?;

    Ok(S3Dest {
        bucket,
        prefix: prefix.to_string(),
        rt,
    })
}

/// Resolve the region (and whether a custom endpoint is in play).
fn region_from_env() -> Result<(Region, bool)> {
    let endpoint = std::env::var("AWS_ENDPOINT_URL")
        .or_else(|_| std::env::var("S3_ENDPOINT"))
        .ok();
    if let Some(endpoint) = endpoint {
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        return Ok((Region::Custom { region, endpoint }, true));
    }
    let name = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .context("set AWS_REGION (or AWS_ENDPOINT_URL for an S3-compatible store)")?;
    let region: Region = name.parse().context("parsing AWS_REGION")?;
    Ok((region, false))
}

impl S3Dest {
    /// Full object key for a chunk/manifest `name` under the prefix.
    fn key(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", self.prefix, name)
        }
    }

    /// Human-readable `s3://…` location for a name (for logs).
    pub fn uri(&self, name: &str) -> String {
        format!("s3://{}/{}", self.bucket.name(), self.key(name))
    }

    /// Upload raw bytes (used for the manifest). Single PUT.
    pub fn put_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let key = self.key(name);
        let resp = self
            .rt
            .block_on(self.bucket.put_object(&key, bytes))
            .map_err(|e| anyhow::anyhow!("uploading {}: {e}", self.uri(name)))?;
        ensure_2xx(resp.status_code(), &key)
    }

    /// Upload an existing local file via multipart (16 MiB parts).
    pub fn put_file(&self, name: &str, path: &Path) -> Result<()> {
        let f = std::fs::File::open(path)
            .with_context(|| format!("opening temp chunk {}", path.display()))?;
        self.upload_reader(name, std::io::BufReader::new(f))
    }

    /// Pack a bin straight into a multipart upload — no local scratch file.
    /// A packer thread writes the tar+zstd stream into a pipe; this thread
    /// drains the pipe into S3 part by part as the bytes arrive.
    pub fn put_bin_streaming(
        &self,
        bin: &Bin,
        zstd_level: i32,
        zstd_threads: u32,
    ) -> Result<ChunkReport> {
        let (reader, writer) = os_pipe::pipe().context("creating upload pipe")?;

        std::thread::scope(|scope| -> Result<ChunkReport> {
            // Packer: feed the pipe, then drop the writer so the reader sees EOF.
            let packer = scope.spawn(move || -> Result<ChunkReport> {
                let (w, report) = pack_bin_into(bin, writer, zstd_level, zstd_threads, |_| {})?;
                drop(w); // close write end -> reader gets EOF
                Ok(report)
            });

            // Uploader drains the pipe concurrently.
            self.upload_reader(&bin.archive, reader)
                .with_context(|| format!("streaming {}", self.uri(&bin.archive)))?;

            packer
                .join()
                .map_err(|_| anyhow::anyhow!("packer thread panicked"))?
        })
    }

    /// Multipart-upload everything `reader` yields under object `name`.
    /// Reads fixed `PART_SIZE` parts with a synchronous `Read` (works for both
    /// a temp `File` and the streaming `os_pipe`), uploading each part through
    /// the async client on the embedded runtime. On any error the upload is
    /// aborted so no orphaned parts are billed.
    fn upload_reader<R: Read>(&self, name: &str, mut reader: R) -> Result<()> {
        const PART_SIZE: usize = 16 * 1024 * 1024; // 16 MiB (>= S3's 5 MiB min)
        let key = self.key(name);
        let ct = "application/octet-stream".to_string();

        let init = self
            .rt
            .block_on(self.bucket.initiate_multipart_upload(&key, &ct))
            .map_err(|e| anyhow::anyhow!("initiating multipart for {}: {e}", self.uri(name)))?;
        let upload_id = init.upload_id;

        let result = (|| -> Result<()> {
            let mut parts: Vec<s3::serde_types::Part> = Vec::new();
            let mut part_number: u32 = 1;
            loop {
                let mut buf = vec![0u8; PART_SIZE];
                let n = read_full(&mut reader, &mut buf)?;
                // Stop at EOF — but always send at least one (possibly empty)
                // part so zero-byte objects still complete.
                if n == 0 && part_number > 1 {
                    break;
                }
                buf.truncate(n);
                let part = self
                    .rt
                    .block_on(self.bucket.put_multipart_chunk(
                        buf,
                        &key,
                        part_number,
                        &upload_id,
                        &ct,
                    ))
                    .map_err(|e| anyhow::anyhow!("uploading part {part_number} of {key}: {e}"))?;
                parts.push(part);
                part_number += 1;
                if n < PART_SIZE {
                    break;
                }
            }
            self.rt
                .block_on(
                    self.bucket
                        .complete_multipart_upload(&key, &upload_id, parts),
                )
                .map_err(|e| anyhow::anyhow!("completing multipart for {key}: {e}"))?;
            Ok(())
        })();

        if result.is_err() {
            // Best-effort cleanup so a failed upload leaves no billable parts.
            let _ = self.rt.block_on(self.bucket.abort_upload(&key, &upload_id));
        }
        result
    }
}

/// Treat any non-2xx S3 status as a hard error.
fn ensure_2xx(code: u16, key: &str) -> Result<()> {
    if (200..300).contains(&code) {
        Ok(())
    } else {
        bail!("S3 returned HTTP {code} for {key}")
    }
}

/// Read until `buf` is full or EOF; returns bytes read (`< buf.len()` => EOF).
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading chunk stream"),
        }
    }
    Ok(filled)
}
