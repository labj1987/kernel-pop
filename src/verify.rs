//! verify.rs — Check that a mainline CHECKSUMS file is signed by Ubuntu's
//! kernel PPA key, so the SHA256 sums the downloads are checked against are
//! Canonical's and not whatever a server or network in between sent.
//!
//! The key ships in the binary (`data/kernel-ppa.gpg`, a binary OpenPGP
//! export of "Kernel PPA <kernel-ppa@canonical.com>") and `gpgv` checks the
//! signature against that keyring alone. The privileged script repeats the
//! same check as root with the copy installed next to it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Primary-key fingerprint of Ubuntu's kernel PPA signing key.
pub const KERNEL_PPA_FINGERPRINT: &str = "60AA7B6F30434AE68E569963E50C6A0917C622B0";

/// The keyring holding only that key.
const KERNEL_PPA_KEYRING: &[u8] = include_bytes!("../data/kernel-ppa.gpg");

/// Verify `signature` (CHECKSUMS.gpg) over `checksums` (CHECKSUMS) against
/// Ubuntu's kernel PPA key.
pub fn verify_checksums_signature(checksums: &[u8], signature: &[u8]) -> Result<()> {
    verify_detached(KERNEL_PPA_KEYRING, KERNEL_PPA_FINGERPRINT, checksums, signature)
}

/// Whether gpgv's `--status-fd` output reports a valid signature whose
/// primary key is `fingerprint`. VALIDSIG's last field is the primary key's
/// fingerprint (the first is the signing (sub)key's).
pub fn status_has_valid_signature(status: &str, fingerprint: &str) -> bool {
    status.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() >= 3
            && fields[0] == "[GNUPG:]"
            && fields[1] == "VALIDSIG"
            && fields.last().is_some_and(|f| f.eq_ignore_ascii_case(fingerprint))
    })
}

/// A private (0700) scratch directory, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "kernel-pop-verify-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            nanos
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("Could not create {}", path.display()))?;
        Ok(ScratchDir(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Verify a detached signature with gpgv against `keyring` only (an empty
/// private home, so no user keyring is consulted), requiring the signature's
/// primary key to be `fingerprint`.
fn verify_detached(keyring: &[u8], fingerprint: &str, data: &[u8], signature: &[u8]) -> Result<()> {
    let dir = ScratchDir::new()?;
    let home = dir.path().join("home");
    std::fs::create_dir(&home)?;
    let ring = dir.path().join("keyring.gpg");
    let data_path = dir.path().join("CHECKSUMS");
    let sig_path = dir.path().join("CHECKSUMS.gpg");
    std::fs::write(&ring, keyring)?;
    std::fs::write(&data_path, data)?;
    std::fs::write(&sig_path, signature)?;

    let out = Command::new("gpgv")
        .arg("--homedir")
        .arg(&home)
        .args(["--status-fd", "1", "--keyring"])
        .arg(&ring)
        .arg(&sig_path)
        .arg(&data_path)
        .output()
        .context("Could not run gpgv to check the CHECKSUMS signature (install the gpgv package)")?;
    let status = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !status_has_valid_signature(&status, fingerprint) {
        bail!(
            "The CHECKSUMS file is not signed by Ubuntu's kernel PPA key — not using it.\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKSUMS: &[u8] = include_bytes!("testdata/v6.17-amd64-CHECKSUMS");
    const SIGNATURE: &[u8] = include_bytes!("testdata/v6.17-amd64-CHECKSUMS.gpg");

    #[test]
    fn real_mainline_signature_verifies() {
        verify_checksums_signature(CHECKSUMS, SIGNATURE).expect("v6.17 CHECKSUMS should verify");
    }

    #[test]
    fn tampered_checksums_are_refused() {
        let mut changed = CHECKSUMS.to_vec();
        // Change the first hex digit of the linux-image package's SHA256.
        let hash = b"9484aeadaa7438a080dbd2d24d219d361b62ed18f44907e7bd4e81651f296bc7";
        let pos = changed.windows(hash.len()).position(|w| w == hash).expect("fixture hash");
        assert_eq!(changed[pos], b'9');
        changed[pos] = b'8';
        assert!(verify_checksums_signature(&changed, SIGNATURE).is_err());
        assert!(verify_checksums_signature(CHECKSUMS, b"not a signature").is_err());
        assert!(verify_checksums_signature(CHECKSUMS, b"").is_err());
    }

    #[test]
    fn a_different_pinned_key_is_refused() {
        assert!(verify_detached(KERNEL_PPA_KEYRING, &"0".repeat(40), CHECKSUMS, SIGNATURE).is_err());
        assert!(verify_detached(b"", KERNEL_PPA_FINGERPRINT, CHECKSUMS, SIGNATURE).is_err());
    }

    #[test]
    fn validsig_parsing_pins_the_primary_key() {
        let fp = KERNEL_PPA_FINGERPRINT;
        let good = format!(
            "[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG E50C6A0917C622B0 Kernel PPA\n\
             [GNUPG:] VALIDSIG {fp} 2025-09-28 1759101330 0 4 0 1 10 00 {fp}\n"
        );
        assert!(status_has_valid_signature(&good, fp));
        assert!(status_has_valid_signature(&good.replace(fp, &fp.to_lowercase()), fp));
        let other_primary = format!("[GNUPG:] VALIDSIG {fp} 2025-09-28 1 0 4 0 1 10 00 {}\n", "A".repeat(40));
        assert!(!status_has_valid_signature(&other_primary, fp));
        assert!(!status_has_valid_signature("[GNUPG:] GOODSIG E50C6A0917C622B0 x\n", fp));
        assert!(!status_has_valid_signature("[GNUPG:] BADSIG E50C6A0917C622B0 x\n", fp));
        assert!(!status_has_valid_signature("", fp));
    }

    #[test]
    fn scratch_dir_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let d = ScratchDir::new().unwrap();
        let p = d.path().to_path_buf();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
        drop(d);
        assert!(!p.exists());
    }
}
