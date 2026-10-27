//! build.rs — Configure a kernel.org tree for this machine and build it into
//! Debian packages with `make bindeb-pkg`, all as the user (never as root).
//!
//! The flow is split in two so the configuration can be shown before the
//! long part starts:
//! - `prepare`: disk preflight, extract the tarball into
//!   ~/.cache/kernel-pop/build/linux-X.Y.Z/, configure it (base config, the
//!   fixes every Ubuntu config needs, olddefconfig, optional localmodconfig)
//!   and return a summary.
//! - `build`: `make bindeb-pkg`, then move the image and headers packages to
//!   ~/Downloads/kernel-pop/linux-<release>/ with the final config, the build
//!   log and a SHA256SUMS manifest, and delete the build tree.
//!
//! Every command runs in its own process group so a cancel can kill the whole
//! build (make, the compilers, dpkg-buildpackage) at once.

use crate::source::Tarball;
use crate::verify::Signer;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

/// Appended to every kernel Kernel Pop builds: 7.2.12 becomes 7.2.12-kernelpop.
pub const LOCALVERSION: &str = "-kernelpop";

/// Packages a `make bindeb-pkg` of an Ubuntu-derived config needs, confirmed
/// with a real build in a clean Ubuntu 26.04 container (docs/SOURCE_BUILD.md).
/// bindeb-pkg itself checks bc, bison, flex, debhelper (debhelper-compat),
/// kmod, libdw-dev, libelf-dev, libssl-dev, python3 and rsync; the Ubuntu
/// config adds pahole (BTF), zstd (module compression), cpio, openssl
/// (module signing key) and gawk (modules.builtin.ranges).
/// The privileged script's --install-build-deps accepts exactly this list.
pub const BUILD_DEPS: &[&str] = &[
    "build-essential",
    "dpkg-dev",
    "bc",
    "bison",
    "flex",
    "debhelper",
    "kmod",
    "libdw-dev",
    "libelf-dev",
    "libssl-dev",
    "python3",
    "rsync",
    "xz-utils",
    "pahole",
    "zstd",
    "cpio",
    "openssl",
    "gawk",
];

/// Free space a build needs in ~/.cache, measured (docs/SOURCE_BUILD.md:
/// 6.7 GB of tree and 0.13 GB of packages for Ubuntu's config with debug
/// info off) plus a margin for ccache and a bigger config.
pub const BUILD_NEED_BYTES: u64 = 12 * 1024 * 1024 * 1024;
/// The same for "Tuned for this machine", which builds far fewer modules.
pub const TUNED_BUILD_NEED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Drivers "Tuned for this machine" always keeps, for hardware that is often
/// plugged in later or used only now and then: USB storage and card
/// readers, the common file systems, input devices and game controllers,
/// USB audio and webcams, Bluetooth, USB network adapters and phone
/// tethering, docks, printers and serial adapters, and the networking that
/// VPNs, containers and virtual machines load on demand. Names that do not
/// exist in a tree are ignored by localmodconfig.
pub const TUNED_EXTRA_MODULES: &[&str] = &[
    // storage and card readers
    "usb_storage",
    "uas",
    "sd_mod",
    "sr_mod",
    "cdrom",
    "mmc_block",
    "sdhci",
    "sdhci_pci",
    "sdhci_acpi",
    "rtsx_pci",
    "rtsx_pci_sdmmc",
    "rtsx_usb",
    "rtsx_usb_sdmmc",
    "nvme",
    // file systems
    "vfat",
    "fat",
    "msdos",
    "exfat",
    "ntfs3",
    "isofs",
    "udf",
    "ext4",
    "btrfs",
    "xfs",
    "f2fs",
    "hfsplus",
    "fuse",
    "squashfs",
    "overlay",
    "nls_utf8",
    "nls_cp437",
    "nls_iso8859_1",
    "nls_ascii",
    "cifs",
    "nfs",
    "nfsv4",
    // input and game controllers
    "usbhid",
    "hid_generic",
    "hid_multitouch",
    "hid_logitech_dj",
    "hid_logitech_hidpp",
    "hid_sony",
    "hid_playstation",
    "hid_nintendo",
    "hid_microsoft",
    "hid_apple",
    "hid_steam",
    "xpad",
    "joydev",
    "uinput",
    "i2c_hid_acpi",
    // audio and video
    "snd_usb_audio",
    "uvcvideo",
    // Bluetooth
    "bluetooth",
    "btusb",
    "btintel",
    "btrtl",
    "btbcm",
    "btmtk",
    // USB network adapters and phone tethering
    "usbnet",
    "cdc_ether",
    "cdc_ncm",
    "cdc_mbim",
    "rndis_host",
    "r8152",
    "ax88179_178a",
    "asix",
    "ipheth",
    // docks, USB-C and Thunderbolt
    "thunderbolt",
    "typec_displayport",
    "ucsi_acpi",
    "typec_ucsi",
    // printers and serial adapters
    "usblp",
    "cdc_acm",
    "usbserial",
    "ftdi_sio",
    "cp210x",
    "ch341",
    "pl2303",
    // VPNs, containers and virtual machines
    "tun",
    "tap",
    "wireguard",
    "veth",
    "bridge",
    "br_netfilter",
    "vxlan",
    "macvlan",
    "ipvlan",
    "nf_tables",
    "nft_compat",
    "nft_chain_nat",
    "nf_nat",
    "nf_conntrack",
    "xt_MASQUERADE",
    "xt_conntrack",
    "xt_addrtype",
    "iptable_nat",
    "iptable_filter",
    "ip6table_nat",
    "ip6table_filter",
    "kvm",
    "kvm_intel",
    "kvm_amd",
    "vhost_net",
    "vhost_vsock",
    // encrypted and image-backed disks, compressed swap, Wine/Proton sync
    "dm_crypt",
    "dm_mod",
    "loop",
    "zram",
    "ntsync",
];

/// The build profile that keeps bindeb-pkg from making a linux-image-*-dbg
/// package (its prefix follows KDEB_SOURCENAME, left at its default).
const NO_DBG_PROFILE: &str = "pkg.linux-upstream.nokerneldbg";

/// Config symbols the kernel needs to trust module signatures from the
/// machine keyring (MOK keys, which DKMS signs with) under Secure Boot.
const MOK_TRUST_SYMBOLS: &[&str] =
    &["MODULE_SIG", "SYSTEM_TRUSTED_KEYRING", "SECONDARY_TRUSTED_KEYRING", "INTEGRITY_PLATFORM_KEYRING", "LOAD_UEFI_KEYS", "INTEGRITY_MACHINE_KEYRING"];

/// Which of `BUILD_DEPS` are not installed, from `dpkg-query` output lines
/// "<package> <status-abbrev>" (only "ii" counts as installed).
pub fn missing_from_dpkg_query(output: &str) -> Vec<String> {
    let installed: Vec<&str> = output
        .lines()
        .filter_map(|l| {
            let (pkg, st) = l.split_once(' ')?;
            st.trim().starts_with("ii").then_some(pkg.split(':').next().unwrap_or(pkg))
        })
        .collect();
    BUILD_DEPS.iter().filter(|p| !installed.contains(p)).map(|p| p.to_string()).collect()
}

