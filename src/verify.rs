//! verify.rs — Check a kernel.org tarball's signature, following
//! https://www.kernel.org/signature.html: the `.tar.sign` is a detached
//! signature over the uncompressed tar, made by a kernel.org developer whose
//! key is published over WKD on kernel.org.
//!
//! No key ships with Kernel Pop. At verify time the keys of the developers
//! who sign releases (Linus Torvalds and Greg Kroah-Hartman) are fetched over
//! WKD into a throwaway GNUPGHOME, exported to a keyring of their own, and
//! `gpgv` checks the decompressed tarball (streamed on stdin) against that
//! keyring alone. A signature that names another @kernel.org signer gets
//! that signer's key fetched as well.

use crate::source::Tarball;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The kernel.org developers whose keys are always fetched.
pub const SIGNER_ADDRESSES: [&str; 2] = ["torvalds@kernel.org", "gregkh@kernel.org"];

/// Who signed a tarball.
#[derive(Debug, Clone, PartialEq)]
pub struct Signer {
    /// "Greg Kroah-Hartman <gregkh@kernel.org>"
    pub uid: String,
    /// Primary-key fingerprint.
    pub fingerprint: String,
}

impl Signer {
    /// The fingerprint in groups of four, as signature.html prints them.
    pub fn grouped_fingerprint(&self) -> String {
        self.fingerprint
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What gpgv's `--status-fd` output says about one signature.
#[derive(Debug, Clone, PartialEq)]
pub enum GpgvVerdict {
    /// GOODSIG and VALIDSIG; the primary key's fingerprint.
    Good { primary_fpr: String },
    /// The keyring does not have the signing key (its long key ID).
    MissingKey { keyid: String },
    /// Anything else: bad, expired, revoked, unreadable.
    Bad { reason: String },
}

pub fn parse_gpgv_status(status: &str) -> GpgvVerdict {
    let mut good = false;
    let mut valid: Option<String> = None;
    let mut missing: Option<String> = None;
    let mut reason: Option<String> = None;
    for line in status.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 2 || f[0] != "[GNUPG:]" {
            continue;
        }
        match f[1] {
            "GOODSIG" => good = true,
            "VALIDSIG" if f.len() >= 3 => valid = f.last().map(|s| s.to_uppercase()),
            "NO_PUBKEY" if f.len() >= 3 => missing = Some(f[2].to_uppercase()),
            "BADSIG" => reason = Some("the signature does not match the tarball (BADSIG)".into()),
            "EXPKEYSIG" => reason = Some("the signing key has expired (EXPKEYSIG)".into()),
            "REVKEYSIG" => reason = Some("the signing key has been revoked (REVKEYSIG)".into()),
            "EXPSIG" => reason = Some("the signature has expired (EXPSIG)".into()),
            "NODATA" => reason = Some("the .tar.sign file holds no signature (NODATA)".into()),
            "ERRSIG" => {
                reason.get_or_insert_with(|| "the signature could not be checked (ERRSIG)".into());
            }
            _ => {}
        }
    }
    match (good, valid, missing, reason) {
        (true, Some(primary_fpr), _, None) => GpgvVerdict::Good { primary_fpr },
        (_, _, Some(keyid), _) => GpgvVerdict::MissingKey { keyid },
        (_, _, _, Some(reason)) => GpgvVerdict::Bad { reason },
        _ => GpgvVerdict::Bad { reason: "gpgv did not report a good signature".into() },
    }
}

/// The "signer's user ID" subpacket of a signature, from `gpg --list-packets`
/// ("(signer's user ID: Some One <x@kernel.org>)"), if the signature has one.
pub fn signer_uid_from_packets(packets: &str) -> Option<String> {
    let start = packets.find("signer's user ID: ")? + "signer's user ID: ".len();
    let rest = &packets[start..];
    Some(rest[..rest.find(')')?].trim().to_string())
}

/// The e-mail address in a user ID ("Name <a@b>" -> "a@b"), lowercased.
pub fn uid_address(uid: &str) -> Option<String> {
    let inner = match (uid.rfind('<'), uid.rfind('>')) {
        (Some(a), Some(b)) if a < b => &uid[a + 1..b],
        _ => uid.trim(),
    };
    (inner.contains('@') && !inner.contains(char::is_whitespace)).then(|| inner.to_ascii_lowercase())
}

/// A private (0700) scratch directory, removed on drop.
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new(prefix: &str) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
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

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A throwaway GNUPGHOME. Its gpg-agent and dirmngr are stopped on drop.
struct GnupgHome {
    dir: ScratchDir,
}

impl GnupgHome {
    fn new() -> Result<Self> {
        Ok(GnupgHome { dir: ScratchDir::new("kernel-pop-gnupg")? })
    }

