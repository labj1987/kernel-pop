//! system.rs — Inventory of installed kernels and their boot health.
//!
//! The core safety feature of this app lives here: every kernel found in
//! /boot is checked for a matching initrd.img and /lib/modules directory,
//! so a kernel that would VFS-panic on boot is visible BEFORE the reboot.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    pub running_kernel: String,
    pub kernels: Vec<InstalledKernel>,
    pub free_boot_bytes: Option<u64>,
    pub free_root_bytes: Option<u64>,
    pub secure_boot: SecureBoot,
    pub kernel_signing: KernelSigning,
    /// Roughly what one more kernel needs in /boot: the largest existing
    /// kernel image plus twice the largest existing initramfs (the margin
    /// covers update-initramfs briefly needing more than the final size).
    pub boot_needed_bytes: u64,
}

impl SystemInfo {
    /// True when /boot has less free space than one more kernel needs.
    pub fn boot_low(&self) -> bool {
        self.free_boot_bytes.is_some_and(|f| f < self.boot_needed_bytes)
    }
}

/// Secure Boot state. Mainline kernels are unsigned, so `Enabled` means a
/// freshly installed one will be refused by shim at boot.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SecureBoot {
    Enabled,
    Disabled,
    #[default]
    Unknown,
}

/// State of Kernel Pop's own kernel-signing key (set up once with the
/// privileged script's `--setup-signing`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum KernelSigning {
    /// No key: kernels are installed unsigned, exactly as before.
    NoKey,
    /// A key exists but is neither in the MOK list nor queued for it.
    KeyNotEnrolled,
    /// Queued with mokutil; confirmed at MOK Manager on the next reboot.
    EnrollmentPending,
    /// In the MOK list: kernels signed with it boot under Secure Boot.
    Enrolled,
    /// A key exists but its enrollment state could not be read.
    #[default]
    Unknown,
}

impl KernelSigning {
    pub fn key_present(&self) -> bool {
        !matches!(self, KernelSigning::NoKey)
    }
}

#[derive(Debug, Clone)]
pub struct InstalledKernel {
    /// Full version string, e.g. "7.1.3-070103-generic"
    pub version: String,
    pub has_initrd: bool,
    pub has_modules: bool,
    /// Whether the active boot loader actually has a menu entry for this
    /// kernel. On GRUB this greps grub.cfg for the vmlinuz; on systemd-boot this checks for a real
    /// entry under the ESP, since nothing else guarantees one exists.
    pub has_boot_entry: bool,
    pub running: bool,
    /// Signed with Kernel Pop's key, from the marker the privileged script
    /// writes after signing. None when signing is not set up. Not part of
    /// `healthy()`: an unsigned kernel boots fine with Secure Boot off.
    pub signed: Option<bool>,
}

impl InstalledKernel {
    /// A kernel is bootable when its initrd, modules, and boot menu entry
    /// are all in place.
    pub fn healthy(&self) -> bool {
        self.has_initrd && self.has_modules && self.has_boot_entry
    }
}

/// Which boot loader actually controls this machine's boot menu, detected
/// the same way privileged-install.sh does. GRUB and systemd-boot binaries
/// can both be present on a system (leftover packages, dual setups) — the
/// signal that matters is which one an ESP loader.conf says is active,
/// not merely which binaries exist.
#[derive(Debug, Clone, PartialEq)]
pub enum Bootloader {
    Grub,
    SystemdBoot { esp: String },
    /// Pop!_OS: kernelstub manages systemd-boot with Pop_OS-current.conf
    /// rather than one entry per kernel version.
    Kernelstub,
    Unknown,
}

pub fn detect_bootloader() -> Bootloader {
    if Path::new("/etc/kernelstub/configuration").exists() || command_exists("kernelstub") {
        return Bootloader::Kernelstub;
    }
    if Path::new("/sys/firmware/efi").exists() {
        if let Ok(out) = Command::new("bootctl").arg("--print-esp-path").output() {
            if out.status.success() {
                let esp = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !esp.is_empty() && Path::new(&format!("{esp}/loader/loader.conf")).exists() {
                    return Bootloader::SystemdBoot { esp };
                }
            }
        }
    }
    if command_exists("update-grub") {
        return Bootloader::Grub;
    }
    Bootloader::Unknown
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// kernelstub keeps only Pop_OS-current / Pop_OS-oldkern rather than a
/// per-version entry, so the best available check is that its kernel image
/// exists on the ESP (EFI/Pop_OS-*/vmlinuz.efi).
fn kernelstub_has_image() -> bool {
    ["/boot/efi/EFI", "/boot/EFI"].iter().any(|dir| {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name().to_str().is_some_and(|n| n.starts_with("Pop_OS-"))
                        && e.path().join("vmlinuz.efi").exists()
                })
            })
            .unwrap_or(false)
    })
}

