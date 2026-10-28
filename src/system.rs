//! system.rs — Inventory of installed kernels and their boot health.
//!
//! The core safety feature of this app lives here: every kernel found in
//! /boot is checked for a matching initrd.img and /lib/modules directory,
//! so a kernel that would VFS-panic on boot is visible BEFORE the reboot.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    pub running_kernel: String,
    pub kernels: Vec<InstalledKernel>,
    pub free_boot_bytes: Option<u64>,
    pub free_root_bytes: Option<u64>,
    pub secure_boot: SecureBoot,
    pub kernel_signing: KernelSigning,
    /// The DKMS module-signing key (/var/lib/shim-signed/mok/MOK.der), which
    /// out-of-tree modules such as the NVIDIA driver are signed with; None
    /// when there is none. Not enrolled means they do not load with Secure
    /// Boot on.
    pub dkms_key: Option<KeyEnrollment>,
    /// What the firmware started this boot with.
    pub boot_path: BootPath,
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

/// Secure Boot state. Kernels built from source are unsigned, so `Enabled`
/// means a freshly installed one is refused by shim unless it gets signed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SecureBoot {
    Enabled,
    Disabled,
    #[default]
    Unknown,
}

/// What the firmware started this boot with, read from efibootmgr. With
/// Secure Boot on, the firmware starts only what its db trusts (shim, signed
/// by Microsoft); shim then trusts the MOK list, where Kernel Pop's key is
/// enrolled, and starts MOK Manager to enroll it.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum BootPath {
    /// shim started the boot loader.
    Shim,
    /// The firmware started systemd-boot itself: refused with Secure Boot
    /// on, and the signing key can never be enrolled. Set Up Signing puts
    /// shim in front of it.
    DirectSystemdBoot,
    /// Another loader started without shim, named by its firmware entry.
    Other(String),
    /// Not an EFI boot, efibootmgr missing, or a fallback path
    /// (\EFI\BOOT\BOOTX64.EFI) that can hold either.
    #[default]
    Unknown,
}

pub fn detect_boot_path() -> BootPath {
    match Command::new("efibootmgr").output() {
        Ok(o) if o.status.success() => parse_boot_path(&String::from_utf8_lossy(&o.stdout)),
        _ => BootPath::Unknown,
    }
}

/// The firmware entry efibootmgr's BootCurrent names, classified.
pub fn parse_boot_path(out: &str) -> BootPath {
    let Some(current) = out.lines().find_map(|l| l.strip_prefix("BootCurrent:")).map(str::trim) else {
        return BootPath::Unknown;
    };
    let Some(entry) = out.lines().find(|l| l.strip_prefix("Boot").and_then(|r| r.get(..4)).is_some_and(|n| n.eq_ignore_ascii_case(current))) else {
        return BootPath::Unknown;
    };
    let lc = entry.to_lowercase();
    if lc.contains("\\shimx64.efi") {
        BootPath::Shim
    } else if lc.contains("\\efi\\boot\\bootx64.efi") {
        BootPath::Unknown
    } else if lc.contains("\\systemd-bootx64.efi") || lc.contains("linux boot manager") {
        BootPath::DirectSystemdBoot
    } else {
        let name = entry.get(8..).unwrap_or("").trim_start_matches(['*', ' ']).split('\t').next().unwrap_or("").trim();
        BootPath::Other(if name.is_empty() { "its boot loader".to_string() } else { name.to_string() })
    }
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
    /// A key exists, but this EFI machine's EFI variables are not mounted,
    /// so its enrollment state cannot be read.
    EfiVarsNotMounted,
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
    pub boot_entry: BootEntry,
    pub running: bool,
    /// Who signed the image, shown once signing is set up (None before
    /// that). Not part of `healthy()`: an unsigned kernel boots fine with
    /// Secure Boot off.
    pub signed: Option<KernelSignature>,
}

/// Whose signature a kernel image carries, as far as the unprivileged app
/// can tell (the image itself is root-only).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KernelSignature {
    /// Signed with Kernel Pop's key: the marker the privileged script writes
    /// after signing still matches the image.
    KernelPop,
    /// An Ubuntu kernel from the archive, signed by Canonical: its package
    /// is the signed linux-image-<version> built by a linux-signed source.
    /// Boots under Secure Boot through shim without Kernel Pop's key.
    Distro,
    /// Neither: a kernel not (or no longer) signed by Kernel Pop.
    Unsigned,
}

/// A kernel's place in the boot menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BootEntry {
    Present,
    /// The boot loader should have an entry for it and has none: a problem.
    Missing,
    /// kernelstub (Pop!_OS) keeps only the current and the previous kernel
    /// on the ESP, so any other installed kernel is simply not in the menu.
    /// That is how kernelstub works, not a fault.
    NotKept,
}

impl InstalledKernel {
    /// A kernel is healthy when its initrd and modules are in place and the
    /// boot loader has the entry it should have.
    pub fn healthy(&self) -> bool {
        self.has_initrd && self.has_modules && self.boot_entry != BootEntry::Missing
    }
}

