//! source.rs — A kernel source tarball: how it is compressed (from its
//! magic bytes) and which kernel it holds (from the top-level Makefile's
//! VERSION, PATCHLEVEL, SUBLEVEL and EXTRAVERSION). Neither is ever taken
//! from the file name: a renamed or patched tree is still read correctly.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Compression {
    Xz,
    Gzip,
}

impl Compression {
    /// Sniff the compression from the first bytes of the file.
    pub fn from_magic(head: &[u8]) -> Option<Compression> {
        if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
            Some(Compression::Xz)
        } else if head.starts_with(&[0x1F, 0x8B]) {
            Some(Compression::Gzip)
        } else {
            None
        }
    }

    /// The command that writes the uncompressed tar of `path` to stdout.
    pub fn decompress_command(self, path: &Path) -> Command {
        let mut cmd = Command::new(match self {
            Compression::Xz => "xz",
            Compression::Gzip => "gzip",
        });
        cmd.arg("-dc").arg("--").arg(path);
        cmd
    }
}

/// The kernel version a tree's top-level Makefile declares.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceVersion {
    pub version: u32,
    pub patchlevel: u32,
    pub sublevel: u32,
    /// "" or e.g. "-rc1"
    pub extraversion: String,
}

impl SourceVersion {
    /// What `make kernelversion` prints: "7.2.12", "7.3.0", "7.4.0-rc1".
    pub fn kernelversion(&self) -> String {
        format!("{}.{}.{}{}", self.version, self.patchlevel, self.sublevel, self.extraversion)
    }

    /// kernel.org's name for the release: "7.2.12", "7.3", "7.4-rc1"
    /// (a .0 sublevel is left out), as in linux-7.3.tar.xz.
    pub fn release_name(&self) -> String {
        if self.sublevel == 0 {
            format!("{}.{}{}", self.version, self.patchlevel, self.extraversion)
        } else {
            format!("{}.{}.{}{}", self.version, self.patchlevel, self.sublevel, self.extraversion)
        }
    }

    /// The Debian package version for a build of it (KDEB_PKGVERSION):
    /// "7.2.12-1"; a pre-release sorts before its release ("7.4.0~rc1-1").
    pub fn deb_version(&self) -> String {
        let upstream: String = self
            .kernelversion()
            .chars()
            .map(|c| match c {
                '-' | '_' => '~',
                c => c,
            })
            .collect();
        format!("{upstream}-1")
    }
}

/// Read VERSION, PATCHLEVEL, SUBLEVEL and EXTRAVERSION from a top-level
/// kernel Makefile ("VERSION = 7" lines; the first of each counts).
pub fn parse_makefile(text: &str) -> Result<SourceVersion> {
    let field = |name: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == name && !k.contains(':') && !k.contains('?')).then(|| v.trim().to_string())
        })
    };
    let number = |name: &str| -> Result<u32> {
        let v = field(name).with_context(|| format!("the Makefile has no {name} line"))?;
        v.parse().with_context(|| format!("the Makefile's {name} is not a number: '{v}'"))
    };
    let extraversion = field("EXTRAVERSION").unwrap_or_default();
    if !extraversion.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)) {
        bail!("the Makefile's EXTRAVERSION '{extraversion}' has characters a kernel version cannot use");
    }
    Ok(SourceVersion {
        version: number("VERSION")?,
        patchlevel: number("PATCHLEVEL")?,
        sublevel: number("SUBLEVEL")?,
        extraversion,
    })
}

/// A kernel source tarball that has been looked into.
#[derive(Debug, Clone, PartialEq)]
pub struct Tarball {
    /// The file that is checked and built. After `private_copy` this is
    /// Kernel Pop's own copy, which nobody else can change.
    pub path: PathBuf,
    /// Where the user's tarball is (shown, and where a sibling .tar.sign is
    /// looked for).
    pub origin: PathBuf,
    pub compression: Compression,
    pub version: SourceVersion,
    /// The tree's top-level directory inside the tarball ("linux-7.2.12").
    pub top_dir: String,
}

impl Tarball {
    /// The `.tar.sign` next to the tarball, if there is one:
    /// linux-7.2.12.tar.xz -> linux-7.2.12.tar.sign. (Only used to find a
    /// sibling file; the version always comes from the Makefile.)
    pub fn sibling_signature(&self) -> Option<PathBuf> {
        let name = self.origin.file_name()?.to_str()?;
        let stem = name.strip_suffix(".xz").or_else(|| name.strip_suffix(".gz"))?;
        if !stem.ends_with(".tar") {
            return None;
        }
        let sig = self.origin.with_file_name(format!("{stem}.sign"));
        sig.is_file().then_some(sig)
    }
}