/// Whether grub.cfg has an entry for vmlinuz-<version>. An unreadable
/// grub.cfg gets the benefit of the doubt rather than flagging everything.
fn grub_cfg_mentions(version: &str) -> bool {
    match std::fs::read_to_string("/boot/grub/grub.cfg") {
        Ok(cfg) => cfg.contains(&format!("vmlinuz-{version}")),
        Err(_) => true,
    }
}

/// Whether the boot loader has a menu entry for this exact kernel version.
/// Unknown boot loaders are given the benefit of the doubt (true) rather
/// than marking every kernel unhealthy for a check we can't perform.
fn has_boot_entry(version: &str, bootloader: &Bootloader) -> bool {
    match bootloader {
        Bootloader::Unknown => true,
        Bootloader::Kernelstub => kernelstub_has_image(),
        Bootloader::Grub => grub_cfg_mentions(version),
        Bootloader::SystemdBoot { esp } => {
            let entries_dir = format!("{esp}/loader/entries");
            std::fs::read_dir(&entries_dir)
                .map(|entries| {
                    entries.flatten().any(|e| {
                        e.file_name()
                            .to_str()
                            .map(|n| n.ends_with(&format!("-{version}.conf")))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        }
    }
}

pub fn query_system() -> SystemInfo {
    let running_kernel = get_running_kernel();
    let bootloader = detect_bootloader();
    let kernel_signing = detect_kernel_signing();
    SystemInfo {
        kernels: get_installed_kernels(&running_kernel, &bootloader, kernel_signing.key_present()),
        running_kernel,
        free_boot_bytes: get_free_disk("/boot"),
        free_root_bytes: get_free_disk("/"),
        secure_boot: detect_secure_boot(),
        kernel_signing,
        boot_needed_bytes: boot_needed_bytes(),
    }
}

// ── Kernel signing ───────────────────────────────────────────────────────

fn mok_dir() -> PathBuf {
    std::env::var_os("KERNEL_POP_MOK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/kernel-pop/mok"))
}

fn signed_dir() -> PathBuf {
    std::env::var_os("KERNEL_POP_SIGNED_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/kernel-pop/signed"))
}

/// Parse `mokutil --test-key <der>`: "<file> is already enrolled" (also
/// printed for a key already in the enrollment request) or "<file> is not
/// enrolled". None when the output says neither.
pub fn parse_mokutil_test_key(text: &str) -> Option<bool> {
    if text.contains("is not enrolled") {
        Some(false)
    } else if text.contains("is already") {
        Some(true)
    } else {
        None
    }
}

/// Lowercase hex digits only: "AB:cd:01" -> "abcd01".
fn normalize_fingerprint(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Parse `openssl x509 -noout -fingerprint -sha1` ("SHA1 Fingerprint=AB:CD:…"
/// or "sha1 Fingerprint=…") into a normalized fingerprint.
pub fn parse_openssl_fingerprint(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.to_ascii_lowercase().contains("fingerprint="))?;
    let fp = normalize_fingerprint(line.split_once('=')?.1);
    (fp.len() == 40).then_some(fp)
}

/// Whether `mokutil --list-new` output (one "SHA1 Fingerprint: ab:cd:…"
/// line per pending key; empty when nothing is pending) includes the key
/// with this normalized SHA1 fingerprint.
pub fn list_new_contains(text: &str, fingerprint: &str) -> bool {
    !fingerprint.is_empty()
        && text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("SHA1 Fingerprint:"))
            .any(|fp| normalize_fingerprint(fp) == fingerprint)
}

pub fn combine_kernel_signing(
    key_present: bool,
    test_key: Option<bool>,
    pending: bool,
) -> KernelSigning {
    match (key_present, pending, test_key) {
        (false, _, _) => KernelSigning::NoKey,
        (true, true, _) => KernelSigning::EnrollmentPending,
        (true, false, Some(true)) => KernelSigning::Enrolled,
        (true, false, Some(false)) => KernelSigning::KeyNotEnrolled,
        (true, false, None) => KernelSigning::Unknown,
    }
}

fn stdout_of(cmd: &mut Command) -> Option<String> {
    cmd.output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

fn detect_kernel_signing() -> KernelSigning {
    let dir = mok_dir();
    let der = dir.join("signing.der");
    let key_present = ["signing.key", "signing.pem", "signing.der"]
        .iter()
        .all(|f| dir.join(f).exists());
    if !key_present {
        return KernelSigning::NoKey;
    }
    let test_key = stdout_of(Command::new("mokutil").arg("--test-key").arg(&der))
        .and_then(|t| parse_mokutil_test_key(&t));
    let fingerprint = stdout_of(
        Command::new("openssl")
            .args(["x509", "-inform", "DER", "-noout", "-fingerprint", "-sha1", "-in"])
            .arg(&der),
    )
    .and_then(|t| parse_openssl_fingerprint(&t));
    let pending = match (fingerprint, stdout_of(Command::new("mokutil").arg("--list-new"))) {
        (Some(fp), Some(list)) => list_new_contains(&list, &fp),
        _ => false,
    };
    combine_kernel_signing(true, test_key, pending)
}

/// Whether a signed marker ("<size> <mtime>", written by the privileged
/// script right after signing) still describes the kernel image on disk.
/// Any change to the image since (a reinstall, say) makes it stale.
pub fn marker_matches(marker: &str, size: u64, mtime: i64) -> bool {
    let mut it = marker.split_whitespace();
    matches!(
        (it.next().and_then(|s| s.parse::<u64>().ok()),
         it.next().and_then(|s| s.parse::<i64>().ok()),
         it.next()),
        (Some(s), Some(m), None) if s == size && m == mtime
    )
}

/// The image itself is root-only (0600), but it can be stat'ed.
fn kernel_signed(version: &str) -> bool {
    let Ok(marker) = std::fs::read_to_string(signed_dir().join(version)) else { return false };
    let Ok(meta) = std::fs::metadata(format!("/boot/vmlinuz-{version}")) else { return false };
    marker_matches(&marker, meta.len(), meta.mtime())
}

// ── Secure Boot ──────────────────────────────────────────────────────────

/// Parse the raw SecureBoot EFI variable: 4 attribute bytes, then one data
/// byte that is 1 when Secure Boot is on.
pub fn parse_secureboot_efivar(raw: &[u8]) -> SecureBoot {
    match raw.get(4) {
        Some(1) => SecureBoot::Enabled,
        Some(0) => SecureBoot::Disabled,
        _ => SecureBoot::Unknown,
    }
}

/// Parse `mokutil --sb-state` output. "SecureBoot validation is disabled in
/// shim" means shim does not enforce signatures even though the firmware
/// has Secure Boot on, so unsigned kernels do boot.
pub fn parse_mokutil_sb_state(text: &str) -> SecureBoot {
    if text.contains("validation is disabled") || text.contains("SecureBoot disabled") {
        SecureBoot::Disabled
    } else if text.contains("SecureBoot enabled") {
        SecureBoot::Enabled
    } else {
        SecureBoot::Unknown
    }
}

/// Merge the EFI variable reading with mokutil's (when installed). mokutil
/// wins when it says disabled (it also sees shim's validation switch); the
/// variable wins otherwise, with mokutil filling in when it is unreadable.
pub fn combine_secure_boot(efi: SecureBoot, mokutil: Option<SecureBoot>) -> SecureBoot {
    match (efi, mokutil) {
        (_, Some(SecureBoot::Disabled)) => SecureBoot::Disabled,
        (SecureBoot::Unknown, Some(m)) => m,
        (e, _) => e,
    }
}

fn detect_secure_boot() -> SecureBoot {
    // Legacy BIOS boot: Secure Boot does not exist.
    if !Path::new("/sys/firmware/efi").exists() {
        return SecureBoot::Disabled;
    }
    let efi = std::fs::read_dir("/sys/firmware/efi/efivars")
        .ok()
        .and_then(|entries| {
            entries.flatten().find(|e| {
                e.file_name().to_str().is_some_and(|n| n.starts_with("SecureBoot-"))
            })
        })
        .and_then(|e| std::fs::read(e.path()).ok())
        .map(|raw| parse_secureboot_efivar(&raw))
        .unwrap_or(SecureBoot::Unknown);
    // mokutil may be missing; that is fine.
    let mokutil = Command::new("mokutil")
        .arg("--sb-state")
        .output()
        .ok()
        .map(|o| parse_mokutil_sb_state(&String::from_utf8_lossy(&o.stdout)));
    combine_secure_boot(efi, mokutil)
}

// ── /boot space ──────────────────────────────────────────────────────────

/// One more kernel needs about the largest existing kernel image plus twice
/// the largest existing initramfs: update-initramfs can briefly need more
/// than the final initramfs size, so the second copy is a safety margin.
/// None when either kind is absent.
pub fn estimate_boot_needed(vmlinuz_sizes: &[u64], initrd_sizes: &[u64]) -> Option<u64> {
    Some(*vmlinuz_sizes.iter().max()? + 2 * *initrd_sizes.iter().max()?)
}

fn boot_needed_bytes() -> u64 {
    let (mut kernels, mut initrds) = (vec![], vec![]);
    if let Ok(entries) = std::fs::read_dir("/boot") {
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(meta) = e.metadata() else { continue };
            if name.starts_with("vmlinuz-") {
                kernels.push(meta.len());
            } else if name.starts_with("initrd.img-") {
                initrds.push(meta.len());
            }
        }
    }
    estimate_boot_needed(&kernels, &initrds).unwrap_or(MIN_BOOT_BYTES)
}

/// Everything worth a confirmation before an install starts. Empty means
/// nothing to warn about.
pub fn install_warnings(info: &SystemInfo) -> Vec<String> {
    let mut out = vec![];
    if info.secure_boot == SecureBoot::Enabled {
        match info.kernel_signing {
            // Signed with an enrolled key: it boots, nothing to say.
            KernelSigning::Enrolled => {}
            KernelSigning::EnrollmentPending => out.push(
                "Secure Boot is enabled. The new kernel will be signed, but the signing key \
                 is still waiting to be enrolled: finish the enrollment at the blue MOK \
                 Manager screen on the next reboot first, or the new kernel will not boot. \
                 The kernel you are running now will keep working."
                    .to_string(),
            ),
            KernelSigning::KeyNotEnrolled => out.push(
                "Secure Boot is enabled. The new kernel will be signed, but the signing key \
                 is not enrolled yet: run Set Up Signing on the System tab and finish the \
                 enrollment at the next reboot first, or the new kernel will not boot. The \
                 kernel you are running now will keep working."
                    .to_string(),
            ),
            KernelSigning::Unknown => out.push(
                "Secure Boot is enabled. The new kernel will be signed, but Kernel Pop could \
                 not confirm that the signing key is enrolled; if it is not, the new kernel \
                 will not boot. The kernel you are running now will keep working."
                    .to_string(),
            ),
            KernelSigning::NoKey => out.push(
                "Secure Boot is enabled. Mainline kernels are unsigned, so this machine will \
                 refuse to boot the new kernel unless Secure Boot is turned off or you sign \
                 the kernel yourself (Set Up Signing on the System tab). The kernel you are \
                 running now will keep working."
                    .to_string(),
            ),
        }
    }
    if let Some(f) = info.free_boot_bytes {
        if info.boot_low() {
            out.push(format!(
                "/boot has {} free but a new kernel needs about {}. The install may fail \
                 part way; remove old kernels first.",
                format_bytes(f),
                format_bytes(info.boot_needed_bytes)
            ));
        }
    }
    out
}

// ── Pruning old kernels ──────────────────────────────────────────────────

/// Leading numeric components of a version string, for newest-first sorting.
fn version_key(version: &str) -> Vec<u32> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Kernels that can be removed to keep only the newest `keep` (at least 1).
/// The running kernel and the newest installed kernel are never returned.
pub fn prune_candidates(kernels: &[InstalledKernel], keep: usize) -> Vec<String> {
    let mut sorted: Vec<&InstalledKernel> = kernels.iter().collect();
    sorted.sort_by_key(|k| std::cmp::Reverse(version_key(&k.version)));
    sorted
        .iter()
        .skip(keep.max(1))
        .filter(|k| !k.running)
        .map(|k| k.version.clone())
        .collect()
}

/// uname -r
fn get_running_kernel() -> String {
    Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Every vmlinuz-* in /boot, with initrd and modules presence checks.
/// Sorted newest-looking first, with the running kernel pinned to the top.
fn get_installed_kernels(
    running: &str,
    bootloader: &Bootloader,
    signing_key_present: bool,
) -> Vec<InstalledKernel> {
    let mut kernels = vec![];

    let Ok(entries) = std::fs::read_dir("/boot") else { return kernels };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(version) = name.strip_prefix("vmlinuz-") else { continue };

        let has_initrd = Path::new(&format!("/boot/initrd.img-{}", version)).exists();
        let has_modules = Path::new(&format!("/lib/modules/{}", version)).exists();

        kernels.push(InstalledKernel {
            version: version.to_string(),
            has_initrd,
            has_modules,
            has_boot_entry: has_boot_entry(version, bootloader),
            running: version == running,
            signed: signing_key_present.then(|| kernel_signed(version)),
        });
    }

    kernels.sort_by(|a, b| {
        b.running
            .cmp(&a.running)
            .then_with(|| version_key(&b.version).cmp(&version_key(&a.version)))
    });
    kernels
}

/// Free space at a mount point via df -B1.
fn get_free_disk(path: &str) -> Option<u64> {
    let out = Command::new("df")
        .args(["-B1", "--output=avail", path])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u64>().ok())
}

/// Minimum free space to comfortably download + unpack a kernel set (bytes).
pub const MIN_ROOT_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB

/// /boot fills up fast with kernels; warn below this.
pub const MIN_BOOT_BYTES: u64 = 200 * 1024 * 1024; // 200 MB

pub fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else {
        format!("{} KB", b / 1024)
    }
}