/// The banner text for the kernels that are not healthy, naming what is
/// actually wrong; None when every kernel is fine.
pub fn health_summary(kernels: &[InstalledKernel]) -> Option<String> {
    let mut problems = vec![];
    if kernels.iter().any(|k| !k.has_initrd) {
        problems.push("missing its initramfs");
    }
    if kernels.iter().any(|k| !k.has_modules) {
        problems.push("missing its modules directory");
    }
    if kernels.iter().any(|k| k.boot_entry == BootEntry::Missing) {
        problems.push("missing from the boot menu");
    }
    if problems.is_empty() {
        return None;
    }
    let advice = if kernels.iter().any(|k| !k.has_initrd || !k.has_modules) { " — do not reboot into it." } else { "." };
    Some(format!("A kernel in /boot is {}{advice} See System tab.", problems.join(" or ")))
}

/// Which boot loader actually controls this machine's boot menu, detected
/// the same way privileged-install.sh does. GRUB and systemd-boot binaries
/// can both be present on a system (leftover packages, dual setups) — the
/// signal that matters is which one an ESP loader.conf says is active,
/// not merely which binaries exist.
#[derive(Debug, Clone, PartialEq)]
pub enum Bootloader {
    Grub,
    SystemdBoot {
        esp: String,
    },
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
    Command::new("sh").args(["-c", &format!("command -v {name}")]).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Which ESP file kernelstub keeps for `version`: it copies only the kernel
/// the `vmlinuz` symlink points at (as vmlinuz.efi) and the one `vmlinuz.old`
/// points at (as vmlinuz-previous.efi). The symlinks are in `/` or in
/// `/boot`, depending on /etc/kernel-img.conf; `/` wins when it has them,
/// like in the privileged script. None when kernelstub keeps no copy of it.
pub fn kernelstub_slot(version: &str, root: &Path, boot: &Path) -> Option<&'static str> {
    let base = if root.join("vmlinuz").is_symlink() { root } else { boot };
    let points_at = |link: &str| {
        std::fs::read_link(base.join(link)).ok().and_then(|t| t.file_name().map(|n| n.to_string_lossy() == format!("vmlinuz-{version}"))).unwrap_or(false)
    };
    if points_at("vmlinuz") {
        Some("vmlinuz.efi")
    } else if points_at("vmlinuz.old") {
        Some("vmlinuz-previous.efi")
    } else {
        None
    }
}

/// Where kernelstub stands with `version`: no slot is NotKept; a slot whose
/// file exists under some EFI/Pop_OS-* directory is Present, otherwise
/// Missing. An ESP the user may not read gets the benefit of the doubt, like
/// an unreadable grub.cfg.
fn kernelstub_entry(version: &str, root: &Path, boot: &Path, efi_dirs: &[&Path]) -> BootEntry {
    let Some(slot) = kernelstub_slot(version, root, boot) else { return BootEntry::NotKept };
    let present = efi_dirs.iter().any(|dir| match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("Pop_OS-")) && e.path().join(slot).exists()),
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
    });
    if present {
        BootEntry::Present
    } else {
        BootEntry::Missing
    }
}

/// Whether a systemd-boot entry file belongs to exactly `version`, the same
/// way the privileged script decides: its "version" line, or else its file
/// name ("<token>-<version>.conf", "+<tries>" boot counter allowed).
fn sdboot_entry_matches(name: &str, content: Option<&str>, version: &str) -> bool {
    if let Some(text) = content {
        if text.lines().any(|l| l.trim_end_matches('\r').strip_prefix("version").is_some_and(|rest| rest.starts_with([' ', '\t']) && rest.trim() == version)) {
            return true;
        }
    }
    let Some(stem) = name.strip_suffix(".conf") else { return false };
    let stem = match stem.rfind('+') {
        Some(i) if stem[i + 1..].chars().all(|c| c.is_ascii_digit() || c == '-') => &stem[..i],
        _ => stem,
    };
    stem.strip_suffix(version).is_some_and(|head| head.ends_with('-'))
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
/// Unknown boot loaders are given the benefit of the doubt (Present) rather
/// than marking every kernel unhealthy for a check we can't perform.
fn boot_entry(version: &str, bootloader: &Bootloader) -> BootEntry {
    let present = |yes: bool| if yes { BootEntry::Present } else { BootEntry::Missing };
    match bootloader {
        Bootloader::Unknown => BootEntry::Present,
        Bootloader::Kernelstub => kernelstub_entry(version, Path::new("/"), Path::new("/boot"), &[Path::new("/boot/efi/EFI"), Path::new("/boot/EFI")]),
        Bootloader::Grub => present(grub_cfg_mentions(version)),
        Bootloader::SystemdBoot { esp } => {
            let entries_dir = format!("{esp}/loader/entries");
            present(match std::fs::read_dir(&entries_dir) {
                Ok(entries) => entries.flatten().any(|e| {
                    let content = std::fs::read_to_string(e.path()).ok();
                    e.file_name().to_str().is_some_and(|n| sdboot_entry_matches(n, content.as_deref(), version))
                }),
                // The ESP is often mounted readable by root only.
                Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
            })
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
        dkms_key: detect_dkms_key(),
        boot_path: detect_boot_path(),
        boot_needed_bytes: boot_needed_bytes(),
    }
}

// ── Kernel signing ───────────────────────────────────────────────────────

const MOK_DIR: &str = "/var/lib/kernel-pop/mok";
const SIGNED_DIR: &str = "/var/lib/kernel-pop/signed";

/// What `mokutil --test-key <der>` says about a key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TestKey {
    /// "<file> is already enrolled"
    Enrolled,
    /// "<file> is already in the enrollment request": queued, confirmed at
    /// MOK Manager on the next boot.
    Pending,
    /// "<file> is not enrolled"
    NotEnrolled,
    /// "EFI variables are not supported on this system" on a machine that
    /// booted with EFI (/sys/firmware/efi exists): efivarfs is not mounted.
    EfiVarsNotMounted,
}

/// Parse `mokutil --test-key <der>` (stdout and stderr). `efi_firmware` is
/// whether /sys/firmware/efi exists. None when the output says none of
/// these (mokutil missing, a machine without EFI, …).
pub fn parse_mokutil_test_key(text: &str, efi_firmware: bool) -> Option<TestKey> {
    if text.contains("EFI variables are not supported") {
        efi_firmware.then_some(TestKey::EfiVarsNotMounted)
    } else if text.contains("is not enrolled") {
        Some(TestKey::NotEnrolled)
    } else if text.contains("is already in the enrollment request") {
        Some(TestKey::Pending)
    } else if text.contains("is already enrolled") {
        Some(TestKey::Enrolled)
    } else {
        None
    }
}

/// Whether a MOK key is trusted at boot yet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyEnrollment {
    Enrolled,
    /// Queued; MOK Manager enrolls it at the next boot.
    Pending,
    NotEnrolled,
    /// The EFI variables are not mounted, so the state cannot be read.
    EfiVarsNotMounted,
    Unknown,
}