pub fn missing_build_deps() -> Vec<String> {
    let out = Command::new("dpkg-query").args(["-W", "-f", "${Package} ${db:Status-Abbrev}\n"]).args(BUILD_DEPS).output();
    match out {
        Ok(o) => missing_from_dpkg_query(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => BUILD_DEPS.iter().map(|p| p.to_string()).collect(),
    }
}

/// Whether ccache is installed (used when it is: consecutive point releases
/// share most objects).
pub fn ccache_available() -> bool {
    Command::new("ccache").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// ~/.cache/kernel-pop/build (or under $XDG_CACHE_HOME): Kernel Pop's own
/// build directory. Only one build runs at a time, and anything left in it
/// from an earlier build is removed when the next one starts.
pub fn build_root() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".cache"));
    cache.join("kernel-pop").join("build")
}

/// The build tree for a source of `version`: build_root()/linux-X.Y.Z.
/// Its build.log stays there after a failed build.
pub fn tree_dir(version: &crate::source::SourceVersion) -> PathBuf {
    build_root().join(format!("linux-{}", version.kernelversion()))
}

/// ~/Downloads/kernel-pop: downloaded tarballs and finished builds.
pub fn downloads_root() -> PathBuf {
    home().join("Downloads").join("kernel-pop")
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigMode {
    /// /boot/config-$(uname -r), with the fixes.
    MatchSystem,
    /// The same, then localmodconfig against the loaded modules, the
    /// drivers for every device present, and `TUNED_EXTRA_MODULES`.
    TunedForMachine,
    /// A .config file the user chose, with the fixes.
    OwnConfig(PathBuf),
}

impl ConfigMode {
    pub fn label(&self) -> &'static str {
        match self {
            ConfigMode::MatchSystem => "Match this system",
            ConfigMode::TunedForMachine => "Tuned for this machine",
            ConfigMode::OwnConfig(_) => "Use my own .config",
        }
    }

    pub fn disk_need(&self) -> u64 {
        match self {
            ConfigMode::TunedForMachine => TUNED_BUILD_NEED_BYTES,
            _ => BUILD_NEED_BYTES,
        }
    }
}

/// Whether the source's signature checked out.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceTrust {
    Verified(Signer),
    /// Built anyway, on the user's explicit say-so for this tarball.
    Unverified {
        reason: String,
    },
}

impl SourceTrust {
    pub fn describe(&self) -> String {
        match self {
            SourceTrust::Verified(s) => format!("verified: signed by {} ({})", s.uid, s.grouped_fingerprint()),
            SourceTrust::Unverified { reason } => format!("UNVERIFIED — built without a valid kernel.org signature ({reason})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub tarball: Tarball,
    pub trust: SourceTrust,
    pub mode: ConfigMode,
    /// uname -r, whose /boot/config-* the system modes start from.
    pub running_kernel: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    Extract,
    Configure,
    Build,
    Package,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Extract => "Extracting",
            Stage::Configure => "Configuring",
            Stage::Build => "Building",
            Stage::Package => "Packaging",
        }
    }
}

#[derive(Debug, Clone)]
pub enum BuildEvent {
    Stage(Stage),
    Line(String),
}

pub type EventSink = Arc<dyn Fn(BuildEvent) + Send + Sync>;

/// The error a build ends with when it was cancelled.
#[derive(Debug)]
pub struct BuildCancelled;

impl std::fmt::Display for BuildCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Build cancelled")
    }
}

impl std::error::Error for BuildCancelled {}

pub fn is_build_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<BuildCancelled>().is_some()
}

/// Cancels a running build: kills the process group of whatever command is
/// running now, and keeps any later one from starting.
#[derive(Default)]
pub struct BuildControl {
    cancelled: AtomicBool,
    pgid: AtomicI32,
}

impl BuildControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// SIGTERM to the whole process group, SIGKILL five seconds later if it
    /// is still there.
    pub fn cancel(self: &Arc<Self>) {
        self.cancelled.store(true, Ordering::SeqCst);
        let pg = self.pgid.load(Ordering::SeqCst);
        if pg > 0 {
            kill_group(pg, libc::SIGTERM);
            let me = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if me.pgid.load(Ordering::SeqCst) == pg {
                    kill_group(pg, libc::SIGKILL);
                }
            });
        }
    }
}

fn kill_group(pgid: i32, sig: i32) {
    // SAFETY: killpg only sends a signal; a stale pgid fails with ESRCH.
    unsafe {
        libc::killpg(pgid, sig);
    }
}

/// Runs the commands of one build: logs every output line to build.log and
/// to the event sink, and registers each process group with the control.
struct Runner {
    events: EventSink,
    log: Mutex<std::fs::File>,
    control: Arc<BuildControl>,
}

impl Runner {
    fn note(&self, line: impl Into<String>) {
        let line = line.into();
        if let Ok(mut f) = self.log.lock() {
            let _ = writeln!(f, "{line}");
        }
        (self.events)(BuildEvent::Line(line));
    }

    fn stage(&self, s: Stage) {
        self.note(format!("==== {} ====", s.label()));
        (self.events)(BuildEvent::Stage(s));
    }

    /// Run `cmd` with stdout and stderr merged into the log. Returns the
    /// output lines when `keep` is set. `on_line` sees every line.
    fn run_with(&self, mut cmd: Command, what: &str, keep: bool, on_line: &dyn Fn(&str)) -> Result<(bool, Vec<String>)> {
        if self.control.is_cancelled() {
            return Err(BuildCancelled.into());
        }
        let (reader, writer) = std::io::pipe().context("Could not create a pipe")?;
        cmd.stdin(Stdio::null()).stdout(writer.try_clone().context("Could not duplicate a pipe")?).stderr(writer).process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("Could not start {what}"))?;
        // The Command holds the pipe's write end: drop it, or the read below
        // never sees the end of the output.
        drop(cmd);
        self.control.pgid.store(child.id() as i32, Ordering::SeqCst);
        if self.control.is_cancelled() {
            kill_group(child.id() as i32, libc::SIGKILL);
        }
        let mut kept = vec![];
        for line in BufReader::new(reader).split(b'\n') {
            let Ok(line) = line else { break };
            let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
            on_line(&text);
            if keep {
                kept.push(text.clone());
            }
            self.note(text);
        }
        let status = child.wait().with_context(|| format!("Could not wait for {what}"))?;
        self.control.pgid.store(0, Ordering::SeqCst);
        if self.control.is_cancelled() {
            return Err(BuildCancelled.into());
        }
        Ok((status.success(), kept))
    }

    /// Run `cmd`; a non-zero exit is an error.
    fn run(&self, cmd: Command, what: &str) -> Result<Vec<String>> {
        let (ok, lines) = self.run_with(cmd, what, true, &|_| {})?;
        if !ok {
            bail!("{what} failed — see the build log");
        }
        Ok(lines)
    }
}

/// `CONFIG_<name>`'s value in a .config: Some("y"), Some("\"-kernelpop\""),
/// or None when it is not set.
pub fn config_value(config: &str, name: &str) -> Option<String> {
    let key = format!("CONFIG_{name}=");
    config.lines().find_map(|l| l.strip_prefix(&key).map(|v| v.to_string()))
}

