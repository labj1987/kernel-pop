//! install.rs — Invoke the privileged script via pkexec.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = "/usr/lib/kernel-pop/privileged-install.sh";

fn run_script(args: &[String]) -> Result<()> {
    if !Path::new(SCRIPT).exists() {
        bail!("Privileged script not found at {}", SCRIPT);
    }

    let mut full = vec![SCRIPT.to_string()];
    full.extend_from_slice(args);

    let status = Command::new("pkexec")
        .args(&full)
        .status()
        .context("Failed to launch pkexec — is polkit installed?")?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        if code == 126 || code == 127 {
            bail!("Authentication was cancelled.");
        }
        bail!(
            "Script exited with code {} (see /var/log/kernel-pop.log)",
            code
        );
    }
    Ok(())
}

/// Install a downloaded .deb set. The script derives the kernel version
/// from the package metadata (never from filenames), generates the
/// initramfs, verifies it exists, and updates the boot loader (GRUB or
/// systemd-boot, whichever the machine actually uses).
pub fn run_privileged_install(deb_dir: &str) -> Result<()> {
    if !Path::new(deb_dir).is_dir() {
        bail!("Download directory not found: {}", deb_dir);
    }
    run_script(&["--install".to_string(), deb_dir.to_string()])
}

/// Remove an installed kernel by full version string. The script refuses
/// to remove the running kernel.
pub fn run_privileged_remove(version: &str) -> Result<()> {
    run_script(&["--remove".to_string(), version.to_string()])
}