/// Lowercase hex digits only: "AB:cd:01" -> "abcd01".
fn normalize_fingerprint(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_hexdigit()).map(|c| c.to_ascii_lowercase()).collect()
}

/// Parse `openssl x509 -noout -fingerprint -sha1` ("SHA1 Fingerprint=AB:CD:…"
/// or "sha1 Fingerprint=…") into a normalized fingerprint.
pub fn parse_openssl_fingerprint(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.to_ascii_lowercase().contains("fingerprint="))?;
    let fp = normalize_fingerprint(line.split_once('=')?.1);
    (fp.len() == 40).then_some(fp)
}

/// The fingerprints in the shared list of waiting keys (written by the
/// signing setup of this app or another one; `mokutil --list-new` cannot
/// tell, as the waiting request, MokNew, is readable by root only). The
/// first line is `moknew-size <bytes>`, MokNew's size right after the setup
/// queued the keys; `moknew_size` is its size now, `None` when nothing is
/// waiting. Any other size means the request changed since (enrolled at
/// boot, revoked, replaced by other means): the list is stale and empty.
pub fn parse_pending_list(text: &str, moknew_size: Option<u64>) -> Vec<String> {
    let mut lines = text.lines();
    let recorded = lines.next().and_then(|l| l.trim().strip_prefix("moknew-size ")).and_then(|n| n.trim().parse::<u64>().ok());
    if recorded.is_none() || recorded != moknew_size {
        return vec![];
    }
    lines.map(normalize_fingerprint).filter(|f| f.len() == 40).collect()
}

/// `pending` is whether the key is waiting in an enrollment request (see
/// `key_pending`); `--test-key` reports that too, but only when run as root.
/// Enrolled wins: a key enrolled at boot may still be on a list.
pub fn combine_kernel_signing(key_present: bool, test_key: Option<TestKey>, pending: bool) -> KernelSigning {
    match (key_present, test_key, pending) {
        (false, _, _) => KernelSigning::NoKey,
        (true, Some(TestKey::Enrolled), _) => KernelSigning::Enrolled,
        (true, _, true) | (true, Some(TestKey::Pending), _) => KernelSigning::EnrollmentPending,
        (true, Some(TestKey::NotEnrolled), false) => KernelSigning::KeyNotEnrolled,
        (true, Some(TestKey::EfiVarsNotMounted), false) => KernelSigning::EfiVarsNotMounted,
        (true, None, false) => KernelSigning::Unknown,
    }
}

/// All three key files the privileged script creates are in `dir`.
fn key_files_present(dir: &Path) -> bool {
    ["signing.key", "signing.pem", "signing.der"].iter().all(|f| dir.join(f).exists())
}