/// Look into a tarball: its compression, top-level directory and the kernel
/// version in its top-level Makefile. Streams through the archive only as
/// far as that Makefile.
pub fn inspect(path: &Path) -> Result<Tarball> {
    let mut head = [0u8; 6];
    let mut f = std::fs::File::open(path).with_context(|| format!("Could not open {}", path.display()))?;
    let n = f.read(&mut head).with_context(|| format!("Could not read {}", path.display()))?;
    let compression = Compression::from_magic(&head[..n])
        .with_context(|| format!("{} is not an xz or gzip compressed tarball", path.display()))?;
    drop(f);

    let mut child = compression
        .decompress_command(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not run the decompressor (xz-utils / gzip)")?;
    let stdout = child.stdout.take().context("no decompressor output")?;
    let found = find_makefile(stdout);
    // Stop reading: the rest of the archive is not needed.
    let _ = child.kill();
    let _ = child.wait();
    let (top_dir, makefile) = found.with_context(|| format!("{} does not look like a kernel source tarball", path.display()))?;
    let version = parse_makefile(&makefile).context("Could not read the kernel version from the tarball's Makefile")?;
    Ok(Tarball { path: path.to_path_buf(), origin: path.to_path_buf(), compression, version, top_dir })
}

/// ~/.cache/kernel-pop/source: the private (0700) copy of the selected
/// tarball. The signature is checked on this copy and the build extracts
/// this copy, so a tarball in a place someone else can write (a shared
/// folder, removable media, /tmp) cannot be swapped after the check.
pub fn private_dir() -> PathBuf {
    crate::build::build_root().parent().map(|p| p.join("source")).unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

/// Copy `t` into `dir` (emptied first; created 0700) and look into the copy.
/// The copy must hold the same kernel version as the original, which may
/// not be inside `dir` itself (emptying it would delete the original).
pub fn private_copy_in(t: &Tarball, dir: &Path) -> Result<Tarball> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let (Ok(src), Ok(d)) = (t.path.canonicalize(), dir.canonicalize()) {
        if src.starts_with(&d) {
            bail!(
                "{} is already Kernel Pop's private copy; choose the original tarball instead",
                t.path.display()
            );
        }
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("Could not clear {}", dir.display()))?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("Could not create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let name = t.origin.file_name().context("the tarball has no file name")?;
    let copy = dir.join(name);
    std::fs::copy(&t.path, &copy).with_context(|| format!("Could not copy {} to {}", t.path.display(), copy.display()))?;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600))?;
    let mut c = inspect(&copy)?;
    if c.version != t.version || c.top_dir != t.top_dir {
        bail!("{} changed while it was being copied", t.origin.display());
    }
    c.origin = t.origin.clone();
    Ok(c)
}

pub fn private_copy(t: &Tarball) -> Result<Tarball> {
    private_copy_in(t, &private_dir())
}

/// Check that every member of the tarball is under its one top-level
/// directory (the extraction strips that directory), by listing it in full.
pub fn check_single_top_dir(t: &Tarball) -> Result<()> {
    let out = Command::new("tar")
        .arg("-tf")
        .arg(&t.path)
        .stderr(Stdio::null())
        .output()
        .context("Could not run tar to list the tarball")?;
    if !out.status.success() {
        bail!("tar could not list {}", t.path.display());
    }
    let prefix = format!("{}/", t.top_dir);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = line.strip_prefix("./").unwrap_or(line);
        if name != t.top_dir && name != prefix && !name.starts_with(&prefix) {
            bail!("The tarball holds {name}, outside its top-level directory {} — refusing to build it", t.top_dir);
        }
        if name.split('/').any(|c| c == "..") {
            bail!("The tarball holds an unsafe member name: {name}");
        }
    }
    Ok(())
}