/// Compare the running kernel's numeric part against a mainline version
/// string like "7.1.3". Returns Newer/Same/Older from the candidate's
/// point of view, for the version badge.
#[derive(PartialEq)]
pub enum VersionRelation { Newer, Same, Older, Unknown }

pub fn compare_to_running(running: &str, candidate: &str) -> VersionRelation {
    // Running looks like "7.1.3-070103-generic" or "7.0.0-27-generic";
    // candidate may be a plain "7.1.3" or an RC like "7.2-rc1" — either way
    // the leading dotted-numeric part is what's comparable.
    let head = |s: &str| -> String {
        s.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect()
    };
    let parse = |s: &str| -> Vec<u32> {
        s.split('.').filter_map(|x| x.parse().ok()).collect()
    };
    let mut rv = parse(&head(running));
    let mut cv = parse(&head(candidate));
    if rv.is_empty() || cv.is_empty() {
        return VersionRelation::Unknown;
    }
    // Normalize lengths so "7.1" and "7.1.0" compare as equal.
    while rv.len() < 3 { rv.push(0); }
    while cv.len() < 3 { cv.push(0); }
    match cv.cmp(&rv) {
        std::cmp::Ordering::Greater => VersionRelation::Newer,
        std::cmp::Ordering::Equal   => VersionRelation::Same,
        std::cmp::Ordering::Less    => VersionRelation::Older,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_newer_same_older() {
        assert!(compare_to_running("6.8.0-45-generic", "6.10.1") == VersionRelation::Newer);
        assert!(compare_to_running("6.8.0-45-generic", "6.8") == VersionRelation::Same);
        assert!(compare_to_running("7.1.3-070103-generic", "7.1.2") == VersionRelation::Older);
    }

    #[test]
    fn compare_handles_rc_and_garbage() {
        assert!(compare_to_running("6.8.0-45-generic", "6.9-rc1") == VersionRelation::Newer);
        assert!(compare_to_running("unknown", "6.9") == VersionRelation::Unknown);
        assert!(compare_to_running("6.8.0", "") == VersionRelation::Unknown);
    }

    fn kernel(version: &str, running: bool) -> InstalledKernel {
        InstalledKernel {
            version: version.into(),
            has_initrd: true,
            has_modules: true,
            has_boot_entry: true,
            running,
            signed: None,
        }
    }

    #[test]
    fn mokutil_test_key_parsing() {
        assert_eq!(
            parse_mokutil_test_key(
                "Failed to access kernel trusted keyring: Required key not available\n\
                 /var/lib/kernel-pop/mok/signing.der is not enrolled\n"
            ),
            Some(false)
        );
        assert_eq!(
            parse_mokutil_test_key("/var/lib/kernel-pop/mok/signing.der is already enrolled\n"),
            Some(true)
        );
        assert_eq!(
            parse_mokutil_test_key("signing.der is already in the enrollment request\n"),
            Some(true)
        );
        assert_eq!(parse_mokutil_test_key("EFI variables are not supported on this system\n"), None);
        assert_eq!(parse_mokutil_test_key(""), None);
    }

    #[test]
    fn fingerprint_and_list_new_parsing() {
        let fp = parse_openssl_fingerprint(
            "SHA1 Fingerprint=76:A0:9A:00:72:12:89:2F:0F:A8:D2:C5:1E:B6:D0:C8:F4:2A:C6:4D\n",
        )
        .unwrap();
        assert_eq!(fp, "76a09a007212892f0fa8d2c51eb6d0c8f42ac64d");
        assert_eq!(
            parse_openssl_fingerprint("sha1 Fingerprint=76:a0:9a:00:72:12:89:2f:0f:a8:d2:c5:1e:b6:d0:c8:f4:2a:c6:4d"),
            Some(fp.clone())
        );
        assert_eq!(parse_openssl_fingerprint("Could not open file\n"), None);
        assert_eq!(parse_openssl_fingerprint("SHA1 Fingerprint=AB:CD\n"), None);

        let list = "[key 1]\n\
                    SHA1 Fingerprint: 11:22:33:44:55:66:77:88:99:00:aa:bb:cc:dd:ee:ff:11:22:33:44\n\
                    Certificate:\n    Data:\n\
                    [key 2]\n\
                    SHA1 Fingerprint: 76:a0:9a:00:72:12:89:2f:0f:a8:d2:c5:1e:b6:d0:c8:f4:2a:c6:4d\n\
                    Certificate:\n";
        assert!(list_new_contains(list, &fp));
        assert!(!list_new_contains(list, "ffffffffffffffffffffffffffffffffffffffff"));
        // Nothing pending: mokutil prints nothing.
        assert!(!list_new_contains("", &fp));
        assert!(!list_new_contains(list, ""));
    }

    #[test]
    fn kernel_signing_combination() {
        use KernelSigning::*;
        assert_eq!(combine_kernel_signing(false, Some(true), true), NoKey);
        assert_eq!(combine_kernel_signing(true, Some(true), true), EnrollmentPending);
        assert_eq!(combine_kernel_signing(true, Some(false), true), EnrollmentPending);
        assert_eq!(combine_kernel_signing(true, Some(true), false), Enrolled);
        assert_eq!(combine_kernel_signing(true, Some(false), false), KeyNotEnrolled);
        assert_eq!(combine_kernel_signing(true, None, false), Unknown);
        assert!(!NoKey.key_present());
        assert!(Unknown.key_present());
    }

    #[test]
    fn signed_marker_matching() {
        assert!(marker_matches("15234560 1759920000\n", 15234560, 1759920000));
        assert!(!marker_matches("15234560 1759920000\n", 15234561, 1759920000));
        assert!(!marker_matches("15234560 1759920000\n", 15234560, 1759920001));
        assert!(!marker_matches("", 0, 0));
        assert!(!marker_matches("15234560", 15234560, 0));
        assert!(!marker_matches("15234560 1759920000 extra", 15234560, 1759920000));
        assert!(!marker_matches("abc def", 0, 0));
    }

    #[test]
    fn install_warnings_follow_signing_state() {
        let mut info = SystemInfo { secure_boot: SecureBoot::Enabled, ..Default::default() };
        info.kernel_signing = KernelSigning::Enrolled;
        assert!(install_warnings(&info).is_empty());
        info.kernel_signing = KernelSigning::EnrollmentPending;
        let w = install_warnings(&info);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("next reboot"));
        info.kernel_signing = KernelSigning::KeyNotEnrolled;
        assert!(install_warnings(&info)[0].contains("next reboot"));
        info.kernel_signing = KernelSigning::NoKey;
        assert!(install_warnings(&info)[0].contains("unsigned"));
        // Secure Boot off: no signing warning whatever the state.
        info.secure_boot = SecureBoot::Disabled;
        for s in [KernelSigning::NoKey, KernelSigning::KeyNotEnrolled, KernelSigning::Unknown] {
            info.kernel_signing = s;
            assert!(install_warnings(&info).is_empty());
        }
    }

    #[test]
    fn secureboot_efivar_parsing() {
        assert_eq!(parse_secureboot_efivar(&[6, 0, 0, 0, 1]), SecureBoot::Enabled);
        assert_eq!(parse_secureboot_efivar(&[6, 0, 0, 0, 0]), SecureBoot::Disabled);
        assert_eq!(parse_secureboot_efivar(&[6, 0, 0, 0]), SecureBoot::Unknown);
        assert_eq!(parse_secureboot_efivar(&[]), SecureBoot::Unknown);
        assert_eq!(parse_secureboot_efivar(&[6, 0, 0, 0, 7]), SecureBoot::Unknown);
    }

    #[test]
    fn mokutil_parsing() {
        assert_eq!(parse_mokutil_sb_state("SecureBoot enabled\n"), SecureBoot::Enabled);
        assert_eq!(parse_mokutil_sb_state("SecureBoot disabled\n"), SecureBoot::Disabled);
        assert_eq!(
            parse_mokutil_sb_state("SecureBoot enabled\nSecureBoot validation is disabled in shim\n"),
            SecureBoot::Disabled
        );
        assert_eq!(
            parse_mokutil_sb_state("This system doesn't support Secure Boot\n"),
            SecureBoot::Unknown
        );
        assert_eq!(parse_mokutil_sb_state(""), SecureBoot::Unknown);
    }

    #[test]
    fn secure_boot_combination() {
        use SecureBoot::*;
        assert_eq!(combine_secure_boot(Enabled, None), Enabled);
        assert_eq!(combine_secure_boot(Unknown, None), Unknown);
        assert_eq!(combine_secure_boot(Unknown, Some(Enabled)), Enabled);
        assert_eq!(combine_secure_boot(Enabled, Some(Disabled)), Disabled);
        assert_eq!(combine_secure_boot(Enabled, Some(Unknown)), Enabled);
        assert_eq!(combine_secure_boot(Disabled, Some(Enabled)), Disabled);
    }

    #[test]
    fn boot_estimate_is_largest_kernel_plus_twice_largest_initrd() {
        assert_eq!(estimate_boot_needed(&[10, 30, 20], &[100, 50]), Some(230));
        assert_eq!(estimate_boot_needed(&[], &[100]), None);
        assert_eq!(estimate_boot_needed(&[10], &[]), None);
    }

    #[test]
    fn install_warnings_cover_secure_boot_and_space() {
        let mut info = SystemInfo::default();
        assert!(install_warnings(&info).is_empty());
        info.secure_boot = SecureBoot::Enabled;
        assert_eq!(install_warnings(&info).len(), 1);
        info.free_boot_bytes = Some(50);
        info.boot_needed_bytes = 100;
        assert!(info.boot_low());
        assert_eq!(install_warnings(&info).len(), 2);
        info.free_boot_bytes = Some(100);
        assert!(!info.boot_low());
        info.secure_boot = SecureBoot::Unknown;
        assert!(install_warnings(&info).is_empty());
    }

    #[test]
    fn prune_keeps_newest_and_running() {
        let ks = vec![
            kernel("6.8.0-45-generic", true),
            kernel("7.1.3-070103-generic", false),
            kernel("6.10.0-9-generic", false),
            kernel("6.5.0-1-generic", false),
        ];
        // keep 2: newest two are 7.1.3 and 6.10.0; running 6.8.0 is spared
        assert_eq!(prune_candidates(&ks, 2), vec!["6.5.0-1-generic".to_string()]);
        // keep 1: only the newest is kept besides the running kernel
        let got = prune_candidates(&ks, 1);
        assert_eq!(got, vec!["6.10.0-9-generic".to_string(), "6.5.0-1-generic".to_string()]);
        // keep 0 behaves like keep 1: the newest is never removed
        assert_eq!(prune_candidates(&ks, 0), got);
        assert!(!prune_candidates(&ks, 0).contains(&"7.1.3-070103-generic".to_string()));
        // keep more than exist: nothing to remove
        assert!(prune_candidates(&ks, 10).is_empty());
    }

    #[test]
    fn healthy_requires_all_three() {
        let mut k = InstalledKernel {
            version: "6.8.0-45-generic".into(),
            has_initrd: true,
            has_modules: true,
            has_boot_entry: true,
            running: false,
            signed: Some(false),
        };
        // An unsigned kernel is still healthy: signing is not a boot check.
        assert!(k.healthy());
        k.has_initrd = false;
        assert!(!k.healthy());
    }
}