fn stdout_of(cmd: &mut Command) -> Option<String> {
    cmd.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

/// The enrollment request shim reads at boot, and the shared list of what
/// the last signing setup (this app's or another one's) put in it.
const MOK_NEW: &str = "/sys/firmware/efi/efivars/MokNew-605dab50-e046-4300-abb6-3dd810dd8b23";
const MOK_PENDING_LIST: &str = "/var/lib/mok-pending/list";
const DKMS_MOK_DER: &str = "/var/lib/shim-signed/mok/MOK.der";

fn cert_fingerprint(der: &Path) -> Option<String> {
    stdout_of(Command::new("openssl").args(["x509", "-inform", "DER", "-noout", "-fingerprint", "-sha1", "-in"]).arg(der))
        .and_then(|t| parse_openssl_fingerprint(&t))
}

/// Whether the certificate is waiting in the enrollment request: a request
/// exists, unchanged since the last setup, and that setup listed it. (A
/// plain user can stat MokNew in efivarfs, not read it.)
fn key_pending(der: &Path) -> bool {
    let Ok(moknew) = std::fs::metadata(MOK_NEW) else { return false };
    match (cert_fingerprint(der), std::fs::read_to_string(MOK_PENDING_LIST)) {
        (Some(fp), Ok(list)) => parse_pending_list(&list, Some(moknew.len())).contains(&fp),
        _ => false,
    }
}

/// `mokutil --test-key`; it prints "EFI variables are not supported" on
/// stderr, so both streams are read.
fn test_key(der: &Path) -> Option<TestKey> {
    let out = Command::new("mokutil").arg("--test-key").arg(der).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    parse_mokutil_test_key(&text, Path::new("/sys/firmware/efi").exists())
}

fn detect_kernel_signing() -> KernelSigning {
    let dir = Path::new(MOK_DIR);
    let der = dir.join("signing.der");
    if !key_files_present(dir) {
        return KernelSigning::NoKey;
    }
    combine_kernel_signing(true, test_key(&der), key_pending(&der))
}

pub fn combine_key_enrollment(test_key: Option<TestKey>, pending: bool) -> KeyEnrollment {
    match (test_key, pending) {
        (Some(TestKey::Enrolled), _) => KeyEnrollment::Enrolled,
        (_, true) | (Some(TestKey::Pending), _) => KeyEnrollment::Pending,
        (Some(TestKey::NotEnrolled), false) => KeyEnrollment::NotEnrolled,
        (Some(TestKey::EfiVarsNotMounted), false) => KeyEnrollment::EfiVarsNotMounted,
        (None, false) => KeyEnrollment::Unknown,
    }
}

fn detect_dkms_key() -> Option<KeyEnrollment> {
    let der = Path::new(DKMS_MOK_DER);
    der.exists().then(|| combine_key_enrollment(test_key(der), key_pending(der)))
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

/// Whether `boot`/vmlinuz-<version> is signed with Kernel Pop's key, from
/// its marker in `signed_dir`. The image itself is root-only (0600), but it
/// can be stat'ed.
fn signed_by_kernel_pop(signed_dir: &Path, boot: &Path, version: &str) -> bool {
    let Ok(marker) = std::fs::read_to_string(signed_dir.join(version)) else { return false };
    let Ok(meta) = std::fs::metadata(boot.join(format!("vmlinuz-{version}"))) else { return false };
    marker_matches(&marker, meta.len(), meta.mtime())
}

/// Kernel versions whose image comes from Ubuntu's signed kernel package,
/// from `dpkg-query -W -f '${db:Status-Abbrev}\t${Package}\t${source:Package}\n'
/// 'linux-image-*'`: an installed linux-image-<version> (not -unsigned-)
/// whose source package is linux-signed or linux-signed-<flavour>.
pub fn parse_distro_signed(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let (status, pkg, source) = (f.next()?.trim(), f.next()?, f.next()?.trim());
            let version = pkg.strip_prefix("linux-image-")?;
            let signed_source = source == "linux-signed" || source.starts_with("linux-signed-");
            (status == "ii" && signed_source && !version.starts_with("unsigned-")).then(|| version.to_string())
        })
        .collect()
}

fn distro_signed_kernels() -> HashSet<String> {
    stdout_of(Command::new("dpkg-query").args(["-W", "-f", "${db:Status-Abbrev}\t${Package}\t${source:Package}\n", "linux-image-*"]))
        .map(|t| parse_distro_signed(&t))
        .unwrap_or_default()
}

/// Kernel Pop's own signature wins (it is what the Sign button is about);
/// otherwise an Ubuntu-signed kernel is reported as such.
pub fn classify_signature(ours: bool, distro: bool) -> KernelSignature {
    match (ours, distro) {
        (true, _) => KernelSignature::KernelPop,
        (false, true) => KernelSignature::Distro,
        (false, false) => KernelSignature::Unsigned,
    }
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
        .and_then(|entries| entries.flatten().find(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("SecureBoot-"))))
        .and_then(|e| std::fs::read(e.path()).ok())
        .map(|raw| parse_secureboot_efivar(&raw))
        .unwrap_or(SecureBoot::Unknown);
    // mokutil may be missing; that is fine.
    let mokutil = Command::new("mokutil").arg("--sb-state").output().ok().map(|o| parse_mokutil_sb_state(&String::from_utf8_lossy(&o.stdout)));
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

/// Why an install must not start at all, or None. With Secure Boot on, a
/// kernel built from source boots only when signed with a key that is enrolled or
/// queued for enrollment; anything else would leave a kernel shim refuses.
/// The privileged script makes the same check itself before installing.
pub fn install_blocker(info: &SystemInfo) -> Option<&'static str> {
    if info.secure_boot != SecureBoot::Enabled {
        return None;
    }
    match info.kernel_signing {
        KernelSigning::Enrolled | KernelSigning::EnrollmentPending => None,
        KernelSigning::NoKey => Some(
            "Secure Boot is on and kernel signing is not set up, so a kernel built from \
             source could not boot on this machine. Use Set Up Signing on the System tab first, or turn \
             Secure Boot off.",
        ),
        KernelSigning::KeyNotEnrolled => Some(
            "Secure Boot is on, but Kernel Pop's signing key is not enrolled, so a kernel \
             signed with it could not boot. Use Set Up Signing on the System tab to queue the \
             enrollment, then confirm it at the next reboot.",
        ),
        KernelSigning::EfiVarsNotMounted => Some(
            "Secure Boot is on, but the EFI variables are not mounted, so Kernel Pop cannot \
             check that its signing key is enrolled and the new kernel might not boot. Mount \
             them (sudo mount -t efivarfs efivarfs /sys/firmware/efi/efivars) or reboot.",
        ),
        KernelSigning::Unknown => Some(
            "Secure Boot is on, and Kernel Pop could not confirm that its signing key is \
             enrolled, so the new kernel might not boot. Check that mokutil is installed.",
        ),
    }
}