/// The number of =m symbols: how many modules the configuration builds.
pub fn module_count(config: &str) -> usize {
    config.lines().filter(|l| l.starts_with("CONFIG_") && l.ends_with("=m")).count()
}

/// The debug-info settings every build gets, as scripts/config arguments
/// and a description for the summary (the measurement behind it is in
/// docs/SOURCE_BUILD.md).
fn debug_info_args() -> (Vec<&'static str>, &'static str) {
    (
        vec![
            "--disable",
            "DEBUG_INFO_DWARF5",
            "--disable",
            "DEBUG_INFO_DWARF4",
            "--disable",
            "DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT",
            "--enable",
            "DEBUG_INFO_NONE",
        ],
        "turned off (half the build time and a sixth of the disk space; this also turns off BTF)",
    )
}

/// What the configuration step did, shown before the build starts.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSummary {
    pub base_config: PathBuf,
    pub mode: ConfigMode,
    pub localversion: String,
    pub kernelrelease: String,
    pub fixes: Vec<String>,
    /// Things the configuration changed on its own or that need attention.
    pub notes: Vec<String>,
    pub modules: usize,
    pub ccache: bool,
}

impl ConfigSummary {
    pub fn text(&self) -> String {
        let mut s = format!(
            "Kernel: {}\nConfiguration: {}\nBase config: {}\nLOCALVERSION: {}\nModules: {}\nccache: {}\n\nFixes applied:\n",
            self.kernelrelease,
            self.mode.label(),
            self.base_config.display(),
            self.localversion,
            self.modules,
            if self.ccache { "used" } else { "not installed" },
        );
        for f in &self.fixes {
            s.push_str(&format!("• {f}\n"));
        }
        if !self.notes.is_empty() {
            s.push_str("\nNotes:\n");
            for n in &self.notes {
                s.push_str(&format!("• {n}\n"));
            }
        }
        s.trim_end().to_string()
    }
}

/// A configured tree, ready for `build`.
pub struct Prepared {
    pub request: BuildRequest,
    pub tree: PathBuf,
    pub summary: ConfigSummary,
    log: PathBuf,
}

impl Prepared {
    /// Throw the configured tree away (the summary was not accepted).
    pub fn discard(self) {
        let _ = std::fs::remove_dir_all(&self.tree);
    }
}

/// Free bytes on the file system holding `path` (or its nearest existing parent).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let out = Command::new("df").args(["-B1", "--output=avail"]).arg(p).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().nth(1)?.trim().parse().ok()
}

/// Why a build cannot start for lack of space, or None. `boot_need` is what
/// one more kernel needs in /boot.
pub fn space_problem(mode: &ConfigMode, cache_free: Option<u64>, boot_free: Option<u64>, boot_need: u64) -> Option<String> {
    use crate::system::format_bytes;
    let need = mode.disk_need();
    if let Some(f) = cache_free.filter(|f| *f < need) {
        return Some(format!(
            "A build needs about {} free in {} and there is {} — free some space first.",
            format_bytes(need),
            build_root().display(),
            format_bytes(f)
        ));
    }
    if let Some(f) = boot_free.filter(|f| *f < boot_need) {
        return Some(format!(
            "/boot has {} free but the new kernel needs about {} there — remove old kernels first.",
            format_bytes(f),
            format_bytes(boot_need)
        ));
    }
    None
}

fn make(tree: &Path, ccache: bool) -> Command {
    let mut c = Command::new("make");
    c.current_dir(tree);
    if ccache {
        c.arg("CC=ccache gcc").env("CCACHE_BASEDIR", tree);
    }
    c
}

fn scripts_config(tree: &Path) -> Command {
    let mut c = Command::new("bash");
    c.current_dir(tree).args(["scripts/config", "--file", ".config"]);
    c
}

/// Bytes used by everything under `path` (symlinks not followed); 0 when
/// it does not exist.
pub fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map(|rd| rd.flatten().map(|e| dir_size(&e.path())).sum()).unwrap_or(0)
}

/// Remove everything in the build root (a failed or cancelled earlier
/// build). It is Kernel Pop's own directory.
fn clear_build_root(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).with_context(|| format!("Could not create {}", root.display()))?;
    for e in std::fs::read_dir(root)?.flatten() {
        let p = e.path();
        let r = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        r.with_context(|| format!("Could not remove the old build file {}", p.display()))?;
    }
    Ok(())
}

/// Extract and configure. On error or cancel the tree is removed again.
pub fn prepare(req: BuildRequest, events: EventSink, control: Arc<BuildControl>, boot_need: u64) -> Result<Prepared> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        bail!("Kernel Pop does not build kernels as root. Run it as your own user; only the install asks for authentication.");
    }
    let missing = missing_build_deps();
    if !missing.is_empty() {
        bail!("Build tools are missing: {}", missing.join(", "));
    }
    let root = build_root();
    let base_config = match &req.mode {
        ConfigMode::OwnConfig(p) => p.clone(),
        _ => PathBuf::from(format!("/boot/config-{}", req.running_kernel)),
    };
    if !base_config.is_file() {
        bail!("The base config {} does not exist", base_config.display());
    }
    // What an earlier failed build left is Kernel Pop's own and goes first,
    // so it does not count against the space this build needs.
    clear_build_root(&root)?;
    if let Some(p) = space_problem(&req.mode, free_bytes(&root), free_bytes(Path::new("/boot")), boot_need) {
        bail!("{p}");
    }
    let tree = tree_dir(&req.tarball.version);
    std::fs::create_dir_all(&tree).with_context(|| format!("Could not create {}", tree.display()))?;
    let log = tree.join("build.log");
    let file = std::fs::File::create(&log).with_context(|| format!("Could not create {}", log.display()))?;
    let runner = Runner { events, log: Mutex::new(file), control };

    match configure(&req, &tree, &base_config, &runner) {
        Ok(summary) => Ok(Prepared { request: req, tree, summary, log }),
        Err(e) => {
            if is_build_cancelled(&e) {
                let _ = std::fs::remove_dir_all(&tree);
            } else {
                runner.note(format!("ERROR: {e:#}"));
            }
            Err(e)
        }
    }
}

