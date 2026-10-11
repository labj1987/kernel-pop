//! install.rs — Invoke the privileged script via pkexec.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SCRIPT: &str = "/usr/lib/kernel-pop/privileged-install.sh";
const KEYRING: &str = "/usr/lib/kernel-pop/kernel-ppa.gpg";

/// Lines of script output kept in an error message.
const ERROR_TAIL_LINES: usize = 15;

/// Every line the privileged script prints starts with this.
const LINE_PREFIX: &str = "[kernel-pop] ";

/// The files AppRun installs to fixed system paths, relative to both the
/// AppImage's root and the file system root. AppRun exports
/// KERNEL_POP_BUNDLE (the AppImage's mount point) so the app can compare the
/// bundled copies with the installed ones.
const HELPER_FILES: [&str; 2] = [SCRIPT, KEYRING];

/// Why the installed privileged helper cannot be used, or None when it
/// matches this build. A helper from another version would be run with
/// arguments it does not understand (or skip checks this version relies on),
/// so it is refused rather than run. Outside an AppImage (a development
/// build) there is nothing to compare against and only presence is checked.
pub fn helper_problem() -> Option<String> {
    let bundle = std::env::var_os("KERNEL_POP_BUNDLE").map(PathBuf::from);
    helper_problem_in(bundle.as_deref(), Path::new("/"))
}

fn helper_problem_in(bundle: Option<&Path>, root: &Path) -> Option<String> {
    let under = |base: &Path, abs: &str| base.join(abs.trim_start_matches('/'));
    let Some(bundle) = bundle else {
        return (!under(root, SCRIPT).exists())
            .then(|| format!("The privileged helper is not installed at {SCRIPT}."));
    };
    for file in HELPER_FILES {
        let (bundled, installed) = (under(bundle, file), under(root, file));
        let Ok(want) = std::fs::read(&bundled) else {
            return Some(format!("This Kernel Pop build is incomplete: {} is missing.", bundled.display()));
        };
        match std::fs::read(&installed) {
            Ok(have) if have == want => {}
            Ok(_) => {
                return Some(format!(
                    "The privileged helper at {file} is from another version of Kernel Pop: \
                     updating it at startup was cancelled or failed. Installing, removing and \
                     signing kernels stay off until it is updated — restart Kernel Pop and \
                     authenticate when asked."
                ))
            }
            Err(_) => {
                return Some(format!(
                    "The privileged helper is not installed ({file} is missing): installing \
                     it at startup was cancelled or failed. Installing, removing and signing \
                     kernels stay off until it is installed — restart Kernel Pop and \
                     authenticate when asked."
                ))
            }
        }
    }
    None
}

/// Run the privileged script and return what it printed. The same lines
/// also go to /var/log/kernel-pop.log, but returning them lets the UI show
/// per-step results (DKMS in particular) in its own Log tab.
fn run_script(args: &[String]) -> Result<String> {
    run_script_with_stdin(args, None)
}

/// Same, optionally writing `secret` (plus a newline) to the script's stdin,
/// so it never appears in an argument list or the environment.
fn run_script_with_stdin(args: &[String], secret: Option<&str>) -> Result<String> {
    if let Some(problem) = helper_problem() {
        bail!("{problem}");
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
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        bail!("{}", describe_failure(output.status.code(), &stdout, &stderr));
    }
    Ok(stdout)
}

