//! download.rs — Download a kernel's .deb set with progress, cancel,
//! retries, and SHA256 verification.

use crate::versions::KernelDeb;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_SECS: u64 = 3;

/// Progress event for the whole set.
#[derive(Debug, Clone)]
pub struct Progress {
    pub file_index: usize,
    pub file_count: usize,
    pub filename: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}

fn client() -> Result<reqwest::Client> {
    // connect_timeout + read_timeout instead of a total request timeout:
    // a total timeout would kill a slow-but-healthy download; read_timeout
    // only fires when the stream actually stalls.
    Ok(reqwest::Client::builder()
        .user_agent(concat!("kernelpop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?)
}

async fn download_one<F>(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    mut progress_cb: F,
    cancel: &Arc<AtomicBool>,
) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    let mut last_err = anyhow::anyhow!("unknown error");

    for attempt in 0..MAX_RETRIES {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        if attempt > 0 {
            let wait = RETRY_BASE_SECS * (2u64.pow(attempt - 1));
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => { last_err = e.into(); continue; }
        };
        if !resp.status().is_success() {
            last_err = anyhow::anyhow!("HTTP {}", resp.status());
            continue;
        }

        let total = resp.content_length();
        let mut downloaded: u64 = 0;
        let mut stream = resp.bytes_stream();

        let mut file = match tokio::fs::File::create(dest).await {
            Ok(f) => f,
            Err(e) => { last_err = e.into(); continue; }
        };

        let mut failed = false;
        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                bail!("Download cancelled");
            }
            match chunk {
                Ok(bytes) => {
                    if let Err(e) = file.write_all(&bytes).await {
                        last_err = e.into(); failed = true; break;
                    }
                    downloaded += bytes.len() as u64;
                    progress_cb(downloaded, total);
                }
                Err(e) => { last_err = e.into(); failed = true; break; }
            }
        }

        if !failed {
            // A stream that ends early is not a success just because no
            // error surfaced: compare against the advertised length.
            if let Some(expected) = total {
                if downloaded != expected {
                    last_err = anyhow::anyhow!(
                        "Incomplete download: got {downloaded} of {expected} bytes"
                    );
                    failed = true;
                }
            }
        }
        if !failed {
            file.flush().await?;
            return Ok(());
        }
        let _ = tokio::fs::remove_file(dest).await;
    }

    Err(last_err).context(format!("Download failed after {} attempts", MAX_RETRIES))
}

/// Download every .deb into `dest_dir`, verifying each against its SHA256
/// when one is published. A failed verification deletes the file and aborts
/// the whole set. Returns the paths of the downloaded files.
pub async fn download_deb_set<F>(
    debs: Vec<KernelDeb>,
    dest_dir: PathBuf,
    mut progress_cb: F,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<PathBuf>>
where
    F: FnMut(Progress) + Send + 'static,
{
    tokio::fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("Could not create download directory: {}", dest_dir.display()))?;

    let client = client()?;
    let count = debs.len();
    let mut paths = vec![];

    for (i, deb) in debs.iter().enumerate() {
        let dest = dest_dir.join(&deb.filename);
        let filename = deb.filename.clone();

        download_one(
            &client,
            &deb.url,
            &dest,
            |downloaded, total| {
                progress_cb(Progress {
                    file_index: i,
                    file_count: count,
                    filename: filename.clone(),
                    downloaded,
                    total,
                });
            },
            &cancel,
        )
        .await
        .with_context(|| format!("Failed to download {}", deb.filename))?;

        if let Some(expected) = &deb.sha256 {
            if let Err(e) = verify_sha256(&dest, expected).await {
                let _ = tokio::fs::remove_file(&dest).await;
                return Err(e).with_context(|| {
                    format!("SHA256 verification failed for {} — file deleted", deb.filename)
                });
            }
        }

        paths.push(dest);
    }

    Ok(paths)
}

/// Verify SHA256 of a file against an expected hex string, streaming in
/// 1 MiB chunks so large debs are never held in memory all at once.
pub async fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("Could not open file for SHA256 verification")?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = file
            .read(&mut buf)
            .await
            .context("Read error during SHA256 verification")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let result = hex::encode(hasher.finalize());
    if result.to_lowercase() != expected.to_lowercase() {
        bail!(
            "SHA256 mismatch:\n  expected: {}\n  got:      {}",
            expected, result
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verify_sha256_accepts_match_and_rejects_mismatch() {
        let path = std::env::temp_dir().join(format!("kernelpop-test-{}.bin", std::process::id()));
        tokio::fs::write(&path, b"hello").await.unwrap();
        // sha256("hello")
        let good = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(&path, good).await.is_ok());
        assert!(verify_sha256(&path, &"0".repeat(64)).await.is_err());
        let _ = tokio::fs::remove_file(&path).await;
    }
}