fn configure(req: &BuildRequest, tree: &Path, base_config: &Path, r: &Runner) -> Result<ConfigSummary> {
    let ccache = ccache_available();
    r.note(format!("Kernel Pop {} source build", env!("CARGO_PKG_VERSION")));
    r.note(format!(
        "Source: {} (Linux {}; building Kernel Pop's private copy {})",
        req.tarball.origin.display(),
        req.tarball.version.kernelversion(),
        req.tarball.path.display()
    ));
    r.note(format!("Source signature: {}", req.trust.describe()));
    if let SourceTrust::Unverified { .. } = req.trust {
        r.note("WARNING: this source was NOT verified against a kernel.org signature; it is built only because that was explicitly allowed for this tarball.");
    }
    r.note(format!("Configuration: {} (base {})", req.mode.label(), base_config.display()));

    r.stage(Stage::Extract);
    crate::source::check_single_top_dir(&req.tarball)?;
    let mut tar = Command::new("tar");
    tar.arg("-xf").arg(&req.tarball.path).arg("-C").arg(tree).args(["--strip-components=1", "--no-same-owner"]);
    r.run(tar, "Extracting the source")?;
    if !tree.join("Makefile").is_file() || !tree.join("scripts/config").is_file() {
        bail!("The extracted source has no Makefile or scripts/config — not a kernel tree");
    }

    r.stage(Stage::Configure);
    std::fs::copy(base_config, tree.join(".config")).with_context(|| format!("Could not copy {}", base_config.display()))?;
    let base = std::fs::read_to_string(tree.join(".config"))?;
    let base_rust = config_value(&base, "RUST").as_deref() == Some("y");

    let (debug_args, debug_text) = debug_info_args();
    let mut fixes = vec![
        format!("LOCALVERSION set to \"{LOCALVERSION}\""),
        "SYSTEM_TRUSTED_KEYS set to \"\" (Ubuntu's points at Canonical certificate files that are not in a kernel.org tree)".to_string(),
        "SYSTEM_REVOCATION_KEYS set to \"\" (same reason)".to_string(),
        format!("Debug info {debug_text}"),
    ];
    let apply_fixes = || -> Result<()> {
        let mut c = scripts_config(tree);
        c.args(["--set-str", "LOCALVERSION", LOCALVERSION])
            .args(["--set-str", "SYSTEM_TRUSTED_KEYS", ""])
            .args(["--set-str", "SYSTEM_REVOCATION_KEYS", ""])
            .args(&debug_args);
        r.run(c, "scripts/config").map(|_| ())
    };
    apply_fixes()?;

    let mut notes = vec![];
    if base_rust {
        let mut c = make(tree, ccache);
        c.arg("rustavailable");
        let (ok, lines) = r.run_with(c, "make rustavailable", true, &|_| {})?;
        if !ok {
            let why: Vec<String> = lines
                .iter()
                .filter_map(|l| l.strip_prefix("***"))
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.contains("Documentation/rust") && !l.starts_with("on how to set up"))
                .collect();
            let why = if why.is_empty() { "make rustavailable failed".to_string() } else { why.join(" ") };
            let mut c = scripts_config(tree);
            c.args(["--disable", "RUST"]);
            r.run(c, "scripts/config")?;
            let note = format!("CONFIG_RUST turned off: the installed Rust toolchain does not satisfy make rustavailable ({why})");
            r.note(note.clone());
            fixes.push(note);
        } else {
            r.note("Rust toolchain: make rustavailable is satisfied, CONFIG_RUST stays on");
        }
    }

    let olddefconfig = || -> Result<()> {
        let mut c = make(tree, ccache);
        c.arg("olddefconfig");
        r.run(c, "make olddefconfig").map(|_| ())
    };
    olddefconfig()?;

    if req.mode == ConfigMode::TunedForMachine {
        let loaded = loaded_modules().context("Could not read the loaded modules")?;
        let present = present_device_modules();
        let new_present = present.iter().filter(|m| !loaded.contains(m)).count();
        let what = format!(
            "the {} modules loaded right now, {} more for devices present but not in use, and {} common plug-in, VPN, container and VM drivers",
            loaded.len(),
            new_present,
            TUNED_EXTRA_MODULES.len()
        );
        let mut keep: Vec<String> = loaded;
        keep.extend(present);
        keep.extend(TUNED_EXTRA_MODULES.iter().map(|m| m.to_string()));
        keep.sort();
        keep.dedup();
        let list = tree.join(".kernel-pop-lsmod");
        std::fs::write(&list, lsmod_text(&keep))?;
        let mut c = make(tree, ccache);
        c.arg(format!("LSMOD={}", list.display())).arg("localmodconfig");
        r.run(c, "make localmodconfig")?;
        apply_fixes()?;
        olddefconfig()?;
        fixes.push(format!("localmodconfig: only drivers for {what}"));
        notes.push(
            "Tuned: hardware that is not present now and not a common plug-in kind (an unusual USB gadget, a different Wi-Fi card) may have no driver in this kernel; Match this system includes everything.".into(),
        );
    }

    let config = std::fs::read_to_string(tree.join(".config"))?;
    let expect = |name: &str, want: &str| -> Result<()> {
        match config_value(&config, name) {
            Some(v) if v == want => Ok(()),
            other => bail!("CONFIG_{name} is {other:?} after configuring, expected {want} — refusing to build"),
        }
    };
    expect("LOCALVERSION", &format!("\"{LOCALVERSION}\""))?;
    if config_value(&config, "DEBUG_INFO").is_some() {
        bail!("CONFIG_DEBUG_INFO is still on after configuring — refusing to build");
    }
    if config_value(&config, "SYSTEM_TRUSTED_KEYS").is_some() {
        expect("SYSTEM_TRUSTED_KEYS", "\"\"")?;
    }
    if config_value(&config, "SYSTEM_REVOCATION_KEYS").is_some() {
        expect("SYSTEM_REVOCATION_KEYS", "\"\"")?;
    }
    if base_rust && config_value(&config, "RUST").is_none() && !fixes.iter().any(|f| f.starts_with("CONFIG_RUST")) {
        notes.push("CONFIG_RUST was turned off by the kernel's own dependency rules during olddefconfig.".into());
    }
    let off: Vec<&str> = MOK_TRUST_SYMBOLS.iter().copied().filter(|s| config_value(&config, s).as_deref() != Some("y")).collect();
    if !off.is_empty() {
        notes.push(format!("CONFIG_{} is off: under Secure Boot this kernel would not load DKMS modules signed with your MOK key.", off.join(", CONFIG_")));
    }
    if config_value(&config, "INTEGRITY_CA_MACHINE_KEYRING").as_deref() == Some("y") {
        notes
            .push("CONFIG_INTEGRITY_CA_MACHINE_KEYRING is on: only CA keys from the MOK list are trusted, so a DKMS key that is not a CA would not be.".into());
    }

    let mut c = make(tree, ccache);
    c.args(["-s", "kernelrelease"]);
    let lines = r.run(c, "make kernelrelease")?;
    let kernelrelease = lines.iter().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()).unwrap_or_default();
    if !valid_release(&kernelrelease) {
        bail!("make kernelrelease gave an unusable version: '{kernelrelease}'");
    }
    let wanted = format!("{}{LOCALVERSION}", req.tarball.version.kernelversion());
    if kernelrelease != wanted {
        notes.push(format!("The tree adds its own local version: the kernel will be {kernelrelease}, not {wanted}."));
    }
    let summary = ConfigSummary {
        base_config: base_config.to_path_buf(),
        mode: req.mode.clone(),
        localversion: LOCALVERSION.to_string(),
        kernelrelease,
        fixes,
        notes,
        modules: module_count(&config),
        ccache,
    };
    for l in summary.text().lines() {
        r.note(l.to_string());
    }
    Ok(summary)
}

/// Module names from /proc/modules.
fn loaded_modules() -> Result<Vec<String>> {
    Ok(parse_proc_modules(&std::fs::read_to_string("/proc/modules")?))
}

pub fn parse_proc_modules(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.split_whitespace().next()).map(normalize_module).collect()
}

/// Module names use '_' and '-' interchangeably; localmodconfig compares
/// them with '_'.
fn normalize_module(name: &str) -> String {
    name.trim().replace('-', "_")
}

