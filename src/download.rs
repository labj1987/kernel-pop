//! download.rs — Download a kernel's .deb set with progress, cancel,
//! retries, and SHA256 verification against the signed CHECKSUMS.

use crate::versions::DebSet;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_SECS: u64 = 3;

/// The error a download ends with when the user cancelled it. Recognise it
/// with `err.downcast_ref::<Cancelled>()`, which works through any context
/// added on the way up.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Download cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// True when `err` is (or wraps) a cancelled download.
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Cancelled>().is_some()
}

/// The privileged script refuses files that anyone but their owner can
/// write, so the download directory and files get these modes explicitly
/// rather than whatever the umask gives.
const DIR_MODE: u32 = 0o755;
const FILE_MODE: u32 = 0o644;

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .await
        .with_context(|| format!("Could not set the mode of {}", path.display()))
}

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
        .user_agent(concat!("kernel-pop/", env!("CARGO_PKG_VERSION")))
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
            return Err(Cancelled.into());
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
                return Err(Cancelled.into());
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

/// Download every .deb of `set` into `dest_dir`, verifying each against its
/// SHA256 from the signed CHECKSUMS. Each file is written as `<name>.part`
/// and only renamed into place once it verified, so a failed or cancelled
/// download never leaves a bad file under a package's name. Anything left in
/// `dest_dir` from an earlier download that is not part of this set is
/// removed, and CHECKSUMS and CHECKSUMS.gpg are written last, so a
/// directory holding them holds a complete, verified set: the privileged
/// script installs exactly those files after checking them again itself.
/// Returns the paths of the downloaded packages.
pub async fn download_deb_set<F>(
    set: DebSet,
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
    set_mode(&dest_dir, DIR_MODE).await?;
    remove_stale_files(&dest_dir, &set).await?;

    let client = client()?;
    let count = set.debs.len();
    let mut paths = vec![];

    for (i, deb) in set.debs.iter().enumerate() {
        let dest = dest_dir.join(&deb.filename);
        let part = dest_dir.join(format!("{}.part", deb.filename));
        let filename = deb.filename.clone();

        download_one(
            &client,
            &deb.url,
            &part,
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

        if let Err(e) = verify_sha256(&part, &deb.sha256).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e).with_context(|| {
                format!("SHA256 verification failed for {} — file deleted", deb.filename)
            });
        }
        set_mode(&part, FILE_MODE).await?;
        tokio::fs::rename(&part, &dest)
            .await
            .with_context(|| format!("Could not move {} into place", deb.filename))?;
        paths.push(dest);
    }

    for (name, bytes) in [("CHECKSUMS", &set.checksums), ("CHECKSUMS.gpg", &set.checksums_sig)] {
        let path = dest_dir.join(name);
        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("Could not write {}", path.display()))?;
        set_mode(&path, FILE_MODE).await?;
    }

    Ok(paths)
}

/// Whether a file in the download directory belongs to an earlier download
/// and should go: package files (and their `.part`s) not in this set, and
/// the CHECKSUMS pair, which is rewritten once the set is complete.
fn is_stale(name: &str, set: &DebSet) -> bool {
    if name == "CHECKSUMS" || name == "CHECKSUMS.gpg" {
        return true;
    }
    if name.ends_with(".part") {
        return true;
    }
    name.ends_with(".deb") && !set.debs.iter().any(|d| d.filename == name)
}