/// Everything worth a confirmation before an install starts. Empty means
/// nothing to warn about.
pub fn install_warnings(info: &SystemInfo) -> Vec<String> {
    let mut out = vec![];
    if info.secure_boot == SecureBoot::Enabled && info.kernel_signing == KernelSigning::EnrollmentPending {
        out.push(
            "Secure Boot is on. The new kernel will be signed, but the signing key is still \
             waiting to be enrolled: confirm the enrollment at the blue MOK Manager screen on \
             the next reboot, or the new kernel will not boot. The kernel you are running now \
             keeps working."
                .to_string(),
        );
    }
    if info.secure_boot == SecureBoot::Enabled && info.dkms_key == Some(KeyEnrollment::NotEnrolled) {
        out.push(
            "Secure Boot is on, but the key DKMS signs out-of-tree modules with (such as the \
             NVIDIA driver) is not enrolled, so those modules will not load. Set Up Signing on \
             the System tab queues it for enrollment together with Kernel Pop's key."
                .to_string(),
        );
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
    version.split(|c: char| !c.is_ascii_digit()).filter_map(|s| s.parse().ok()).collect()
}

/// Kernels that can be removed to keep only the newest `keep` (at least 1).
/// The running kernel and the newest installed kernel are never returned.
pub fn prune_candidates(kernels: &[InstalledKernel], keep: usize) -> Vec<String> {
    let mut sorted: Vec<&InstalledKernel> = kernels.iter().collect();
    sorted.sort_by_key(|k| std::cmp::Reverse(version_key(&k.version)));
    sorted.iter().skip(keep.max(1)).filter(|k| !k.running).map(|k| k.version.clone()).collect()
}

/// uname -r
fn get_running_kernel() -> String {
    Command::new("uname").arg("-r").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|_| "unknown".to_string())
}

/// Every vmlinuz-* in /boot, with initrd and modules presence checks.
/// Sorted newest-looking first, with the running kernel pinned to the top.
fn get_installed_kernels(running: &str, bootloader: &Bootloader, signing_key_present: bool) -> Vec<InstalledKernel> {
    let mut kernels = vec![];
    let distro_signed = if signing_key_present { distro_signed_kernels() } else { HashSet::new() };

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
            boot_entry: boot_entry(version, bootloader),
            running: version == running,
            signed: signing_key_present
                .then(|| classify_signature(signed_by_kernel_pop(Path::new(SIGNED_DIR), Path::new("/boot"), version), distro_signed.contains(version))),
        });
    }

    kernels.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| version_key(&b.version).cmp(&version_key(&a.version))));
    kernels
}

/// Free space at a mount point via df -B1.
fn get_free_disk(path: &str) -> Option<u64> {
    let out = Command::new("df").args(["-B1", "--output=avail", path]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1).and_then(|l| l.trim().parse::<u64>().ok())
}

/// Free space / should have for a kernel install (bytes).
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

/// Compare the running kernel's numeric part against a kernel.org version
/// string like "7.1.3". Returns Newer/Same/Older from the candidate's
/// point of view, for the version badge.
#[derive(PartialEq)]
pub enum VersionRelation {
    Newer,
    Same,
    Older,
    Unknown,
}

