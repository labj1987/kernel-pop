//! download.rs — Download files (a kernel.org tarball and its signature)
//! with progress, cancel and retries, and hash files with SHA256.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::io::AsyncWriteExt;

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
/// write, so downloaded and built files and their directories get these
/// modes explicitly rather than whatever the umask gives.
pub const DIR_MODE: u32 = 0o755;
pub const FILE_MODE: u32 = 0o644;

pub async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await.with_context(|| format!("Could not set the mode of {}", path.display()))
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

/// Resolves once `cancel` is set.
async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// `future`, or `Cancelled` as soon as `cancel` is set: a request or a read
/// that hangs on the network must not hold up a cancel (closing the window
/// waits for it).
async fn or_cancel<T>(cancel: &AtomicBool, future: impl std::future::Future<Output = T>) -> Result<T> {
    tokio::select! {
        biased;
        () = cancelled(cancel) => Err(Cancelled.into()),
        value = future => Ok(value),
    }
}

async fn download_one<F>(client: &reqwest::Client, url: &str, dest: &Path, mut progress_cb: F, cancel: &Arc<AtomicBool>) -> Result<()>
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
            or_cancel(cancel, tokio::time::sleep(std::time::Duration::from_secs(wait))).await?;
        }

        let resp = match or_cancel(cancel, client.get(url).send()).await? {
            Ok(r) => r,
            Err(e) => {
                last_err = e.into();
                continue;
            }
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
            Err(e) => {
                last_err = e.into();
                continue;
            }
        };

        let mut failed = false;
        loop {
            let chunk = match or_cancel(cancel, stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(e);
                }
            };
            match chunk {
                Ok(bytes) => {
                    if let Err(e) = file.write_all(&bytes).await {
                        last_err = e.into();
                        failed = true;
                        break;
                    }
                    downloaded += bytes.len() as u64;
                    progress_cb(downloaded, total);
                }
                Err(e) => {
                    last_err = e.into();
                    failed = true;
                    break;
                }
            }
        }

        if !failed {
            // A stream that ends early is not a success just because no
            // error surfaced: compare against the advertised length.
            if let Some(expected) = total {
                if downloaded != expected {
                    last_err = anyhow::anyhow!("Incomplete download: got {downloaded} of {expected} bytes");
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

/// One file to download: where from, and its name in the target directory.
#[derive(Debug, Clone)]
pub struct Remote {
    pub url: String,
    pub filename: String,
}

/// Download `files` into `dest_dir`. Each file is written as `<name>.part`
/// and only renamed into place once it is complete, so a failed or
/// cancelled download never leaves a partial file under the real name.
/// Returns the paths of the downloaded files, in order.
pub async fn download_files<F>(files: Vec<Remote>, dest_dir: PathBuf, mut progress_cb: F, cancel: Arc<AtomicBool>) -> Result<Vec<PathBuf>>
where
    F: FnMut(Progress) + Send + 'static,
{
    tokio::fs::create_dir_all(&dest_dir).await.with_context(|| format!("Could not create download directory: {}", dest_dir.display()))?;
    set_mode(&dest_dir, DIR_MODE).await?;

    let client = crate::kernelorg::client()?;
    let count = files.len();
    let mut paths = vec![];

    for (i, f) in files.iter().enumerate() {
        if f.filename.contains('/') || f.filename.starts_with('.') || f.filename.is_empty() {
            bail!("Refusing an unusual file name from the server: {:?}", f.filename);
        }
        let dest = dest_dir.join(&f.filename);
        let part = part_path(&dest_dir, &f.filename);
        let filename = f.filename.clone();

        download_one(
            &client,
            &f.url,
            &part,
            |downloaded, total| {
                progress_cb(Progress { file_index: i, file_count: count, filename: filename.clone(), downloaded, total });
            },
            &cancel,
        )
        .await
        .with_context(|| format!("Failed to download {}", f.filename))?;

        set_mode(&part, FILE_MODE).await?;
        tokio::fs::rename(&part, &dest).await.with_context(|| format!("Could not move {} into place", f.filename))?;
        paths.push(dest);
    }
    Ok(paths)
}

/// Where `download_files` writes `filename` in `dir` until it is complete.
pub fn part_path(dir: &Path, filename: &str) -> PathBuf {
    dir.join(format!("{filename}.part"))
}

/// The `.part` files a download of `files` into `dir` writes.
pub fn part_paths(files: &[Remote], dir: &Path) -> Vec<PathBuf> {
    files.iter().map(|f| part_path(dir, &f.filename)).collect()
}

/// Deletes `parts`, the `.part` files of one download (`part_paths`), when
/// closing gave up waiting for that download to stop. Only those: another
/// copy without the single-instance socket may be downloading into the same
/// folder. Only regular files are touched. Returns the deleted paths.
pub fn remove_parts(parts: &[PathBuf]) -> Vec<PathBuf> {
    parts
        .iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file()))
        .filter(|path| std::fs::remove_file(path).is_ok())
        .cloned()
        .collect()
}

/// Deletes the app's own `.part` files in `dir`: what a kernel.org download
/// (`linux-<version>.tar.<xz|sign>`) writes before it is complete. Called at
/// startup, before this copy downloads anything and only when it holds the
/// single-instance socket, so each is left over from an unfinished one.
/// Anything else there (a browser's download, say) is left alone, and only
/// regular files are touched. Returns the deleted paths.
pub fn remove_stale_parts(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(own_part))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| std::fs::remove_file(path).is_ok())
        .collect()
}