/// The drivers for every device the kernel sees, in use or not: each
/// device's modalias resolved with `modprobe --resolve-alias`.
fn present_device_modules() -> Vec<String> {
    let mut aliases = std::collections::BTreeSet::new();
    collect_modaliases(Path::new("/sys/devices"), &mut aliases, 0);
    let mut mods = std::collections::BTreeSet::new();
    for chunk in aliases.into_iter().collect::<Vec<_>>().chunks(64) {
        // One modprobe per alias (it resolves one at a time); run them in a
        // single shell to keep the process count down.
        let script = chunk.iter().map(|a| format!("modprobe -R {} 2>/dev/null;", shell_quote(a))).collect::<String>();
        if let Ok(out) = Command::new("sh").arg("-c").arg(script).output() {
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                if !l.trim().is_empty() {
                    mods.insert(normalize_module(l));
                }
            }
        }
    }
    mods.into_iter().collect()
}

fn collect_modaliases(dir: &Path, out: &mut std::collections::BTreeSet<String>, depth: usize) {
    if depth > 24 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            collect_modaliases(&e.path(), out, depth + 1);
        } else if e.file_name() == "modalias" {
            if let Ok(a) = std::fs::read_to_string(e.path()) {
                let a = a.trim();
                if !a.is_empty() {
                    out.insert(a.to_string());
                }
            }
        }
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// An lsmod-style listing of `modules` for localmodconfig's LSMOD.
pub fn lsmod_text(modules: &[String]) -> String {
    let mut s = String::from("Module                  Size  Used by\n");
    for m in modules {
        s.push_str(&format!("{m} 0 0\n"));
    }
    s
}

/// A kernel release string is used in paths and package names: a plain token.
pub fn valid_release(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
        && !s.contains("..")
}

/// A finished build in ~/Downloads/kernel-pop/linux-<release>/.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltSet {
    pub dir: PathBuf,
    pub kernelrelease: String,
    /// Package file names, from the manifest.
    pub debs: Vec<String>,
    /// The source signature line from build-info.txt.
    pub source: String,
}

pub const MANIFEST: &str = "SHA256SUMS";
const BUILD_INFO: &str = "build-info.txt";

/// "<sha256>  <file>" lines; only plain file names are accepted.
pub fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (hash, name) = line.split_once("  ").with_context(|| format!("Bad manifest line: {line}"))?;
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("Bad hash in manifest line: {line}");
        }
        if name.contains('/') || name.starts_with('.') || !name.ends_with(".deb") {
            bail!("Bad file name in manifest line: {line}");
        }
        m.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    if m.is_empty() {
        bail!("The manifest lists no packages");
    }
    Ok(m)
}

pub fn manifest_text(entries: &BTreeMap<String, String>) -> String {
    entries.iter().map(|(n, h)| format!("{h}  {n}\n")).collect()
}