/// The top-level directory and the contents of `<top>/Makefile` from a tar
/// stream. Every member before it must be under that same directory.
fn find_makefile(stream: impl Read) -> Result<(String, String)> {
    use std::path::Component;
    let mut archive = tar::Archive::new(stream);
    let mut top: Option<String> = None;
    for entry in archive.entries().context("not a tar archive")? {
        let entry = entry.context("the tar archive is damaged")?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let path = entry.path().context("a tar member has an unreadable name")?.into_owned();
        let parts: Vec<String> = path
            .components()
            .map(|c| match c {
                Component::Normal(s) => Ok(s.to_string_lossy().into_owned()),
                Component::CurDir => Ok(String::new()),
                _ => bail!("the tar archive holds an unsafe member name: {}", path.display()),
            })
            .filter(|p| !matches!(p, Ok(s) if s.is_empty()))
            .collect::<Result<_>>()?;
        let Some(first) = parts.first() else { continue };
        match &top {
            None => top = Some(first.clone()),
            Some(t) if t != first => bail!("the tarball has more than one top-level directory ({t}, {first})"),
            _ => {}
        }
        if parts.len() == 2 && parts[1] == "Makefile" && kind.is_file() {
            let mut text = String::new();
            entry.take(4 << 20).read_to_string(&mut text).context("Could not read the Makefile")?;
            return Ok((first.clone(), text));
        }
    }
    bail!("there is no top-level Makefile")
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const MAKEFILE: &str = "# SPDX-License-Identifier: GPL-2.0\nVERSION = 7\nPATCHLEVEL = 2\nSUBLEVEL = 12\nEXTRAVERSION =\nNAME = Baby Opossum Posse\n\n# *DOCUMENTATION*\nKERNELVERSION = $(VERSION)$(if $(PATCHLEVEL),.$(PATCHLEVEL)$(if $(SUBLEVEL),.$(SUBLEVEL)))$(EXTRAVERSION)\n";

    #[test]
    fn makefile_versions() {
        let v = parse_makefile(MAKEFILE).unwrap();
        assert_eq!(v.kernelversion(), "7.2.12");
        assert_eq!(v.release_name(), "7.2.12");
        assert_eq!(v.deb_version(), "7.2.12-1");
        let rc = parse_makefile("VERSION = 7\nPATCHLEVEL = 4\nSUBLEVEL = 0\nEXTRAVERSION = -rc1\n").unwrap();
        assert_eq!(rc.kernelversion(), "7.4.0-rc1");
        assert_eq!(rc.release_name(), "7.4-rc1");
        assert_eq!(rc.deb_version(), "7.4.0~rc1-1");
        let dot0 = parse_makefile("VERSION = 7\nPATCHLEVEL = 3\nSUBLEVEL = 0\nEXTRAVERSION =\n").unwrap();
        assert_eq!(dot0.release_name(), "7.3");
        assert_eq!(dot0.kernelversion(), "7.3.0");
    }

    #[test]
    fn bad_makefiles_are_refused() {
        assert!(parse_makefile("all:\n\techo hi\n").is_err());
        assert!(parse_makefile("VERSION = x\nPATCHLEVEL = 1\nSUBLEVEL = 0\n").is_err());
        assert!(parse_makefile("VERSION = 7\nPATCHLEVEL = 1\nSUBLEVEL = 0\nEXTRAVERSION = -a/../b\n").is_err());
        assert!(parse_makefile("VERSION = 7\nPATCHLEVEL = 1\nSUBLEVEL = 0\nEXTRAVERSION = $(shell id)\n").is_err());
        // "VERSION := x" style assignments elsewhere are not the version line
        let v = parse_makefile("VERSION = 6\nPATCHLEVEL = 1\nSUBLEVEL = 2\nFOO_VERSION := 9\n").unwrap();
        assert_eq!(v.kernelversion(), "6.1.2");
    }

    #[test]
    fn magic_bytes() {
        assert_eq!(Compression::from_magic(&[0xFD, b'7', b'z', b'X', b'Z', 0]), Some(Compression::Xz));
        assert_eq!(Compression::from_magic(&[0x1F, 0x8B, 8]), Some(Compression::Gzip));
        assert_eq!(Compression::from_magic(b"ustar"), None);
        assert_eq!(Compression::from_magic(&[]), None);
    }

    /// A small tar: <top>/README, <top>/Documentation/Makefile, <top>/Makefile.
    pub fn tar_bytes(top: &str, makefile: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |name: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        };
        add(&format!("{top}/README"), b"Linux kernel\n");
        add(&format!("{top}/Documentation/Makefile"), b"VERSION = 1\nPATCHLEVEL = 0\nSUBLEVEL = 0\n");
        add(&format!("{top}/Makefile"), makefile.as_bytes());
        b.into_inner().unwrap()
    }

    pub fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kernel-pop-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write `tar` compressed with `prog` (xz or gzip) to `path`.
    pub fn compress_to(prog: &str, tar: &[u8], path: &Path) {
        use std::io::Write;
        let mut c = Command::new(prog)
            .arg("-c")
            .stdin(Stdio::piped())
            .stdout(std::fs::File::create(path).unwrap())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(tar).unwrap();
        assert!(c.wait().unwrap().success());
    }

    #[test]
    fn makefile_found_only_at_top_level() {
        let (top, mk) = find_makefile(&tar_bytes("linux-7.2.12", MAKEFILE)[..]).unwrap();
        assert_eq!(top, "linux-7.2.12");
        assert_eq!(parse_makefile(&mk).unwrap().kernelversion(), "7.2.12");
        assert!(find_makefile(&b"garbage that is no tar archive at all"[..]).is_err());
    }

    #[test]
    fn inspect_reads_compressed_tarballs_by_content() {
        let d = scratch("inspect");
        // Misleading names on purpose: the version and compression come from the content.
        let xz = d.join("linux-9.9.9.tar.gz");
        compress_to("xz", &tar_bytes("linux-7.2.12", MAKEFILE), &xz);
        let t = inspect(&xz).unwrap();
        assert_eq!(t.compression, Compression::Xz);
        assert_eq!(t.top_dir, "linux-7.2.12");
        assert_eq!(t.version.kernelversion(), "7.2.12");
        let gz = d.join("my-tree.tar.xz");
        compress_to("gzip", &tar_bytes("src", MAKEFILE), &gz);
        let t = inspect(&gz).unwrap();
        assert_eq!(t.compression, Compression::Gzip);
        assert_eq!(t.top_dir, "src");
        let plain = d.join("plain.tar");
        std::fs::write(&plain, tar_bytes("x", MAKEFILE)).unwrap();
        assert!(inspect(&plain).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn private_copy_is_private_and_used() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("private");
        let xz = d.join("linux-7.2.12.tar.xz");
        compress_to("xz", &tar_bytes("linux-7.2.12", MAKEFILE), &xz);
        std::fs::write(d.join("linux-7.2.12.tar.sign"), b"sig").unwrap();
        let t = inspect(&xz).unwrap();
        let dir = d.join("private");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale.tar.xz"), b"old").unwrap();
        let c = private_copy_in(&t, &dir).unwrap();
        assert_eq!(c.path, dir.join("linux-7.2.12.tar.xz"));
        assert_eq!(c.origin, xz);
        assert!(!dir.join("stale.tar.xz").exists());
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&c.path).unwrap().permissions().mode() & 0o777, 0o600);
        // The signature is still looked for next to the user's file.
        assert_eq!(c.sibling_signature(), Some(d.join("linux-7.2.12.tar.sign")));
        // Changing the original afterwards does not change what is built.
        std::fs::write(&xz, b"swapped").unwrap();
        assert_eq!(inspect(&c.path).unwrap().version.kernelversion(), "7.2.12");
        // Copying the private copy into its own directory would delete it
        // first: refused, and the copy stays.
        let err = private_copy_in(&c, &dir).unwrap_err().to_string();
        assert!(err.contains("already Kernel Pop's private copy"), "{err}");
        assert!(c.path.exists());
        // Through another spelling of the same directory too.
        let err = private_copy_in(&c, &d.join("private/../private")).unwrap_err().to_string();
        assert!(err.contains("private copy"), "{err}");
        assert!(c.path.exists());
        // Check Again starts over from the user's file (the origin).
        std::fs::write(&xz, std::fs::read(&c.path).unwrap()).unwrap();
        let again = private_copy_in(&inspect(&c.origin).unwrap(), &dir).unwrap();
        assert_eq!(again.path, c.path);
        assert!(again.path.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn members_outside_the_top_directory_are_refused() {
        let d = scratch("topdir");
        let good = d.join("good.tar.xz");
        compress_to("xz", &tar_bytes("linux-7.2.12", MAKEFILE), &good);
        check_single_top_dir(&inspect(&good).unwrap()).unwrap();
        // The Makefile comes first, then a member under another directory.
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in [("linux-7.2.12/Makefile", MAKEFILE.as_bytes()), ("other/evil", b"x".as_slice())] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        }
        let bad = d.join("bad.tar.xz");
        compress_to("xz", &b.into_inner().unwrap(), &bad);
        let t = inspect(&bad).unwrap();
        assert!(check_single_top_dir(&t).unwrap_err().to_string().contains("outside"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sibling_signature_is_found_next_to_the_tarball() {
        let d = scratch("sibling");
        let xz = d.join("linux-7.2.12.tar.xz");
        compress_to("xz", &tar_bytes("linux-7.2.12", MAKEFILE), &xz);
        let t = inspect(&xz).unwrap();
        assert_eq!(t.sibling_signature(), None);
        std::fs::write(d.join("linux-7.2.12.tar.sign"), b"sig").unwrap();
        assert_eq!(t.sibling_signature(), Some(d.join("linux-7.2.12.tar.sign")));
        let odd = Tarball { origin: d.join("kernel.xz"), ..t };
        assert_eq!(odd.sibling_signature(), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