async fn remove_stale_files(dir: &Path, set: &DebSet) -> Result<()> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("Could not read {}", dir.display()))?;
    while let Some(e) = entries.next_entry().await? {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_stale(name, set) && e.file_type().await.is_ok_and(|t| t.is_file() || t.is_symlink()) {
            tokio::fs::remove_file(e.path())
                .await
                .with_context(|| format!("Could not remove the old file {}", e.path().display()))?;
        }
    }
    Ok(())
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

    use crate::versions::KernelDeb;

    #[test]
    fn cancellation_is_recognised_through_context() {
        let e: anyhow::Error = Cancelled.into();
        let wrapped = Err::<(), _>(e)
            .context("Failed to download linux-image.deb")
            .context("outer")
            .unwrap_err();
        assert!(is_cancelled(&wrapped));
        assert!(!is_cancelled(&anyhow::anyhow!("Download cancelled")));
    }

    fn set_of(names: &[&str]) -> DebSet {
        DebSet {
            debs: names
                .iter()
                .map(|n| KernelDeb { filename: n.to_string(), url: String::new(), sha256: "0".repeat(64) })
                .collect(),
            checksums: b"sums".to_vec(),
            checksums_sig: b"sig".to_vec(),
        }
    }

    #[test]
    fn stale_files_are_old_packages_parts_and_checksums() {
        let set = set_of(&["linux-image-a.deb", "linux-modules-a.deb"]);
        assert!(!is_stale("linux-image-a.deb", &set));
        assert!(is_stale("linux-image-old-build.deb", &set));
        assert!(is_stale("linux-image-a.deb.part", &set));
        assert!(is_stale("CHECKSUMS", &set));
        assert!(is_stale("CHECKSUMS.gpg", &set));
        assert!(!is_stale("notes.txt", &set));
    }

    #[tokio::test]
    async fn stale_files_are_removed_and_modes_set() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kernel-pop-test-stale-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        for f in ["linux-image-a.deb", "linux-image-old.deb", "x.deb.part", "CHECKSUMS", "notes.txt"] {
            tokio::fs::write(dir.join(f), b"x").await.unwrap();
        }
        remove_stale_files(&dir, &set_of(&["linux-image-a.deb"])).await.unwrap();
        let mut left = vec![];
        let mut rd = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            left.push(e.file_name().into_string().unwrap());
        }
        left.sort();
        assert_eq!(left, ["linux-image-a.deb", "notes.txt"]);
        set_mode(&dir.join("notes.txt"), FILE_MODE).await.unwrap();
        let mode = std::fs::metadata(dir.join("notes.txt")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// Downloads the real v6.17 set (about 150 MB) into
    /// $KERNEL_POP_LIVE_DEST (kept, for running the privileged script's
    /// verification on it: KERNEL_POP_TEST_REAL_DEBS) or a temp dir.
    #[tokio::test]
    #[ignore]
    async fn live_download_writes_a_verified_set() {
        use std::os::unix::fs::PermissionsExt;
        let v = crate::versions::KernelVersion {
            version: "6.17".into(),
            url: format!("{}v6.17/", crate::versions::MAINLINE_BASE),
            is_rc: false,
        };
        let keep = std::env::var_os("KERNEL_POP_LIVE_DEST").map(PathBuf::from);
        let dest = keep.clone().unwrap_or_else(|| std::env::temp_dir().join("kernel-pop-live-download"));
        let set = crate::versions::fetch_deb_list(&v).await.unwrap();
        let paths = download_deb_set(set, dest.clone(), |_| {}, Arc::new(AtomicBool::new(false))).await.unwrap();
        assert_eq!(paths.len(), 4);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest), 0o755);
        for p in paths.iter().chain([&dest.join("CHECKSUMS"), &dest.join("CHECKSUMS.gpg")]) {
            assert_eq!(mode(p), 0o644, "{}", p.display());
        }
        if keep.is_none() {
            std::fs::remove_dir_all(&dest).unwrap();
        }
    }

    #[tokio::test]
    async fn verify_sha256_accepts_match_and_rejects_mismatch() {
        let path = std::env::temp_dir().join(format!("kernel-pop-test-{}.bin", std::process::id()));
        tokio::fs::write(&path, b"hello").await.unwrap();
        // sha256("hello")
        let good = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(&path, good).await.is_ok());
        assert!(verify_sha256(&path, &"0".repeat(64)).await.is_err());
        let _ = tokio::fs::remove_file(&path).await;
    }
}