pub fn compare_to_running(running: &str, candidate: &str) -> VersionRelation {
    // Running looks like "7.1.3-070103-generic" or "7.0.0-27-generic";
    // candidate may be a plain "7.1.3" or an RC like "7.2-rc1" — either way
    // the leading dotted-numeric part is what's comparable.
    let head = |s: &str| -> String { s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect() };
    let parse = |s: &str| -> Vec<u32> { s.split('.').filter_map(|x| x.parse().ok()).collect() };
    let mut rv = parse(&head(running));
    let mut cv = parse(&head(candidate));
    if rv.is_empty() || cv.is_empty() {
        return VersionRelation::Unknown;
    }
    // Normalize lengths so "7.1" and "7.1.0" compare as equal.
    while rv.len() < 3 {
        rv.push(0);
    }
    while cv.len() < 3 {
        cv.push(0);
    }
    match cv.cmp(&rv) {
        std::cmp::Ordering::Greater => VersionRelation::Newer,
        std::cmp::Ordering::Equal => VersionRelation::Same,
        std::cmp::Ordering::Less => VersionRelation::Older,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn boot_path_from_efibootmgr() {
        let direct = "BootCurrent: 0006\nTimeout: 2 seconds\nBootOrder: 0006,0000\nBoot0000* UEFI PC SN530 NVMe WDC 512GB\tHD(1,GPT,1b97,0x800,0x219800)/\\EFI\\Boot\\BootX64.efi{auto_created_boot_option}\nBoot0006* Linux Boot Manager\tHD(1,GPT,1b97,0x800,0x219800)/\\EFI\\systemd\\systemd-bootx64.efi\n";
        assert_eq!(parse_boot_path(direct), BootPath::DirectSystemdBoot);
        let shim = "BootCurrent: 0003\nBootOrder: 0003,0000\nBoot0003* Ubuntu\tHD(1,GPT,1b97,0x800,0x219800)/\\EFI\\ubuntu\\shimx64.efi5c004500460049005c00\n";
        assert_eq!(parse_boot_path(shim), BootPath::Shim);
        let fallback = direct.replace("BootCurrent: 0006", "BootCurrent: 0000");
        assert_eq!(parse_boot_path(&fallback), BootPath::Unknown);
        let grub = "BootCurrent: 0001\nBoot0001* ubuntu-grub\tHD(1,GPT,x)/\\EFI\\ubuntu\\grubx64.efi\n";
        assert_eq!(parse_boot_path(grub), BootPath::Other("ubuntu-grub".to_string()));
        assert_eq!(parse_boot_path("BootOrder: 0001\n"), BootPath::Unknown);
        assert_eq!(parse_boot_path("BootCurrent: 0009\nBoot0001* x\tHD()/\\EFI\\x.efi\n"), BootPath::Unknown);
    }

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
        InstalledKernel { version: version.into(), has_initrd: true, has_modules: true, boot_entry: BootEntry::Present, running, signed: None }
    }

    #[test]
    fn mokutil_test_key_parsing() {
        assert_eq!(
            parse_mokutil_test_key(
                "Failed to access kernel trusted keyring: Required key not available\n\
                 /var/lib/kernel-pop/mok/signing.der is not enrolled\n",
                true
            ),
            Some(TestKey::NotEnrolled)
        );
        assert_eq!(parse_mokutil_test_key("/var/lib/kernel-pop/mok/signing.der is already enrolled\n", true), Some(TestKey::Enrolled));
        assert_eq!(parse_mokutil_test_key("signing.der is already in the enrollment request\n", true), Some(TestKey::Pending));
        // On an EFI machine this means efivarfs is not mounted; without EFI
        // firmware there is simply nothing to read.
        assert_eq!(parse_mokutil_test_key("EFI variables are not supported on this system\n", true), Some(TestKey::EfiVarsNotMounted));
        assert_eq!(parse_mokutil_test_key("EFI variables are not supported on this system\n", false), None);
        assert_eq!(parse_mokutil_test_key("", true), None);
    }

    #[test]
    fn fingerprint_and_pending_list_parsing() {
        let fp = parse_openssl_fingerprint("SHA1 Fingerprint=76:A0:9A:00:72:12:89:2F:0F:A8:D2:C5:1E:B6:D0:C8:F4:2A:C6:4D\n").unwrap();
        assert_eq!(fp, "76a09a007212892f0fa8d2c51eb6d0c8f42ac64d");
        assert_eq!(parse_openssl_fingerprint("sha1 Fingerprint=76:a0:9a:00:72:12:89:2f:0f:a8:d2:c5:1e:b6:d0:c8:f4:2a:c6:4d"), Some(fp.clone()));
        assert_eq!(parse_openssl_fingerprint("Could not open file\n"), None);
        assert_eq!(parse_openssl_fingerprint("SHA1 Fingerprint=AB:CD\n"), None);

        let other = "43e4f8245b73f134bf13c1dbe63001527c7b0905".to_string();
        let list = "moknew-size 2052\n76a09a007212892f0fa8d2c51eb6d0c8f42ac64d\n43E4F8245B73F134BF13C1DBE63001527C7B0905\n";
        // Valid only while MokNew exists with the size recorded at the import.
        assert_eq!(parse_pending_list(list, Some(2052)), vec![fp.clone(), other.clone()]);
        assert!(parse_pending_list(list, Some(2053)).is_empty(), "request changed since: stale");
        assert!(parse_pending_list(list, None).is_empty(), "no request waiting: stale");
        // Without the size line (or with a damaged one) a list is never trusted.
        assert!(parse_pending_list("76a09a007212892f0fa8d2c51eb6d0c8f42ac64d\n", Some(2052)).is_empty());
        assert!(parse_pending_list("moknew-size x\n76a09a007212892f0fa8d2c51eb6d0c8f42ac64d\n", Some(2052)).is_empty());
        assert!(parse_pending_list("", Some(0)).is_empty());
        // Lines that are not fingerprints are skipped.
        assert_eq!(parse_pending_list("moknew-size 10\nabc\n\n76a09a007212892f0fa8d2c51eb6d0c8f42ac64d\n", Some(10)), vec![fp]);
        assert_eq!(parse_pending_list("moknew-size 10\n", Some(10)), Vec::<String>::new());
    }

    #[test]
    fn key_enrollment_combination() {
        use KeyEnrollment::*;
        use TestKey as T;
        assert_eq!(combine_key_enrollment(Some(T::Enrolled), true), Enrolled);
        assert_eq!(combine_key_enrollment(Some(T::NotEnrolled), true), Pending);
        assert_eq!(combine_key_enrollment(Some(T::Pending), false), Pending);
        assert_eq!(combine_key_enrollment(Some(T::NotEnrolled), false), NotEnrolled);
        assert_eq!(combine_key_enrollment(None, false), Unknown);
        assert_eq!(combine_key_enrollment(None, true), Pending);
        assert_eq!(combine_key_enrollment(Some(T::EfiVarsNotMounted), false), EfiVarsNotMounted);
    }

    #[test]
    fn kernel_signing_combination() {
        use KernelSigning::*;
        use TestKey as T;
        assert_eq!(combine_kernel_signing(false, Some(T::Enrolled), true), NoKey);
        // Enrolled wins over a list entry: the key was enrolled at boot.
        assert_eq!(combine_kernel_signing(true, Some(T::Enrolled), true), Enrolled);
        assert_eq!(combine_kernel_signing(true, Some(T::NotEnrolled), true), EnrollmentPending);
        assert_eq!(combine_kernel_signing(true, Some(T::Enrolled), false), Enrolled);
        assert_eq!(combine_kernel_signing(true, Some(T::NotEnrolled), false), KeyNotEnrolled);
        assert_eq!(combine_kernel_signing(true, None, false), Unknown);
        assert_eq!(combine_kernel_signing(true, Some(T::EfiVarsNotMounted), false), EfiVarsNotMounted);
        assert_eq!(combine_kernel_signing(true, None, true), EnrollmentPending);
        // --test-key alone is enough to see a queued key: with openssl or
        // --list-new failing, a pending key must never read as Enrolled.
        assert_eq!(combine_kernel_signing(true, Some(T::Pending), false), EnrollmentPending);
        assert!(!NoKey.key_present());
        assert!(Unknown.key_present());
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kernel-pop-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn kernelstub_slots_follow_the_symlinks() {
        use std::os::unix::fs::symlink;
        let d = scratch("ks");
        let (root, boot, efi) = (d.join("root"), d.join("boot"), d.join("efi/EFI"));
        for p in [&root, &boot, &efi.join("Pop_OS-1234")] {
            std::fs::create_dir_all(p).unwrap();
        }
        symlink("vmlinuz-7.1.3-070103-generic", boot.join("vmlinuz")).unwrap();
        symlink("vmlinuz-6.8.0-45-generic", boot.join("vmlinuz.old")).unwrap();
        assert_eq!(kernelstub_slot("7.1.3-070103-generic", &root, &boot), Some("vmlinuz.efi"));
        assert_eq!(kernelstub_slot("6.8.0-45-generic", &root, &boot), Some("vmlinuz-previous.efi"));
        assert_eq!(kernelstub_slot("6.5.0-1-generic", &root, &boot), None);
        assert_eq!(kernelstub_slot("1.3-070103-generic", &root, &boot), None);

        let dirs = [efi.as_path()];
        std::fs::write(efi.join("Pop_OS-1234/vmlinuz.efi"), b"x").unwrap();
        assert_eq!(kernelstub_entry("7.1.3-070103-generic", &root, &boot, &dirs), BootEntry::Present);
        // Its slot file is missing on the ESP: a real problem
        assert_eq!(kernelstub_entry("6.8.0-45-generic", &root, &boot, &dirs), BootEntry::Missing);
        // No slot at all: kernelstub keeps no copy of it, which is normal
        assert_eq!(kernelstub_entry("6.5.0-1-generic", &root, &boot, &dirs), BootEntry::NotKept);

        // Links in / win over /boot
        symlink("boot/vmlinuz-6.5.0-1-generic", root.join("vmlinuz")).unwrap();
        assert_eq!(kernelstub_slot("6.5.0-1-generic", &root, &boot), Some("vmlinuz.efi"));
        assert_eq!(kernelstub_slot("7.1.3-070103-generic", &root, &boot), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn systemd_boot_entries_match_exactly() {
        let v = "7.1.3-070103-generic";
        let m = |name: &str| sdboot_entry_matches(name, None, v);
        assert!(m("abc-7.1.3-070103-generic.conf"));
        assert!(m("abc-7.1.3-070103-generic+3.conf"));
        assert!(m("abc-7.1.3-070103-generic+2-1.conf"));
        assert!(!m("abc-17.1.3-070103-generic.conf"));
        assert!(!m("abc-7.1.3-070103-generic-64k.conf"));
        assert!(!m("abc-7.1.3-070103-generic.conf.bak"));
        // The version line counts too, whatever the file is called (as in the script).
        assert!(sdboot_entry_matches("custom.conf", Some("title Ubuntu\nversion 7.1.3-070103-generic\n"), v));
        assert!(sdboot_entry_matches("custom.conf", Some("version\t7.1.3-070103-generic  \r\n"), v));
        assert!(!sdboot_entry_matches("custom.conf", Some("version 17.1.3-070103-generic\n"), v));
        assert!(!sdboot_entry_matches("custom.conf", Some("versionx 7.1.3-070103-generic\n"), v));
    }

    #[test]
    fn signature_sources() {
        let d = scratch("sig");
        let (signed, boot) = (d.join("signed"), d.join("boot"));
        std::fs::create_dir_all(&signed).unwrap();
        std::fs::create_dir_all(&boot).unwrap();
        let img = boot.join("vmlinuz-7.1.3-070103-generic");
        std::fs::write(&img, b"image").unwrap();
        assert!(!signed_by_kernel_pop(&signed, &boot, "7.1.3-070103-generic"));
        let meta = std::fs::metadata(&img).unwrap();
        std::fs::write(signed.join("7.1.3-070103-generic"), format!("{} {}\n", meta.len(), meta.mtime())).unwrap();
        assert!(signed_by_kernel_pop(&signed, &boot, "7.1.3-070103-generic"));
        assert!(!signed_by_kernel_pop(&signed, &boot, "6.8.0-45-generic"));
        assert!(!key_files_present(&d));
        for f in ["signing.key", "signing.pem", "signing.der"] {
            std::fs::write(d.join(f), b"").unwrap();
        }
        assert!(key_files_present(&d));
        std::fs::remove_dir_all(&d).unwrap();

        let dpkg = "ii \tlinux-image-6.8.0-45-generic\tlinux-signed\n\
                    ii \tlinux-image-6.11.0-19-generic\tlinux-signed-hwe-6.11\n\
                    ii \tlinux-image-unsigned-6.8.0-45-generic\tlinux\n\
                    ii \tlinux-image-unsigned-7.1.3-070103-generic\tlinux-upstream\n\
                    rc \tlinux-image-6.5.0-1-generic\tlinux-signed\n\
                    ii \tlinux-image-generic\tlinux-meta\n";
        let set = parse_distro_signed(dpkg);
        assert_eq!(set.len(), 2);
        assert!(set.contains("6.8.0-45-generic") && set.contains("6.11.0-19-generic"));
        assert_eq!(classify_signature(true, true), KernelSignature::KernelPop);
        assert_eq!(classify_signature(false, true), KernelSignature::Distro);
        assert_eq!(classify_signature(false, false), KernelSignature::Unsigned);
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
    fn install_is_refused_when_the_kernel_could_not_boot() {
        let mut info = SystemInfo { secure_boot: SecureBoot::Enabled, ..Default::default() };
        for (s, blocked) in [
            (KernelSigning::NoKey, true),
            (KernelSigning::KeyNotEnrolled, true),
            (KernelSigning::Unknown, true),
            (KernelSigning::EfiVarsNotMounted, true),
            (KernelSigning::EnrollmentPending, false),
            (KernelSigning::Enrolled, false),
        ] {
            info.kernel_signing = s;
            assert_eq!(install_blocker(&info).is_some(), blocked, "{s:?}");
        }
        info.kernel_signing = KernelSigning::NoKey;
        assert!(install_blocker(&info).unwrap().contains("Set Up Signing"));
        // Secure Boot off or unknown: never refused here (the script checks
        // the real state itself).
        for sb in [SecureBoot::Disabled, SecureBoot::Unknown] {
            info.secure_boot = sb;
            assert!(install_blocker(&info).is_none());
        }
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
        info.secure_boot = SecureBoot::Disabled;
        assert!(install_warnings(&info).is_empty());
        // The DKMS key matters only with Secure Boot on, and only when not enrolled or queued.
        info.dkms_key = Some(KeyEnrollment::NotEnrolled);
        assert!(install_warnings(&info).is_empty());
        info.secure_boot = SecureBoot::Enabled;
        info.kernel_signing = KernelSigning::Enrolled;
        assert!(install_warnings(&info)[0].contains("NVIDIA"));
        info.dkms_key = Some(KeyEnrollment::Pending);
        assert!(install_warnings(&info).is_empty());
    }

    #[test]
    fn health_summary_names_the_real_problem() {
        let mut ks = vec![kernel("7.1.3-070103-generic", false), kernel("6.8.0-45-generic", true)];
        assert_eq!(health_summary(&ks), None);
        // kernelstub keeping no copy is information, not a problem
        ks[0].boot_entry = BootEntry::NotKept;
        assert!(ks[0].healthy());
        assert_eq!(health_summary(&ks), None);
        ks[0].boot_entry = BootEntry::Missing;
        assert!(!ks[0].healthy());
        let t = health_summary(&ks).unwrap();
        assert!(t.contains("missing from the boot menu") && !t.contains("initramfs"), "{t}");
        ks[1].has_initrd = false;
        let t = health_summary(&ks).unwrap();
        assert!(t.contains("missing its initramfs") && t.contains("do not reboot"), "{t}");
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
        assert_eq!(parse_mokutil_sb_state("SecureBoot enabled\nSecureBoot validation is disabled in shim\n"), SecureBoot::Disabled);
        assert_eq!(parse_mokutil_sb_state("This system doesn't support Secure Boot\n"), SecureBoot::Unknown);
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
    fn install_warnings_cover_space() {
        let mut info = SystemInfo::default();
        assert!(install_warnings(&info).is_empty());
        info.free_boot_bytes = Some(50);
        info.boot_needed_bytes = 100;
        assert!(info.boot_low());
        assert_eq!(install_warnings(&info).len(), 1);
        info.free_boot_bytes = Some(100);
        assert!(!info.boot_low());
        assert!(install_warnings(&info).is_empty());
    }

    #[test]
    fn prune_keeps_newest_and_running() {
        let ks =
            vec![kernel("6.8.0-45-generic", true), kernel("7.1.3-070103-generic", false), kernel("6.10.0-9-generic", false), kernel("6.5.0-1-generic", false)];
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
            boot_entry: BootEntry::Present,
            running: false,
            signed: Some(KernelSignature::Unsigned),
        };
        // An unsigned kernel is still healthy: signing is not a boot check.
        assert!(k.healthy());
        k.has_initrd = false;
        assert!(!k.healthy());
    }
}