/// What a failed run means. pkexec itself exits 126 when the authentication
/// dialog was dismissed and 127 when authorization failed or it could not
/// run the program; the script can exit with any code too (127 for a missing
/// command, say). The script prints its prefixed lines as soon as it starts,
/// so without any of them pkexec's code is pkexec's. stderr is always shown.
fn describe_failure(code: Option<i32>, stdout: &str, stderr: &str) -> String {
    let script_ran = stdout.lines().any(|l| l.starts_with(LINE_PREFIX));
    let err = tail_lines(stderr, ERROR_TAIL_LINES);
    let with_err = |msg: String| if err.is_empty() { msg } else { format!("{msg}\n{err}") };
    match code {
        Some(126) if !script_ran => "Authentication was cancelled.".to_string(),
        Some(127) if !script_ran => with_err(
            "Not authorized, or pkexec could not run the privileged helper.".to_string(),
        ),
        Some(c) => with_err(format!(
            "Script exited with code {c} (see /var/log/kernel-pop.log)\n{}",
            tail_lines(stdout, ERROR_TAIL_LINES)
        )),
        None => with_err(format!(
            "The privileged helper was killed by a signal (see /var/log/kernel-pop.log)\n{}",
            tail_lines(stdout, ERROR_TAIL_LINES)
        )),
    }
}

/// The last `n` non-empty lines of `text`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Whether a just-installed kernel will boot, as the script reported it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BootOutlook {
    /// Nothing reported against it.
    #[default]
    Ok,
    /// Boots once the signing key's enrollment is confirmed at the next reboot.
    AfterEnrollment,
    /// Will not boot while Secure Boot is enforcing.
    Blocked,
}

/// The problems a run of the privileged script reported, read from the
/// markers at the start of its lines (see the protocol at the top of the
/// script): one DKMS WARNING / SIGN WARNING line per problem, BOOT BLOCKED /
/// BOOT PENDING for whether the kernel boots. Summary lines carry no marker.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScriptReport {
    pub dkms_problems: usize,
    pub sign_problems: usize,
    pub boot: BootOutlook,
}

impl ScriptReport {
    pub fn parse(output: &str) -> Self {
        let mut r = ScriptReport::default();
        for line in output.lines() {
            let Some(text) = line.strip_prefix(LINE_PREFIX) else { continue };
            if text.starts_with("DKMS WARNING:") {
                r.dkms_problems += 1;
            } else if text.starts_with("SIGN WARNING:") {
                r.sign_problems += 1;
            } else if text.starts_with("BOOT BLOCKED:") {
                r.boot = BootOutlook::Blocked;
            } else if text.starts_with("BOOT PENDING:") && r.boot == BootOutlook::Ok {
                r.boot = BootOutlook::AfterEnrollment;
            }
        }
        r
    }
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

/// Install a downloaded .deb set. The script copies the set into a
/// root-only directory and verifies it there (CHECKSUMS signature, every
/// package's SHA256), derives the kernel version from the package metadata
/// (never from filenames), signs it when signing is set up, builds DKMS
/// modules, generates the initramfs, verifies it exists, and updates the
/// boot loader (GRUB or systemd-boot, whichever the machine actually uses).
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
    fn report_counts_one_per_problem_line_and_ignores_summaries() {
        let out = "[kernel-pop] DKMS OK: zfs/2.2 installed for 6.8\n\
                   [kernel-pop] DKMS WARNING: nvidia/550 is 'built' for 6.8\n\
                   [kernel-pop]   | make: *** DKMS WARNING: in a make.log tail\n\
                   [kernel-pop] SIGN WARNING: sbsign failed for /boot/vmlinuz-7.1\n\
                   [kernel-pop] 7.1 was installed but could not be signed (see the SIGN WARNING above)\n\
                   [kernel-pop] DKMS SUMMARY: 1 DKMS problem(s) for 6.8\n\
                   [kernel-pop] Verified: /boot/initrd.img-6.8 exists\n";
        let r = ScriptReport::parse(out);
        assert_eq!(r.dkms_problems, 1);
        assert_eq!(r.sign_problems, 1);
        assert_eq!(r.boot, BootOutlook::Ok);
        assert_eq!(ScriptReport::parse("all fine\n"), ScriptReport::default());
        assert_eq!(ScriptReport::parse("[kernel-pop] Kernel signing not set up\n").sign_problems, 0);
    }