/// Whether `name` is a `.part` file this app writes: `linux-*.tar.*.part`.
fn own_part(name: &str) -> bool {
    name.strip_suffix(".part")
        .and_then(|name| name.strip_prefix("linux-"))
        .is_some_and(|rest| rest.split_once(".tar.").is_some_and(|(version, ext)| !version.is_empty() && !ext.is_empty()))
}

/// SHA256 of a file as lowercase hex, streaming in 1 MiB chunks so large
/// packages are never held in memory all at once.
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("Could not open {} to hash it", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).with_context(|| format!("Read error while hashing {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_recognised_through_context() {
        let e: anyhow::Error = Cancelled.into();
        let wrapped = Err::<(), _>(e).context("Failed to download linux-7.2.12.tar.xz").context("outer").unwrap_err();
        assert!(is_cancelled(&wrapped));
        assert!(!is_cancelled(&anyhow::anyhow!("Download cancelled")));
    }

    #[test]
    fn leftover_part_files_are_removed_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("kernel-pop-test-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build.part")).unwrap();
        for name in [
            "linux-7.2.tar.xz.part",
            "linux-7.2.tar.sign.part",
            "linux-7.1.tar.xz",
            "notes.part.txt",
            ".part",
            "firefox-setup.tar.bz2.part",
            "linux-.tar.xz.part",
            "video.mp4.part",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let mut removed: Vec<_> = remove_stale_parts(&dir).into_iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        removed.sort();
        assert_eq!(removed, ["linux-7.2.tar.sign.part", "linux-7.2.tar.xz.part"]);
        for kept in ["linux-7.1.tar.xz", "notes.part.txt", ".part", "build.part", "firefox-setup.tar.bz2.part", "linux-.tar.xz.part", "video.mp4.part"] {
            assert!(dir.join(kept).exists(), "{kept}");
        }
        assert!(remove_stale_parts(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn closing_deletes_only_its_own_downloads_part_files() {
        let dir = std::env::temp_dir().join(format!("kernel-pop-test-own-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<Remote> = ["linux-7.2.tar.xz", "linux-7.2.tar.sign"]
            .into_iter()
            .map(|name| Remote { url: format!("https://cdn.kernel.org/{name}"), filename: name.to_string() })
            .collect();
        let parts = part_paths(&files, &dir);
        assert_eq!(parts, [dir.join("linux-7.2.tar.xz.part"), dir.join("linux-7.2.tar.sign.part")]);
        // This download got as far as its tarball; another one (another copy) writes 7.1.
        std::fs::write(&parts[0], b"x").unwrap();
        std::fs::write(dir.join("linux-7.1.tar.xz.part"), b"x").unwrap();
        assert_eq!(remove_parts(&parts), [parts[0].clone()], "the missing .sign.part is no error");
        assert!(!parts[0].exists());
        assert!(dir.join("linux-7.1.tar.xz.part").exists(), "another download's .part is left alone");
        // A directory under that name is not a file to delete.
        std::fs::create_dir(&parts[1]).unwrap();
        assert!(remove_parts(&parts).is_empty());
        assert!(parts[1].is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_cancel_ends_a_hung_request_at_once() {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let result = or_cancel(&cancel, std::future::pending::<()>()).await;
        assert!(is_cancelled(&result.unwrap_err()));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(or_cancel(&AtomicBool::new(false), async { 7 }).await.unwrap(), 7);
    }

    #[test]
    fn sha256_of_a_file() {
        let path = std::env::temp_dir().join(format!("kernel-pop-test-{}.bin", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        std::fs::remove_file(&path).unwrap();
        assert!(sha256_file(&path).is_err());
    }

    #[tokio::test]
    async fn odd_file_names_are_refused() {
        let dir = std::env::temp_dir().join(format!("kernel-pop-test-dl-{}", std::process::id()));
        for name in ["../x", ".hidden", ""] {
            let files = vec![Remote { url: "https://invalid.test/x".into(), filename: name.into() }];
            let err = download_files(files, dir.clone(), |_| {}, Arc::new(AtomicBool::new(false))).await.unwrap_err();
            assert!(err.to_string().contains("unusual file name"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Downloads the real current stable signature (about 1 KB) and checks
    /// the modes. Run with --ignored.
    #[tokio::test]
    #[ignore]
    async fn live_download_writes_files_with_modes() {
        use std::os::unix::fs::PermissionsExt;
        let rel = crate::kernelorg::fetch_releases().await.unwrap();
        let stable = rel.iter().find(|r| r.moniker == crate::kernelorg::Moniker::Stable).unwrap();
        let url = stable.pgp.clone().unwrap();
        let name = crate::kernelorg::url_file_name(&url).unwrap().to_string();
        let dest = std::env::temp_dir().join("kernel-pop-live-download");
        let paths = download_files(vec![Remote { url, filename: name }], dest.clone(), |_| {}, Arc::new(AtomicBool::new(false))).await.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest), 0o755);
        assert_eq!(mode(&paths[0]), 0o644);
        assert!(std::fs::read_to_string(&paths[0]).unwrap().contains("BEGIN PGP SIGNATURE"));
        std::fs::remove_dir_all(&dest).unwrap();
    }
}