    fn gpg(&self) -> Command {
        let mut c = Command::new("gpg");
        c.arg("--homedir").arg(self.dir.path()).args(["--batch", "--no-tty"]).stdin(Stdio::null());
        c
    }

    /// Fetch the keys for `addresses` over WKD only. Returns gpg's messages.
    fn locate_keys(&self, addresses: &[&str]) -> Result<String> {
        let out = self
            .gpg()
            .args(["--auto-key-locate", "clear,nodefault,wkd", "--locate-keys"])
            .args(addresses)
            .output()
            .context("Could not run gpg to fetch the kernel.org signing keys (install the gnupg package)")?;
        Ok(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }

    /// Export every key in the home to a binary keyring file.
    fn export(&self, to: &Path) -> Result<()> {
        let out = self.gpg().arg("--export").output().context("Could not run gpg --export")?;
        if !out.status.success() || out.stdout.is_empty() {
            bail!("No kernel.org signing key could be fetched over WKD — is the network up?");
        }
        std::fs::write(to, &out.stdout).with_context(|| format!("Could not write {}", to.display()))
    }

    /// The first user ID of the key with this primary fingerprint.
    fn uid_of(&self, fpr: &str) -> Option<String> {
        let out = self.gpg().args(["--with-colons", "--list-keys", fpr]).output().ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.starts_with("uid:"))
            .and_then(|l| l.split(':').nth(9))
            .map(|s| s.to_string())
    }

