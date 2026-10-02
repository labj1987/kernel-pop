//! install.rs — Invoke the privileged script via pkexec.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = "/usr/lib/kernel-pop/privileged-install.sh";

/// Lines of script output kept in an error message.
const ERROR_TAIL_LINES: usize = 15;

/// Run the privileged script and return what it printed. The same lines
/// also go to /var/log/kernel-pop.log, but returning them lets the UI show
/// per-step results (DKMS in particular) in its own Log tab.
fn run_script(args: &[String]) -> Result<String> {
    if !Path::new(SCRIPT).exists() {
        bail!("Privileged script not found at {}", SCRIPT);
    }

    let mut full = vec![SCRIPT.to_string()];
    full.extend_from_slice(args);

    let output = Command::new("pkexec")
        .args(&full)
        .output()
        .context("Failed to launch pkexec — is polkit installed?")?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        if code == 126 || code == 127 {
            bail!("Authentication was cancelled.");
        }
        bail!(
            "Script exited with code {} (see /var/log/kernel-pop.log)\n{}",
            code,
            tail_lines(&stdout, ERROR_TAIL_LINES)
        );
    }
    Ok(stdout)
}

/// The last `n` non-empty lines of `text`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Lines the install script flagged as DKMS problems. A non-empty result
/// means the kernel installed but out-of-tree modules (such as the NVIDIA
/// driver) may be missing after reboot.
pub fn dkms_warnings(output: &str) -> Vec<&str> {
    output.lines().filter(|l| l.contains("DKMS WARNING")).collect()
}

/// Install a downloaded .deb set. The script derives the kernel version
/// from the package metadata (never from filenames), builds DKMS modules,
/// generates the initramfs, verifies it exists, and updates the boot loader
/// (GRUB or systemd-boot, whichever the machine actually uses).
pub fn run_privileged_install(deb_dir: &str) -> Result<String> {
    if !Path::new(deb_dir).is_dir() {
        bail!("Download directory not found: {}", deb_dir);
    }
    run_script(&["--install".to_string(), deb_dir.to_string()])
}

/// Remove an installed kernel by full version string. The script refuses
/// to remove the running kernel.
pub fn run_privileged_remove(version: &str) -> Result<String> {
    run_script(&["--remove".to_string(), version.to_string()])
}

/// Remove several kernels under one authentication. The script never
/// removes the running kernel or the newest installed kernel.
pub fn run_privileged_remove_many(versions: &[String]) -> Result<String> {
    let mut args = vec!["--remove-many".to_string()];
    args.extend_from_slice(versions);
    run_script(&args)
}

/// Boot the given installed kernel on the next boot only. Does not reboot.
pub fn run_privileged_boot_once(version: &str) -> Result<String> {
    run_script(&["--boot-once".to_string(), version.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dkms_warnings_are_picked_out() {
        let out = "[kernel-pop] DKMS OK: zfs/2.2 installed for 6.8\n\
                   [kernel-pop] DKMS WARNING: nvidia/550 is 'built' for 6.8\n\
                   [kernel-pop] Verified: /boot/initrd.img-6.8 exists\n";
        let w = dkms_warnings(out);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("nvidia/550"));
        assert!(dkms_warnings("all fine\n").is_empty());
    }

    #[test]
    fn tail_lines_keeps_last_non_empty() {
        assert_eq!(tail_lines("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a\nb\n", 10), "a\nb");
        assert_eq!(tail_lines("", 3), "");
    }
}
