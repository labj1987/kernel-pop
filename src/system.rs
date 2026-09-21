//! system.rs — Inventory of installed kernels and their boot health.
//!
//! The core safety feature of this app lives here: every kernel found in
//! /boot is checked for a matching initrd.img and /lib/modules directory,
//! so a kernel that would VFS-panic on boot is visible BEFORE the reboot.

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    pub running_kernel: String,
    pub kernels: Vec<InstalledKernel>,
    pub free_boot_bytes: Option<u64>,
    pub free_root_bytes: Option<u64>,
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
    SystemInfo {
        kernels: get_installed_kernels(&running_kernel, &bootloader),
        running_kernel,
        free_boot_bytes: get_free_disk("/boot"),
        free_root_bytes: get_free_disk("/"),
    }
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
fn get_installed_kernels(running: &str, bootloader: &Bootloader) -> Vec<InstalledKernel> {
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
        });
    }

    kernels.sort_by(|a, b| {
        let key = |k: &InstalledKernel| -> Vec<u32> {
            k.version
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|s| s.parse().ok())
                .collect()
        };
        b.running
            .cmp(&a.running)
            .then_with(|| key(b).cmp(&key(a)))
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