    fn signer_uid(&self, sig: &Path) -> Option<String> {
        let out = self.gpg().arg("--list-packets").arg(sig).output().ok()?;
        signer_uid_from_packets(&String::from_utf8_lossy(&out.stdout))
    }
}

impl Drop for GnupgHome {
    fn drop(&mut self) {
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(self.dir.path())
            .args(["--kill", "all"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Run gpgv on `sig` over the decompressed `tarball` against `keyring`
/// only, with an empty private home so no user keyring is consulted.
pub fn gpgv_tarball(tarball: &Tarball, sig: &Path, keyring: &Path) -> Result<GpgvVerdict> {
    let scratch = ScratchDir::new("kernel-pop-gpgv")?;
    let mut decompress = tarball
        .compression
        .decompress_command(&tarball.path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not run the decompressor (xz-utils / gzip)")?;
    let tar_stream = decompress.stdout.take().context("no decompressor output")?;
    let gpgv = Command::new("gpgv")
        .arg("--homedir")
        .arg(scratch.path())
        .args(["--status-fd", "1", "--keyring"])
        .arg(keyring)
        .arg(sig)
        .arg("-")
        .stdin(Stdio::from(tar_stream))
        .output();
    let dec = decompress.wait_with_output().context("Could not wait for the decompressor")?;
    let gpgv = gpgv.context("Could not run gpgv (install the gpgv package)")?;
    if !dec.status.success() {
        return Ok(GpgvVerdict::Bad {
            reason: format!(
                "the tarball is damaged and could not be decompressed ({})",
                String::from_utf8_lossy(&dec.stderr).trim()
            ),
        });
    }
    Ok(parse_gpgv_status(&String::from_utf8_lossy(&gpgv.stdout)))
}

/// Verify a kernel.org tarball against its `.tar.sign`, fetching the
/// signers' keys over WKD. `log` gets one line per step.
pub fn verify_tarball(tarball: &Tarball, sig: &Path, log: &dyn Fn(String)) -> Result<Signer> {
    let gnupg = GnupgHome::new()?;
    let ring_dir = ScratchDir::new("kernel-pop-keyring")?;
    let ring = ring_dir.path().join("kernel.org.gpg");

    log(format!("Fetching the kernel.org signing keys over WKD ({})…", SIGNER_ADDRESSES.join(", ")));
    let msgs = gnupg.locate_keys(&SIGNER_ADDRESSES)?;
    for l in msgs.lines().filter(|l| (l.contains("imported") && l.contains("key ")) || l.contains("error")) {
        log(format!("  {l}"));
    }
    // Another @kernel.org developer may have signed it; their key is only
    // fetched when the signature itself names them.
    if let Some(addr) = gnupg.signer_uid(sig).as_deref().and_then(uid_address) {
        if addr.ends_with("@kernel.org") && !SIGNER_ADDRESSES.contains(&addr.as_str()) {
            log(format!("The signature names {addr}; fetching that key over WKD too…"));
            gnupg.locate_keys(&[addr.as_str()])?;
        }
    }
    gnupg.export(&ring)?;

    log(format!("Checking {} against {}…", tarball.path.display(), sig.display()));
    match gpgv_tarball(tarball, sig, &ring)? {
        GpgvVerdict::Good { primary_fpr } => {
            let uid = gnupg.uid_of(&primary_fpr).unwrap_or_else(|| "unknown user ID".into());
            Ok(Signer { uid, fingerprint: primary_fpr })
        }
        GpgvVerdict::MissingKey { keyid } => bail!(
            "The signature was made by key {keyid}, which is not the key kernel.org publishes for {}",
            SIGNER_ADDRESSES.join(" or ")
        ),
        GpgvVerdict::Bad { reason } => bail!("The signature check failed: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::tests::{compress_to, scratch, tar_bytes, MAKEFILE};

    #[test]
    fn gpgv_status_parsing() {
        let fp = "647F28654894E3BD457199BE38DBBDC86092693E";
        let good = format!(
            "[GNUPG:] NEWSIG\n[GNUPG:] KEY_CONSIDERED {fp} 0\n[GNUPG:] GOODSIG 38DBBDC86092693E Greg Kroah-Hartman <gregkh@kernel.org>\n\
             [GNUPG:] VALIDSIG {fp} 2026-10-21 1792571661 0 4 0 1 10 00 {fp}\n"
        );
        assert_eq!(parse_gpgv_status(&good), GpgvVerdict::Good { primary_fpr: fp.into() });
        let missing = "[GNUPG:] NEWSIG\n[GNUPG:] ERRSIG 38DBBDC86092693E 1 10 00 1792571661 9 647F\n[GNUPG:] NO_PUBKEY 38DBBDC86092693E\n";
        assert_eq!(parse_gpgv_status(missing), GpgvVerdict::MissingKey { keyid: "38DBBDC86092693E".into() });
        let bad = "[GNUPG:] NEWSIG\n[GNUPG:] BADSIG 38DBBDC86092693E Greg\n";
        assert!(matches!(parse_gpgv_status(bad), GpgvVerdict::Bad { reason } if reason.contains("BADSIG")));
        let expired = format!("[GNUPG:] EXPKEYSIG 38DB Greg\n[GNUPG:] VALIDSIG {fp} x\n");
        assert!(matches!(parse_gpgv_status(&expired), GpgvVerdict::Bad { reason } if reason.contains("expired")));
        let revoked = format!("[GNUPG:] REVKEYSIG 38DB Greg\n[GNUPG:] VALIDSIG {fp} x\n");
        assert!(matches!(parse_gpgv_status(&revoked), GpgvVerdict::Bad { .. }));
        // GOODSIG alone, without VALIDSIG, is not enough
        assert!(matches!(parse_gpgv_status("[GNUPG:] GOODSIG 38DB Greg\n"), GpgvVerdict::Bad { .. }));
        assert!(matches!(parse_gpgv_status(""), GpgvVerdict::Bad { .. }));
    }

    #[test]
    fn signer_uid_subpacket_and_addresses() {
        let p = "\thashed subpkt 28 len 19 (signer's user ID: Sasha Levin <sashal@kernel.org>)\n";
        assert_eq!(signer_uid_from_packets(p).as_deref(), Some("Sasha Levin <sashal@kernel.org>"));
        assert_eq!(signer_uid_from_packets("\thashed subpkt 33 len 21 (issuer fpr v4 647F)\n"), None);
        assert_eq!(uid_address("Sasha Levin <SashaL@kernel.org>").as_deref(), Some("sashal@kernel.org"));
        assert_eq!(uid_address("gregkh@kernel.org").as_deref(), Some("gregkh@kernel.org"));
        assert_eq!(uid_address("no address"), None);
        let s = Signer { uid: String::new(), fingerprint: "647F28654894E3BD457199BE38DBBDC86092693E".into() };
        assert_eq!(s.grouped_fingerprint(), "647F 2865 4894 E3BD 4571 99BE 38DB BDC8 6092 693E");
    }

    /// A throwaway signing key in its own GNUPGHOME, exported as a keyring.
    struct TestKey {
        home: ScratchDir,
        ring: PathBuf,
    }

    impl TestKey {
        fn new() -> Self {
            let home = ScratchDir::new("kernel-pop-test-gpg").unwrap();
            let gpg = |args: &[&str]| {
                let out = Command::new("gpg")
                    .arg("--homedir")
                    .arg(home.path())
                    .args(["--batch", "--no-tty"])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "gpg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
                out.stdout
            };
            gpg(&["--pinentry-mode", "loopback", "--passphrase", "", "--quick-gen-key", "Kernel Pop test <test@invalid>", "rsa2048", "sign", "never"]);
            let ring = home.path().join("ring.gpg");
            std::fs::write(&ring, gpg(&["--export"])).unwrap();
            TestKey { home, ring }
        }

        /// Armored detached signature over `data`, like kernel.org's .tar.sign.
        fn sign(&self, data: &[u8], sig: &Path) {
            use std::io::Write;
            let mut c = Command::new("gpg")
                .arg("--homedir")
                .arg(self.home.path())
                .args(["--batch", "--no-tty", "--yes", "--armor", "--detach-sign", "-o"])
                .arg(sig)
                .arg("-")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.take().unwrap().write_all(data).unwrap();
            assert!(c.wait().unwrap().success());
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let _ = Command::new("gpgconf").arg("--homedir").arg(self.home.path()).args(["--kill", "all"]).status();
        }
    }

    #[test]
    fn tarball_signatures_are_checked_over_the_uncompressed_tar() {
        let d = scratch("gpgv");
        let key = TestKey::new();
        let tar = tar_bytes("linux-7.2.12", MAKEFILE);
        let sig = d.join("linux-7.2.12.tar.sign");
        key.sign(&tar, &sig);

        // One signature covers the .xz and the .gz alike.
        for prog in ["xz", "gzip"] {
            let path = d.join(format!("linux-7.2.12.tar.{prog}"));
            compress_to(prog, &tar, &path);
            let t = crate::source::inspect(&path).unwrap();
            assert!(matches!(gpgv_tarball(&t, &sig, &key.ring).unwrap(), GpgvVerdict::Good { .. }), "{prog}");
        }
        let xz = d.join("linux-7.2.12.tar.xz");
        let t = crate::source::inspect(&xz).unwrap();

        // One byte of the tree changed: the signature no longer matches.
        let mut changed = tar.clone();
        let pos = changed.windows(6).position(|w| w == b"Linux ").unwrap();
        changed[pos] = b'l';
        compress_to("xz", &changed, &xz);
        assert!(matches!(gpgv_tarball(&t, &sig, &key.ring).unwrap(), GpgvVerdict::Bad { .. }));

        // One byte of the compressed file changed: it no longer decompresses.
        compress_to("xz", &tar, &xz);
        let mut raw = std::fs::read(&xz).unwrap();
        let mid = raw.len() / 2;
        raw[mid] ^= 0x01;
        std::fs::write(&xz, &raw).unwrap();
        assert!(matches!(gpgv_tarball(&t, &sig, &key.ring).unwrap(), GpgvVerdict::Bad { .. }));

        // Signed by a key that is not in the keyring.
        compress_to("xz", &tar, &xz);
        let other = TestKey::new();
        assert!(matches!(gpgv_tarball(&t, &sig, &other.ring).unwrap(), GpgvVerdict::MissingKey { .. }));

        // Not a signature at all.
        std::fs::write(&sig, b"not a signature").unwrap();
        assert!(matches!(gpgv_tarball(&t, &sig, &key.ring).unwrap(), GpgvVerdict::Bad { .. }));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn scratch_dir_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let d = ScratchDir::new("kernel-pop-test").unwrap();
        let p = d.path().to_path_buf();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
        drop(d);
        assert!(!p.exists());
    }

    /// The real tarball with one byte changed fails: once in the tar
    /// itself (BADSIG), once in the compressed file (it no longer
    /// decompresses). KERNEL_POP_LIVE_TARBALL=<.tar.xz>, .tar.sign next to it.
    #[test]
    #[ignore]
    fn live_tampered_kernel_org_tarball_fails() {
        let path = PathBuf::from(std::env::var("KERNEL_POP_LIVE_TARBALL").expect("set KERNEL_POP_LIVE_TARBALL"));
        let t = crate::source::inspect(&path).unwrap();
        let sig = t.sibling_signature().expect("a .tar.sign next to the tarball");
        let d = scratch("live-tamper");
        let name = path.file_name().unwrap().to_str().unwrap();
        let stem = name.strip_suffix(".xz").unwrap();
        std::fs::copy(&sig, d.join(format!("{stem}.sign"))).unwrap();

        // One byte of the tar changed (in the middle), recompressed as .gz.
        let mut tar = Command::new("xz").arg("-dc").arg(&path).output().unwrap().stdout;
        let mid = tar.len() / 2;
        tar[mid] ^= 0x20;
        let gz = d.join(format!("{stem}.gz"));
        let mut c = Command::new("gzip").arg("-1c").stdin(Stdio::piped()).stdout(std::fs::File::create(&gz).unwrap()).spawn().unwrap();
        std::io::Write::write_all(&mut c.stdin.take().unwrap(), &tar).unwrap();
        assert!(c.wait().unwrap().success());
        drop(tar);
        let tg = crate::source::inspect(&gz).unwrap();
        let err = verify_tarball(&tg, &tg.sibling_signature().unwrap(), &|l| println!("{l}")).unwrap_err().to_string();
        println!("one byte of the tar changed: {err}");
        assert!(err.contains("BADSIG"), "{err}");
        std::fs::remove_file(&gz).unwrap();

        // One byte of the .xz changed.
        let mut raw = std::fs::read(&path).unwrap();
        let mid = raw.len() / 2;
        raw[mid] ^= 0x01;
        let xz = d.join(name);
        std::fs::write(&xz, &raw).unwrap();
        let tx = Tarball { path: xz.clone(), ..t };
        let err = verify_tarball(&tx, &tx.sibling_signature().unwrap(), &|l| println!("{l}")).unwrap_err().to_string();
        println!("one byte of the .xz changed: {err}");
        assert!(err.contains("damaged"), "{err}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// The real procedure against a real kernel.org tarball and signature,
    /// with the keys fetched over WKD: KERNEL_POP_LIVE_TARBALL=<.tar.xz>
    /// (its .tar.sign next to it).
    #[test]
    #[ignore]
    fn live_kernel_org_tarball_verifies() {
        let path = PathBuf::from(std::env::var("KERNEL_POP_LIVE_TARBALL").expect("set KERNEL_POP_LIVE_TARBALL"));
        let t = crate::source::inspect(&path).unwrap();
        let sig = t.sibling_signature().expect("a .tar.sign next to the tarball");
        let signer = verify_tarball(&t, &sig, &|l| println!("{l}")).expect("should verify");
        println!("signed by {} ({})", signer.uid, signer.grouped_fingerprint());
        assert!(signer.uid.contains("@kernel.org"));
    }
}
