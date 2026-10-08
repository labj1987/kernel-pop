//! install.rs — Invoke the privileged script via pkexec.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const SCRIPT: &str = "/usr/lib/kernel-pop/privileged-install.sh";

/// Lines of script output kept in an error message.
const ERROR_TAIL_LINES: usize = 15;

/// Run the privileged script and return what it printed. The same lines
/// also go to /var/log/kernel-pop.log, but returning them lets the UI show
/// per-step results (DKMS in particular) in its own Log tab.
fn run_script(args: &[String]) -> Result<String> {
    run_script_with_stdin(args, None)
}

/// Same, optionally writing `secret` (plus a newline) to the script's stdin,
/// so it never appears in an argument list or the environment.
fn run_script_with_stdin(args: &[String], secret: Option<&str>) -> Result<String> {
    if !Path::new(SCRIPT).exists() {
        bail!("Privileged script not found at {}", SCRIPT);
    }

    let mut full = vec![SCRIPT.to_string()];
    full.extend_from_slice(args);

    let mut cmd = Command::new("pkexec");
    cmd.args(&full).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if secret.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd
        .spawn()
        .context("Failed to launch pkexec — is polkit installed?")?;
    if let (Some(secret), Some(mut stdin)) = (secret, child.stdin.take()) {
        // A write error (the script exited early, e.g. on a cancelled
        // authentication) shows up in the exit status below.
        let _ = stdin.write_all(format!("{secret}\n").as_bytes());
    }
    let output = child.wait_with_output().context("Failed to wait for pkexec")?;
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

/// Lines the script flagged as signing problems. A non-empty result means a
/// kernel (or its ESP copy) may be unsigned.
pub fn sign_warnings(output: &str) -> Vec<&str> {
    output.lines().filter(|l| l.contains("SIGN WARNING")).collect()
}

/// MOK Manager asks for this password at the next boot, on a US keyboard
/// layout: 8-16 printable ASCII characters, no spaces, typed twice the same.
pub fn validate_mok_password(password: &str, confirm: &str) -> Result<(), &'static str> {
    let n = password.chars().count();
    if !(8..=16).contains(&n) {
        return Err("Use 8 to 16 characters");
    }
    if !password.chars().all(|c| c.is_ascii_graphic()) {
        return Err("Use plain letters, digits and symbols only (no spaces or accents)");
    }
    if password != confirm {
        return Err("The two passwords do not match");
    }
    Ok(())
}

/// One-time signing setup: installs the signing tools if missing, creates
/// the key, signs every installed kernel and queues the key for MOK
/// enrollment with `password` (sent on stdin).
pub fn run_privileged_setup_signing(password: &str) -> Result<String> {
    run_script_with_stdin(&["--setup-signing".to_string()], Some(password))
}

/// Sign one installed kernel with the existing key.
pub fn run_privileged_sign(version: &str) -> Result<String> {
    run_script(&["--sign".to_string(), version.to_string()])
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
    fn sign_warnings_are_picked_out() {
        let out = "[kernel-pop] Signed: /boot/vmlinuz-7.1\n\
                   [kernel-pop] SIGN WARNING: the ESP copy /boot/efi/x/linux does not carry the Kernel Pop signature\n\
                   [kernel-pop] DKMS WARNING: nvidia/550 is 'built' for 7.1\n";
        let w = sign_warnings(out);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("ESP copy"));
        assert!(sign_warnings("[kernel-pop] Kernel signing not set up\n").is_empty());
    }

    #[test]
    fn mok_password_validation() {
        assert!(validate_mok_password("abcdefgh", "abcdefgh").is_ok());
        assert!(validate_mok_password("Abc123!@#$%^&*()", "Abc123!@#$%^&*()").is_ok());
        assert!(validate_mok_password("short", "short").is_err());
        assert!(validate_mok_password("abcdefghijklmnopq", "abcdefghijklmnopq").is_err());
        assert!(validate_mok_password("has space1", "has space1").is_err());
        assert!(validate_mok_password("pässwörd1", "pässwörd1").is_err());
        assert!(validate_mok_password("abcdefgh", "abcdefgx").is_err());
    }

    #[test]
    fn tail_lines_keeps_last_non_empty() {
        assert_eq!(tail_lines("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a\nb\n", 10), "a\nb");
        assert_eq!(tail_lines("", 3), "");
    }
}