fn deb_package(deb: &Path) -> Result<String> {
    let out = Command::new("dpkg-deb").arg("-f").arg(deb).arg("Package").output().context("Could not run dpkg-deb")?;
    if !out.status.success() {
        bail!("{} is not a readable package", deb.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_err() {
        // Different file systems: copy, then remove the original.
        std::fs::copy(from, to).with_context(|| format!("Could not copy {} to {}", from.display(), to.display()))?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

fn set_mode_sync(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).with_context(|| format!("Could not set the mode of {}", p.display()))
}

/// Build the prepared tree into packages and publish them. On a cancel the
/// tree and every package written so far are removed; on a failure the tree
/// (and its build.log) stays until the next build starts.
pub fn build(p: Prepared, events: EventSink, control: Arc<BuildControl>) -> Result<BuiltSet> {
    let file = std::fs::OpenOptions::new().append(true).open(&p.log).with_context(|| format!("Could not open {}", p.log.display()))?;
    let runner = Runner { events, log: Mutex::new(file), control };
    let root = p.tree.parent().context("build tree has no parent")?.to_path_buf();
    let result = run_build(&p, &runner, &root);
    match &result {
        Err(e) if is_build_cancelled(e) => {
            let _ = std::fs::remove_dir_all(&p.tree);
            remove_package_outputs(&root);
        }
        Err(e) => {
            runner.note(format!("ERROR: {e:#}"));
            remove_package_outputs(&root);
        }
        Ok(_) => {}
    }
    result
}

/// What dpkg-buildpackage writes next to the tree.
fn remove_package_outputs(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if [".deb", ".buildinfo", ".changes"].iter().any(|s| name.ends_with(s)) && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn run_build(p: &Prepared, r: &Runner, root: &Path) -> Result<BuiltSet> {
    let krel = &p.summary.kernelrelease;
    r.stage(Stage::Build);
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut c = make(&p.tree, p.summary.ccache);
    c.arg(format!("-j{jobs}"))
        .arg("bindeb-pkg")
        .arg(format!("KDEB_PKGVERSION={}", p.request.tarball.version.deb_version()))
        .env("DEB_BUILD_PROFILES", NO_DBG_PROFILE)
        .env_remove("KDEB_SOURCENAME")
        .env_remove("KDEB_COMPRESS")
        .env_remove("DPKG_FLAGS")
        .env_remove("DEB_BUILD_OPTIONS");
    r.note(format!("make -j{jobs} bindeb-pkg KDEB_PKGVERSION={}", p.request.tarball.version.deb_version()));
    let packaging = AtomicBool::new(false);
    let on_line = |l: &str| {
        if !packaging.load(Ordering::Relaxed) && is_packaging_line(l) {
            packaging.store(true, Ordering::Relaxed);
            (r.events)(BuildEvent::Stage(Stage::Package));
        }
    };
    let (ok, _) = r.run_with(c, "make bindeb-pkg", false, &on_line)?;
    if !ok {
        bail!("make bindeb-pkg failed — see the build log");
    }
    if !packaging.load(Ordering::Relaxed) {
        (r.events)(BuildEvent::Stage(Stage::Package));
    }

    // Pick the image and headers packages by their Package field.
    let (image_name, headers_name) = (format!("linux-image-{krel}"), format!("linux-headers-{krel}"));
    let mut keep: Vec<(PathBuf, String)> = vec![];
    for e in std::fs::read_dir(root)?.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("deb") {
            continue;
        }
        let pkg = deb_package(&path)?;
        if pkg == image_name || pkg == headers_name {
            keep.push((path, pkg));
        } else {
            r.note(format!("Not keeping {} ({pkg}): Kernel Pop installs only the image and headers", path.display()));
        }
    }
    if !keep.iter().any(|(_, pkg)| *pkg == image_name) {
        bail!("bindeb-pkg produced no {image_name} package");
    }
    if !keep.iter().any(|(_, pkg)| *pkg == headers_name) {
        r.note(format!("WARNING: no {headers_name} package (CONFIG_MODULES off?) — DKMS modules cannot be built for this kernel"));
    }
    let names: Vec<String> = keep.iter().filter_map(|(p, _)| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();

    // Publish: everything goes into a temp dir next to the final one, which
    // is renamed into place last, so the install step only ever sees a
    // complete set with its manifest. On any error the temp dir goes again
    // and an earlier build of the same kernel stays as it was.
    let downloads = downloads_root();
    std::fs::create_dir_all(&downloads)?;
    set_mode_sync(&downloads, crate::download::DIR_MODE)?;
    let out = downloads.join(format!("linux-{krel}"));
    let tmp = downloads.join(format!(".linux-{krel}.new"));
    let published = publish(p, r, &keep, &names, &tmp, &out);
    if published.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    published?;
    r.note(format!("Packages: {} — in {}", names.join(", "), out.display()));
    std::fs::remove_dir_all(&p.tree).with_context(|| format!("Could not delete the build tree {}", p.tree.display()))?;
    remove_package_outputs(root);
    Ok(BuiltSet { dir: out, kernelrelease: krel.clone(), debs: names, source: p.request.trust.describe() })
}

/// Fill `tmp` with the packages, config, build-info, build log and manifest,
/// then swap it in as `out`.
fn publish(p: &Prepared, r: &Runner, keep: &[(PathBuf, String)], names: &[String], tmp: &Path, out: &Path) -> Result<()> {
    let krel = &p.summary.kernelrelease;
    if tmp.exists() {
        std::fs::remove_dir_all(tmp)?;
    }
    std::fs::create_dir(tmp)?;
    set_mode_sync(tmp, crate::download::DIR_MODE)?;
    let mut manifest = BTreeMap::new();
    for ((path, _), name) in keep.iter().zip(names) {
        let dest = tmp.join(name);
        move_file(path, &dest)?;
        set_mode_sync(&dest, crate::download::FILE_MODE)?;
        manifest.insert(name.clone(), crate::download::sha256_file(&dest)?);
    }
    let cfg = tmp.join(format!("config-{krel}"));
    std::fs::copy(p.tree.join(".config"), &cfg)?;
    let info = format!(
        "kernel: {krel}\nsource: {}\nsignature: {}\nconfiguration: {}\nbase config: {}\nbuilt: {}\n",
        p.request.tarball.origin.display(),
        p.request.trust.describe(),
        p.summary.mode.label(),
        p.summary.base_config.display(),
        glib::DateTime::now_local().ok().and_then(|d| d.format("%Y-%m-%d %H:%M").ok()).map(|s| s.to_string()).unwrap_or_default(),
    );
    std::fs::write(tmp.join(BUILD_INFO), info)?;
    std::fs::write(tmp.join(MANIFEST), manifest_text(&manifest))?;
    r.note(format!("Publishing to {}", out.display()));
    // A copy: the log in the tree stays complete if anything below fails.
    std::fs::copy(&p.log, tmp.join("build.log"))?;
    for f in [cfg.clone(), tmp.join(BUILD_INFO), tmp.join(MANIFEST), tmp.join("build.log")] {
        set_mode_sync(&f, crate::download::FILE_MODE)?;
    }
    // Swap: the earlier set is moved aside first and only deleted once the
    // new one is in place; if the rename fails it is put back.
    let old = out.with_file_name(format!(".linux-{krel}.old"));
    if old.exists() {
        std::fs::remove_dir_all(&old)?;
    }
    let had_old = out.exists();
    if had_old {
        std::fs::rename(out, &old).with_context(|| format!("Could not move the earlier build in {} aside", out.display()))?;
    }
    if let Err(e) = std::fs::rename(tmp, out) {
        if had_old {
            let _ = std::fs::rename(&old, out);
        }
        return Err(e).with_context(|| format!("Could not move the new build into {}", out.display()));
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// The first output line of bindeb-pkg's packaging step.
fn is_packaging_line(l: &str) -> bool {
    l.starts_with("dpkg-deb: building package") || l.trim_start().starts_with("INSTALL debian/")
}

/// The finished builds in ~/Downloads/kernel-pop, newest first.
pub fn built_sets() -> Vec<BuiltSet> {
    built_sets_in(&downloads_root())
}

pub fn built_sets_in(root: &Path) -> Vec<BuiltSet> {
    let Ok(rd) = std::fs::read_dir(root) else { return vec![] };
    let mut sets: Vec<(std::time::SystemTime, BuiltSet)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let kernelrelease = name.strip_prefix("linux-")?.to_string();
            let dir = e.path();
            let manifest = std::fs::read_to_string(dir.join(MANIFEST)).ok()?;
            let debs = parse_manifest(&manifest).ok()?.into_keys().collect();
            let source = std::fs::read_to_string(dir.join(BUILD_INFO))
                .ok()
                .and_then(|t| t.lines().find_map(|l| l.strip_prefix("signature: ").map(|s| s.to_string())))
                .unwrap_or_default();
            let mtime = std::fs::metadata(dir.join(MANIFEST)).and_then(|m| m.modified()).ok()?;
            valid_release(&kernelrelease).then_some((mtime, BuiltSet { dir, kernelrelease, debs, source }))
        })
        .collect();
    sets.sort_by_key(|s| std::cmp::Reverse(s.0));
    sets.into_iter().map(|(_, s)| s).collect()
}

/// Check every package of a set against its manifest.
pub fn verify_built_set(set: &BuiltSet) -> Result<()> {
    let manifest = parse_manifest(&std::fs::read_to_string(set.dir.join(MANIFEST))?)?;
    for (name, want) in &manifest {
        let got = crate::download::sha256_file(&set.dir.join(name))?;
        if &got != want {
            bail!("{name} does not match the manifest (expected {want}, got {got})");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpkg_query_status_is_read_per_package() {
        let mut out = String::new();
        for p in BUILD_DEPS {
            let st = match *p {
                "pahole" => "un ",
                "zstd" => "rc ",
                _ => "ii ",
            };
            if *p != "cpio" {
                out.push_str(&format!("{p} {st}\n"));
            }
        }
        assert_eq!(missing_from_dpkg_query(&out), vec!["pahole", "zstd", "cpio"]);
        // Multi-arch names ("libssl-dev:amd64") count as the package.
        let all: String = BUILD_DEPS.iter().map(|p| format!("{p}:amd64 ii \n")).collect();
        assert!(missing_from_dpkg_query(&all).is_empty());
        assert_eq!(missing_from_dpkg_query("").len(), BUILD_DEPS.len());
    }

    #[test]
    fn build_deps_match_the_privileged_allowlist() {
        let script = include_str!("../scripts/privileged-install.sh");
        let start = script.find("BUILD_DEPS_ALLOWED=(").expect("allowlist in the script") + "BUILD_DEPS_ALLOWED=(".len();
        let list: Vec<&str> = script[start..start + script[start..].find(')').unwrap()].split_whitespace().collect();
        assert_eq!(list, BUILD_DEPS);
    }

    #[test]
    fn module_lists_for_localmodconfig() {
        let proc = "nvidia_drm 167936 88 - Live 0x0000000000000000 (POE)\nsnd-hda-intel 69632 3 - Live 0x0\n";
        assert_eq!(parse_proc_modules(proc), ["nvidia_drm", "snd_hda_intel"]);
        let text = lsmod_text(&["btusb".to_string(), "usb_storage".to_string()]);
        assert_eq!(text, "Module                  Size  Used by\nbtusb 0 0\nusb_storage 0 0\n");
        assert_eq!(shell_quote("pci:v0000'x"), "'pci:v0000'\\''x'");
        // Every extra is a plain module name, listed once.
        let mut seen = std::collections::HashSet::new();
        for m in TUNED_EXTRA_MODULES {
            assert!(m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "{m}");
            assert!(seen.insert(*m), "{m} listed twice");
        }
    }

    #[test]
    fn config_values() {
        let c = "CONFIG_RUST=y\nCONFIG_LOCALVERSION=\"-kernelpop\"\n# CONFIG_DEBUG_INFO_NONE is not set\nCONFIG_E1000=m\nCONFIG_EXT4_FS=y\nCONFIG_SND=m\n";
        assert_eq!(config_value(c, "RUST").as_deref(), Some("y"));
        assert_eq!(config_value(c, "LOCALVERSION").as_deref(), Some("\"-kernelpop\""));
        assert_eq!(config_value(c, "DEBUG_INFO_NONE"), None);
        assert_eq!(config_value(c, "RUS"), None);
        assert_eq!(module_count(c), 2);
    }

    #[test]
    fn releases_and_manifests() {
        assert!(valid_release("7.2.12-kernelpop"));
        assert!(valid_release("7.4.0-rc1-kernelpop"));
        for bad in ["", "../x", "a/b", "-x", "a b", "7..2", "$(id)"] {
            assert!(!valid_release(bad), "{bad}");
        }
        let h = "a".repeat(64);
        let m = parse_manifest(&format!(
            "{h}  linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb\n{}  linux-headers-7.2.12-kernelpop_7.2.12-1_amd64.deb\n",
            "B".repeat(64)
        ))
        .unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["linux-headers-7.2.12-kernelpop_7.2.12-1_amd64.deb"], "b".repeat(64));
        assert_eq!(parse_manifest(&manifest_text(&m)).unwrap(), m);
        for bad in [format!("{h}  ../x.deb"), format!("{h}  x.txt"), format!("{h} x.deb"), "abc  x.deb".into(), String::new()] {
            assert!(parse_manifest(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn summary_lists_everything_shown_before_a_build() {
        let s = ConfigSummary {
            base_config: "/boot/config-7.2.6-070206-generic".into(),
            mode: ConfigMode::MatchSystem,
            localversion: LOCALVERSION.into(),
            kernelrelease: "7.2.12-kernelpop".into(),
            fixes: vec!["LOCALVERSION set".into()],
            notes: vec!["CONFIG_RUST turned off".into()],
            modules: 6000,
            ccache: true,
        };
        let t = s.text();
        for want in [
            "7.2.12-kernelpop",
            "Match this system",
            "/boot/config-7.2.6-070206-generic",
            "-kernelpop",
            "LOCALVERSION set",
            "CONFIG_RUST turned off",
            "ccache: used",
        ] {
            assert!(t.contains(want), "{want} missing from {t}");
        }
    }

    #[test]
    fn space_preflight() {
        let gb = 1024 * 1024 * 1024u64;
        assert!(space_problem(&ConfigMode::MatchSystem, Some(100 * gb), Some(gb), 200 << 20).is_none());
        assert!(space_problem(&ConfigMode::MatchSystem, Some(8 * gb), Some(gb), 200 << 20).unwrap().contains("free in"));
        assert!(space_problem(&ConfigMode::TunedForMachine, Some(10 * gb), Some(gb), 200 << 20).is_none());
        assert!(space_problem(&ConfigMode::MatchSystem, Some(100 * gb), Some(100 << 20), 200 << 20).unwrap().contains("/boot"));
        // Unknown free space does not block (df failed): the build itself reports a full disk.
        assert!(space_problem(&ConfigMode::MatchSystem, None, None, 200 << 20).is_none());
    }

    #[test]
    fn packaging_is_recognised() {
        assert!(is_packaging_line("dpkg-deb: building package 'linux-image-7.2.12-kernelpop' in '../x.deb'."));
        assert!(is_packaging_line("  INSTALL debian/linux-image-7.2.12-kernelpop/lib/modules/7.2.12-kernelpop/kernel/fs/x.ko"));
        assert!(!is_packaging_line("  CC [M]  fs/omfs/dir.o"));
    }

    #[test]
    fn built_sets_are_found_by_their_manifest() {
        let d = crate::source::tests::scratch("built");
        let set = d.join("linux-7.2.12-kernelpop");
        std::fs::create_dir_all(&set).unwrap();
        std::fs::write(set.join("linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb"), b"image").unwrap();
        let mut m = BTreeMap::new();
        m.insert(
            "linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb".to_string(),
            crate::download::sha256_file(&set.join("linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb")).unwrap(),
        );
        std::fs::write(set.join(MANIFEST), manifest_text(&m)).unwrap();
        std::fs::write(set.join(BUILD_INFO), "kernel: 7.2.12-kernelpop\nsignature: verified: signed by X\n").unwrap();
        // A half-published set (no manifest) and a stray file are not sets.
        std::fs::create_dir_all(d.join(".linux-7.2.13-kernelpop.new")).unwrap();
        std::fs::create_dir_all(d.join("linux-7.2.11-kernelpop")).unwrap();
        std::fs::write(d.join("linux-7.2.12.tar.xz"), b"x").unwrap();
        let sets = built_sets_in(&d);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].kernelrelease, "7.2.12-kernelpop");
        assert_eq!(sets[0].source, "verified: signed by X");
        verify_built_set(&sets[0]).unwrap();
        std::fs::write(set.join("linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb"), b"changed").unwrap();
        assert!(verify_built_set(&sets[0]).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn publishing_replaces_an_earlier_set_whole() {
        let d = crate::source::tests::scratch("publish");
        let tree = d.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".config"), "CONFIG_X=y\n").unwrap();
        let log = tree.join("build.log");
        std::fs::write(&log, "log\n").unwrap();
        let tb = d.join("linux-7.2.12.tar.xz");
        crate::source::tests::compress_to("xz", &crate::source::tests::tar_bytes("linux-7.2.12", crate::source::tests::MAKEFILE), &tb);
        let tarball = crate::source::inspect(&tb).unwrap();
        let summary = ConfigSummary {
            base_config: "/boot/config-x".into(),
            mode: ConfigMode::MatchSystem,
            localversion: LOCALVERSION.into(),
            kernelrelease: "7.2.12-kernelpop".into(),
            fixes: vec![],
            notes: vec![],
            modules: 1,
            ccache: false,
        };
        let request =
            BuildRequest { tarball, trust: SourceTrust::Unverified { reason: "test".into() }, mode: ConfigMode::MatchSystem, running_kernel: "x".into() };
        let p = Prepared { request, tree: tree.clone(), summary, log: log.clone() };
        let runner = Runner { events: Arc::new(|_| {}), log: Mutex::new(std::fs::File::create(d.join("runner.log")).unwrap()), control: BuildControl::new() };
        let out = d.join("linux-7.2.12-kernelpop");
        let tmp = d.join(".linux-7.2.12-kernelpop.new");
        for round in ["first", "second"] {
            let deb = d.join("linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb");
            std::fs::write(&deb, round).unwrap();
            let keep = vec![(deb, "linux-image-7.2.12-kernelpop".to_string())];
            let names = vec!["linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb".to_string()];
            publish(&p, &runner, &keep, &names, &tmp, &out).unwrap();
            assert_eq!(std::fs::read_to_string(out.join(&names[0])).unwrap(), round);
            let set = built_sets_in(&d);
            assert_eq!(set.len(), 1);
            verify_built_set(&set[0]).unwrap();
            assert!(set[0].source.starts_with("UNVERIFIED"));
            assert!(out.join("build.log").exists() && out.join("config-7.2.12-kernelpop").exists());
            // The tree's own log stays where it was.
            assert!(log.exists());
        }
        assert!(!tmp.exists());
        assert!(!d.join(".linux-7.2.12-kernelpop.old").exists());
        // A failure part way (a package that is not there) leaves the
        // earlier set untouched; the caller removes the temp dir.
        let keep = vec![(d.join("missing.deb"), "x".to_string())];
        assert!(publish(&p, &runner, &keep, &["missing.deb".to_string()], &tmp, &out).is_err());
        assert_eq!(std::fs::read_to_string(out.join("linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb")).unwrap(), "second");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn dir_sizes_add_up() {
        let d = crate::source::tests::scratch("dirsize");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/x"), [0u8; 100]).unwrap();
        std::fs::write(d.join("a/b/y"), [0u8; 23]).unwrap();
        std::os::unix::fs::symlink("/usr", d.join("a/link")).unwrap();
        assert_eq!(dir_size(&d.join("a")), 123 + "/usr".len() as u64);
        assert_eq!(dir_size(&d.join("nope")), 0);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn cancel_kills_the_whole_process_group() {
        let d = crate::source::tests::scratch("cancel");
        let control = BuildControl::new();
        let lines = Arc::new(Mutex::new(vec![]));
        let l2 = lines.clone();
        let events: EventSink = Arc::new(move |e| {
            if let BuildEvent::Line(l) = e {
                l2.lock().unwrap().push(l);
            }
        });
        let runner = Runner { events, log: Mutex::new(std::fs::File::create(d.join("log")).unwrap()), control: control.clone() };
        let c2 = control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            c2.cancel();
        });
        // A parent and a grandchild that would both outlive a plain kill.
        let mut cmd = Command::new("bash");
        cmd.args(["-c", &format!("echo started; (sleep 60; touch {}/survived) & sleep 60", d.display())]);
        let t = std::time::Instant::now();
        let err = runner.run(cmd, "sleeper").unwrap_err();
        assert!(is_build_cancelled(&err), "{err}");
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(lines.lock().unwrap().first().map(String::as_str), Some("started"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!d.join("survived").exists());
        // Nothing new starts after a cancel.
        assert!(is_build_cancelled(&runner.run(Command::new("true"), "true").unwrap_err()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn output_is_logged_and_failures_reported() {
        let d = crate::source::tests::scratch("runner");
        let events: EventSink = Arc::new(|_| {});
        let runner = Runner { events, log: Mutex::new(std::fs::File::create(d.join("log")).unwrap()), control: BuildControl::new() };
        let mut ok = Command::new("bash");
        ok.args(["-c", "echo out; echo err >&2"]);
        let lines = runner.run(ok, "echo").unwrap();
        assert_eq!(lines, ["out", "err"]);
        let mut fail = Command::new("bash");
        fail.args(["-c", "exit 3"]);
        assert!(runner.run(fail, "failing step").unwrap_err().to_string().contains("failing step failed"));
        let log = std::fs::read_to_string(d.join("log")).unwrap();
        assert!(log.contains("out\nerr\n"), "{log}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Extract and configure only, with the source marked unverified: the
    /// build log must say so. KERNEL_POP_LIVE_TARBALL=<tarball>.
    #[test]
    #[ignore]
    fn live_prepare_unverified_is_logged() {
        let path = PathBuf::from(std::env::var("KERNEL_POP_LIVE_TARBALL").expect("set KERNEL_POP_LIVE_TARBALL"));
        let tarball = crate::source::inspect(&path).unwrap();
        let running = String::from_utf8(Command::new("uname").arg("-r").output().unwrap().stdout).unwrap().trim().to_string();
        let trust = SourceTrust::Unverified { reason: "test: no signature".into() };
        let req = BuildRequest { tarball, trust, mode: ConfigMode::MatchSystem, running_kernel: running };
        let events: EventSink = Arc::new(|_| {});
        let p = prepare(req, events, BuildControl::new(), 300 << 20).unwrap();
        println!("{}", p.summary.text());
        let log = std::fs::read_to_string(&p.log).unwrap();
        assert!(log.contains("Source signature: UNVERIFIED"), "{log}");
        assert!(log.contains("was NOT verified"), "{log}");
        let config = std::fs::read_to_string(p.tree.join(".config")).unwrap();
        assert_eq!(config_value(&config, "LOCALVERSION").as_deref(), Some("\"-kernelpop\""));
        assert_eq!(config_value(&config, "SYSTEM_TRUSTED_KEYS").as_deref(), Some("\"\""));
        let tree = p.tree.clone();
        p.discard();
        assert!(!tree.exists());
    }

    /// A real build of a real kernel.org tarball, like the Build page does
    /// it: KERNEL_POP_LIVE_TARBALL=<tarball> KERNEL_POP_LIVE_MODE=match|tuned,
    /// optionally KERNEL_POP_LIVE_UNVERIFIED=1 to skip the signature check.
    /// Takes the better part of an hour. Run with --ignored --nocapture.
    #[test]
    #[ignore]
    fn live_build() {
        let path = PathBuf::from(std::env::var("KERNEL_POP_LIVE_TARBALL").expect("set KERNEL_POP_LIVE_TARBALL"));
        let mode = match std::env::var("KERNEL_POP_LIVE_MODE").as_deref() {
            Ok("tuned") => ConfigMode::TunedForMachine,
            _ => ConfigMode::MatchSystem,
        };
        let tarball = crate::source::inspect(&path).unwrap();
        let trust = if std::env::var_os("KERNEL_POP_LIVE_UNVERIFIED").is_some() {
            SourceTrust::Unverified { reason: "live test override".into() }
        } else {
            let sig = tarball.sibling_signature().expect("a .tar.sign next to the tarball");
            SourceTrust::Verified(crate::verify::verify_tarball(&tarball, &sig, &|l| println!("{l}")).unwrap())
        };
        let running = String::from_utf8(Command::new("uname").arg("-r").output().unwrap().stdout).unwrap().trim().to_string();
        let req = BuildRequest { tarball, trust, mode, running_kernel: running };
        let start = std::time::Instant::now();
        let events: EventSink = Arc::new(move |e| match e {
            BuildEvent::Stage(s) => println!("[{:>6}s] STAGE {}", start.elapsed().as_secs(), s.label()),
            BuildEvent::Line(l) if l.starts_with("====") || l.contains("ERROR") || l.contains("WARNING") || l.contains("error:") => println!("{l}"),
            BuildEvent::Line(_) => {}
        });
        let control = BuildControl::new();
        let prepared = prepare(req, events.clone(), control.clone(), 300 << 20).unwrap();
        println!("{}", prepared.summary.text());
        println!("prepared in {}s", start.elapsed().as_secs());
        let set = build(prepared, events, control).unwrap();
        println!("built {:?} in {}s", set, start.elapsed().as_secs());
        verify_built_set(&set).unwrap();
    }
}