    #[test]
    fn report_reads_the_boot_outlook() {
        let blocked = "[kernel-pop] BOOT BLOCKED: 7.1 will not boot while Secure Boot is enforcing: x.\n";
        assert_eq!(ScriptReport::parse(blocked).boot, BootOutlook::Blocked);
        let pending = "[kernel-pop] BOOT PENDING: 7.1 is signed, but the key is waiting.\n";
        assert_eq!(ScriptReport::parse(pending).boot, BootOutlook::AfterEnrollment);
        // Blocked wins whatever the order.
        assert_eq!(ScriptReport::parse(&format!("{blocked}{pending}")).boot, BootOutlook::Blocked);
        assert_eq!(ScriptReport::parse(&format!("{pending}{blocked}")).boot, BootOutlook::Blocked);
        // Mentioned mid-line is not a marker.
        assert_eq!(ScriptReport::parse("[kernel-pop] see BOOT BLOCKED: above\n").boot, BootOutlook::Ok);
    }

    #[test]
    fn failures_are_told_apart() {
        assert_eq!(describe_failure(Some(126), "", ""), "Authentication was cancelled.");
        let e = describe_failure(Some(127), "", "Error executing command as another user: Not authorized\n");
        assert!(e.starts_with("Not authorized"), "{e}");
        assert!(e.contains("Error executing command"), "{e}");
        // The script ran and itself exited 127 (a missing command): not an auth problem.
        let e = describe_failure(
            Some(127),
            "[kernel-pop] ==== Kernel install started ====\n",
            "privileged-install.sh: line 9: gpgv: command not found\n",
        );
        assert!(e.starts_with("Script exited with code 127"), "{e}");
        assert!(e.contains("Kernel install started") && e.contains("gpgv: command not found"), "{e}");
        let e = describe_failure(Some(126), "[kernel-pop] started\n", "");
        assert!(e.starts_with("Script exited with code 126"), "{e}");
        let e = describe_failure(Some(1), "[kernel-pop] ERROR: boom\n", "");
        assert!(e.contains("ERROR: boom") && !e.ends_with('\n'), "{e}");
        assert!(describe_failure(None, "", "").contains("killed by a signal"));
    }

    #[test]
    fn helper_must_match_the_bundle() {
        let base = std::env::temp_dir().join(format!("kernel-pop-test-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (bundle, root) = (base.join("bundle"), base.join("root"));
        for d in [&bundle, &root] {
            std::fs::create_dir_all(d.join("usr/lib/kernel-pop")).unwrap();
        }
        let put = |d: &Path, f: &str, b: &[u8]| std::fs::write(d.join(f.trim_start_matches('/')), b).unwrap();
        put(&bundle, SCRIPT, b"script v2");
        put(&bundle, KEYRING, b"ring");
        // Nothing installed
        assert!(helper_problem_in(Some(&bundle), &root).unwrap().contains("not installed"));
        // Installed, but an older script
        put(&root, SCRIPT, b"script v1");
        put(&root, KEYRING, b"ring");
        assert!(helper_problem_in(Some(&bundle), &root).unwrap().contains("another version"));
        // Script current but the keyring missing
        put(&root, SCRIPT, b"script v2");
        std::fs::remove_file(root.join(KEYRING.trim_start_matches('/'))).unwrap();
        assert!(helper_problem_in(Some(&bundle), &root).unwrap().contains("kernel-ppa.gpg is missing"));
        // Both current
        put(&root, KEYRING, b"ring");
        assert_eq!(helper_problem_in(Some(&bundle), &root), None);
        // An incomplete bundle
        std::fs::remove_file(bundle.join(KEYRING.trim_start_matches('/'))).unwrap();
        assert!(helper_problem_in(Some(&bundle), &root).unwrap().contains("incomplete"));
        // Development build: only presence
        assert_eq!(helper_problem_in(None, &root), None);
        assert!(helper_problem_in(None, &base.join("nowhere")).is_some());
        std::fs::remove_dir_all(&base).unwrap();
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
