use crate::build::{
    self, built_sets, ccache_available, free_bytes, is_build_cancelled, missing_build_deps, space_problem, verify_built_set, BuildControl, BuildEvent,
    BuildRequest, BuiltSet, ConfigMode, EventSink, Prepared, SourceTrust, Stage,
};
use crate::download::{download_files, is_cancelled, Progress, Remote};
use crate::install::{
    helper_problem, run_privileged_boot_once, run_privileged_install, run_privileged_install_build_deps, run_privileged_remove, run_privileged_remove_many,
    run_privileged_setup_signing, run_privileged_sign, validate_mok_password, BootOutlook, ScriptReport,
};
use crate::kernelorg::{fetch_releases, fetch_signature, url_file_name, Moniker, Release};
use crate::source::{inspect, private_copy, Tarball};
use crate::system::{
    compare_to_running, format_bytes, health_summary, install_blocker, install_warnings, prune_candidates, query_system, BootEntry, BootPath, KernelSignature,
    KernelSigning, KeyEnrollment, SecureBoot, SystemInfo, VersionRelation, MIN_ROOT_BYTES,
};
use crate::verify::{verify_tarball, Signer};

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, Label, ListBox, Orientation, PasswordEntry, ProgressBar, ScrolledWindow, SelectionMode, SpinButton, Spinner,
    Stack, TextView, WrapMode,
};
use libadwaita::prelude::*;
use libadwaita::{AboutDialog, ActionRow, AlertDialog, Application, ApplicationWindow, Banner, HeaderBar, PreferencesGroup, SwitchRow, Toast, ToastOverlay};

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
//  App state
// ─────────────────────────────────────────────────────────────────────────────

/// Late-bound handle to load_sysinfo, for callbacks created inside it.
type ReloadSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Starts a download, build or privileged action under the global busy
/// state, or returns None (with a toast) when another one is still running.
/// The busy state lasts as long as the returned guard: move it into the
/// action's completion callback, so it is released even when that callback
/// never runs (a panicking task drops the callback, and with it the guard).
type BeginBusy = Rc<dyn Fn(&'static str) -> Option<BusyGuard>>;

struct BusyGuard(Rc<dyn Fn()>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Where the selected source's signature check stands.
#[derive(Clone, Default)]
enum SigState {
    #[default]
    None,
    Checking,
    Verified(Signer),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Default)]
enum ModeChoice {
    #[default]
    Match,
    Tuned,
    Own,
}

#[derive(Default)]
struct AppState {
    releases: Vec<Release>,
    selected_release: Option<Release>,
    /// The tarball on the Build page.
    source: Option<Tarball>,
    signature: SigState,
    /// "Build unverified source", given for this one tarball only.
    allow_unverified: bool,
    mode: ModeChoice,
    own_config: Option<PathBuf>,
    missing_deps: Vec<String>,
    /// Finished builds in ~/Downloads/kernel-pop, newest first.
    built: Vec<BuiltSet>,
    /// The build selected on the Install tab, and whether its packages
    /// matched its manifest.
    staged: Option<BuiltSet>,
    staged_verified: bool,
    sysinfo: SystemInfo,
    download_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    build_control: Option<Arc<BuildControl>>,
    build_started: Option<std::time::Instant>,
    /// What is running right now, if anything: one download, build or
    /// privileged action at a time. While set, every control that would
    /// start another one is insensitive (see refresh_controls).
    busy: Option<&'static str>,
}

impl AppState {
    fn config_mode(&self) -> Option<ConfigMode> {
        match self.mode {
            ModeChoice::Match => Some(ConfigMode::MatchSystem),
            ModeChoice::Tuned => Some(ConfigMode::TunedForMachine),
            ModeChoice::Own => self.own_config.clone().map(ConfigMode::OwnConfig),
        }
    }

    /// The trust a build of the current source would carry, or None while
    /// it may not be built.
    fn source_trust(&self) -> Option<SourceTrust> {
        match &self.signature {
            SigState::Verified(s) => Some(SourceTrust::Verified(s.clone())),
            SigState::Failed(reason) if self.allow_unverified => Some(SourceTrust::Unverified { reason: reason.clone() }),
            _ => None,
        }
    }

    fn can_build(&self) -> bool {
        self.source.is_some() && self.source_trust().is_some() && self.missing_deps.is_empty() && self.config_mode().is_some()
    }
}

/// Messages from worker threads to the GTK main loop: log lines (batched
/// into the Log tab) and build stages.
enum UiMsg {
    Log(String),
    Stage(Stage),
}

/// The Log tab keeps this many lines; the full build output is in build.log.
const LOG_MAX_LINES: i32 = 6000;

// ─────────────────────────────────────────────────────────────────────────────
//  Async bridge: run future on Tokio, deliver result to GTK main thread
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_async<F, T, CB>(future: F, callback: CB)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
    CB: FnOnce(T) + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<T>();
    crate::runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    glib::MainContext::default().spawn_local(async move {
        if let Ok(val) = rx.await {
            callback(val);
        }
    });
}

/// Show what the privileged script printed, line by line, in the Log tab.
fn log_output(log_fn: &impl Fn(String), output: &str) {
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        log_fn(line.to_string());
    }
}

/// Secure Boot row subtitle, taking the signing state into account.
fn secure_boot_subtitle(sb: SecureBoot, signing: KernelSigning) -> &'static str {
    match (sb, signing) {
        (SecureBoot::Enabled, KernelSigning::Enrolled) => "Enabled — kernels are signed with your enrolled key, so they boot",
        (SecureBoot::Enabled, KernelSigning::EnrollmentPending) => {
            "Enabled — finish enrolling the signing key at the next reboot, or newly signed kernels will not boot"
        }
        (SecureBoot::Enabled, KernelSigning::KeyNotEnrolled | KernelSigning::Unknown) => "Enabled — signed kernels boot only once the signing key is enrolled",
        (SecureBoot::Enabled, KernelSigning::EfiVarsNotMounted) => {
            "Enabled — the EFI variables are not mounted, so the signing key's enrollment cannot be checked"
        }
        (SecureBoot::Enabled, KernelSigning::NoKey) => "Enabled — unsigned kernels built from source will not boot while it is on",
        (SecureBoot::Disabled, KernelSigning::NoKey) => "Off — unsigned kernels can boot",
        (SecureBoot::Disabled, _) => "Off — every kernel can boot; kernels are still signed as they are installed",
        (SecureBoot::Unknown, _) => "Could not determine (mokutil missing or EFI variables unreadable)",
    }
}

/// Boot path row subtitle; None hides the row.
fn boot_path_subtitle(path: &BootPath) -> Option<String> {
    match path {
        BootPath::Shim => Some("shim starts the boot loader, so Secure Boot trusts it through your enrolled key".to_string()),
        BootPath::DirectSystemdBoot => Some(
            "The firmware starts systemd-boot without shim: with Secure Boot on it refuses it and the machine does not boot, and the signing key cannot be enrolled. Set Up Signing puts shim in front of it".to_string(),
        ),
        BootPath::Other(name) => Some(format!("The firmware starts {name} without shim, so Secure Boot cannot use your enrolled key")),
        BootPath::Unknown => None,
    }
}

fn signing_subtitle(signing: KernelSigning) -> &'static str {
    match signing {
        KernelSigning::NoKey => "Not set up — kernels are installed unsigned",
        KernelSigning::KeyNotEnrolled =>
            "Key created but not enrolled — set up again to queue the enrollment",
        KernelSigning::EnrollmentPending =>
            "Enrollment pending — confirm it at the blue MOK Manager screen on the next reboot",
        KernelSigning::Enrolled => "Set up — every kernel Kernel Pop installs is signed with an enrolled key",
        KernelSigning::EfiVarsNotMounted =>
            "Key present; the EFI variables are not mounted, so its enrollment cannot be read. Mount them (sudo mount -t efivarfs efivarfs /sys/firmware/efi/efivars) or reboot",
        KernelSigning::Unknown =>
            "Key present; could not read its enrollment state (mokutil missing?)",
    }
}

/// What a privileged run that can report DKMS/signing/boot problems did.
#[derive(Clone, Copy)]
enum Done {
    Install,
    Sign,
    SetupSigning,
}

/// The toast for a finished run, from what the script reported: the
/// problems, or else a reminder to confirm a pending key enrollment.
fn outcome_message(done: Done, r: &ScriptReport) -> String {
    let (ok, subject) = match done {
        Done::Install => ("Kernel installed — reboot to activate", "Kernel installed"),
        Done::Sign => ("Kernel signed", "Kernel signed"),
        Done::SetupSigning => ("Signing set up — confirm the key at the next reboot", "Signing set up"),
    };
    let mut problems = vec![];
    if r.dkms_problems > 0 {
        problems.push("DKMS modules failed");
    }
    if r.sign_problems > 0 {
        problems.push("signing had problems");
    }
    if !problems.is_empty() {
        return format!("{subject}, but {} — see Log tab before rebooting", problems.join(" and "));
    }
    if r.boot == BootOutlook::AfterEnrollment {
        return format!("{subject} — confirm the key enrollment at the next reboot, or it will not boot");
    }
    ok.to_string()
}

/// Log the script's output, a summary of the problems it reported, and
/// return the toast to show.
fn report_run(log_fn: &impl Fn(String), out: &str, done: Done) -> String {
    log_output(log_fn, out);
    let r = ScriptReport::parse(out);
    if r.dkms_problems > 0 {
        log_fn(format!(
            "WARNING: {} DKMS problem(s) — out-of-tree modules such as the NVIDIA driver may be missing after reboot. See the DKMS lines above.",
            r.dkms_problems
        ));
    }
    if r.sign_problems > 0 {
        log_fn(format!("WARNING: {} signing problem(s) — a kernel or its boot loader copy may be unsigned. See the SIGN lines above.", r.sign_problems));
    }
    outcome_message(done, &r)
}

// ─────────────────────────────────────────────────────────────────────────────
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_ui(app: &Application) {
    let state = Rc::new(RefCell::new(AppState::default()));

    let window = ApplicationWindow::builder().application(app).title("Kernel Pop").default_width(860).default_height(820).build();

    let toast_overlay = ToastOverlay::new();

    // ── Header ───────────────────────────────────────────────────────────────
    let header = HeaderBar::new();

    let refresh_btn = Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh kernel list").build();
    header.pack_start(&refresh_btn);

    let about_btn = Button::builder().icon_name("help-about-symbolic").tooltip_text("About").build();
    header.pack_end(&about_btn);

    // ── Stack ────────────────────────────────────────────────────────────────
    let stack = Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
    let switcher = gtk4::StackSwitcher::builder().stack(&stack).build();
    header.set_title_widget(Some(&switcher));

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 1: System — installed kernels and their boot health
    // ═════════════════════════════════════════════════════════════════════════
    let sysinfo_page = GtkBox::new(Orientation::Vertical, 12);
    sysinfo_page.set_margin_top(12);
    sysinfo_page.set_margin_bottom(12);
    sysinfo_page.set_margin_start(12);
    sysinfo_page.set_margin_end(12);

    // "Newer kernel available" banner — populated by a background kernel.org
    // fetch after the (local, fast) system query renders; see load_sysinfo.
    let update_banner = Banner::new("");
    update_banner.set_revealed(false);
    sysinfo_page.append(&update_banner);

    let running_group = PreferencesGroup::builder().title("Running Kernel").build();
    let running_row = ActionRow::builder().title("Detecting…").build();
    running_group.add(&running_row);
    sysinfo_page.append(&running_group);

    let kernels_group = PreferencesGroup::builder()
        .title("Installed Kernels")
        .description("Every kernel in /boot, checked for a matching initramfs and modules directory")
        .build();
    let kernels_list = ListBox::new();
    kernels_list.set_selection_mode(SelectionMode::None);
    kernels_list.add_css_class("boxed-list");
    kernels_group.add(&kernels_list);
    sysinfo_page.append(&kernels_group);

    let safety_group = PreferencesGroup::builder().title("Boot Safety").build();
    let secure_boot_row = ActionRow::builder().title("Secure Boot").subtitle("Checking…").build();
    safety_group.add(&secure_boot_row);
    let boot_path_row = ActionRow::builder().title("Boot path").visible(false).build();
    safety_group.add(&boot_path_row);
    let signing_row = ActionRow::builder().title("Kernel signing").subtitle("Checking…").build();
    let setup_signing_btn = Button::builder()
        .label("Set Up Signing")
        .tooltip_text(
            "Create a signing key, put shim in front of systemd-boot where it is missing, sign the installed kernels and queue the key for enrollment",
        )
        .valign(Align::Center)
        .visible(false)
        .build();
    signing_row.add_suffix(&setup_signing_btn);
    safety_group.add(&signing_row);
    sysinfo_page.append(&safety_group);

    let disk_group = PreferencesGroup::builder().title("Disk").build();
    let boot_row = ActionRow::builder().title("/boot Free Space").subtitle("Checking…").build();
    let root_row = ActionRow::builder().title("/ Free Space").subtitle("Checking…").build();
    disk_group.add(&boot_row);
    disk_group.add(&root_row);
    sysinfo_page.append(&disk_group);

    let sys_btn_box = GtkBox::new(Orientation::Horizontal, 8);
    sys_btn_box.set_halign(Align::Center);
    sys_btn_box.set_margin_top(8);
    let refresh_sysinfo_btn = Button::builder().label("Refresh System Info").build();
    refresh_sysinfo_btn.add_css_class("flat");
    let prune_btn = Button::builder().label("Remove Old Kernels…").build();
    prune_btn.add_css_class("flat");
    sys_btn_box.append(&refresh_sysinfo_btn);
    sys_btn_box.append(&prune_btn);
    sysinfo_page.append(&sys_btn_box);

    let sys_scroll = ScrolledWindow::builder().vexpand(true).child(&sysinfo_page).build();
    stack.add_titled(&sys_scroll, Some("system"), "System");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 2: Browse — kernel.org releases
    // ═════════════════════════════════════════════════════════════════════════
    let browse_page = GtkBox::new(Orientation::Vertical, 0);

    let banner = Banner::new("");
    banner.set_revealed(false);
    browse_page.append(&banner);

    let search_bar = gtk4::SearchBar::new();
    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Filter releases…"));
    search_bar.set_child(Some(&search_entry));
    search_bar.set_show_close_button(false);
    search_bar.set_search_mode(true);
    browse_page.append(&search_bar);

    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_top(8);
    list_box.set_margin_bottom(8);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    let scroll = ScrolledWindow::builder().vexpand(true).child(&list_box).build();
    browse_page.append(&scroll);

    let list_spinner = Spinner::new();
    list_spinner.set_halign(Align::Center);
    list_spinner.set_size_request(48, 48);
    browse_page.append(&list_spinner);

    let selected_label = Label::builder().label("No release selected").margin_top(4).margin_bottom(4).build();
    selected_label.add_css_class("dim-label");
    browse_page.append(&selected_label);

    let browse_bar = gtk4::ActionBar::new();
    let download_btn = Button::builder()
        .label("Download Source")
        .tooltip_text("Download the tarball and its signature from cdn.kernel.org to ~/Downloads/kernel-pop")
        .sensitive(false)
        .build();
    download_btn.add_css_class("suggested-action");
    browse_bar.pack_end(&download_btn);
    browse_page.append(&browse_bar);

    stack.add_titled(&browse_page, Some("browse"), "Browse");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 3: Build — source, configuration, build
    // ═════════════════════════════════════════════════════════════════════════
    let build_page = GtkBox::new(Orientation::Vertical, 12);
    build_page.set_margin_top(12);
    build_page.set_margin_bottom(12);
    build_page.set_margin_start(12);
    build_page.set_margin_end(12);

    let source_group = PreferencesGroup::builder()
        .title("Kernel Source")
        .description("Pick a release on the Browse tab, open a tarball, or drop a linux-*.tar.xz or .tar.gz onto this window")
        .build();
    let source_row = ActionRow::builder().title("No source selected").subtitle("Its version is read from the tarball's Makefile").build();
    let open_btn = Button::builder().label("Open Tarball…").valign(Align::Center).build();
    open_btn.add_css_class("flat");
    source_row.add_suffix(&open_btn);
    source_group.add(&source_row);
    let sig_row = ActionRow::builder().title("Signature").subtitle("—").build();
    let recheck_btn = Button::builder().label("Check Again").valign(Align::Center).visible(false).build();
    recheck_btn.add_css_class("flat");
    sig_row.add_suffix(&recheck_btn);
    source_group.add(&sig_row);
    let unverified_row = SwitchRow::builder()
        .title("Build unverified source")
        .subtitle("For your own patched tree only. The build log and the package folder record that the source was not verified.")
        .visible(false)
        .build();
    source_group.add(&unverified_row);
    build_page.append(&source_group);

    let config_group = PreferencesGroup::builder().title("Configuration").build();
    let match_check = CheckButton::new();
    match_check.set_active(true);
    let tuned_check = CheckButton::new();
    tuned_check.set_group(Some(&match_check));
    let own_check = CheckButton::new();
    own_check.set_group(Some(&match_check));
    let match_row = ActionRow::builder()
        .title("Match this system")
        .subtitle("Start from the running kernel's /boot config, so everything that works now keeps working")
        .activatable_widget(&match_check)
        .build();
    match_row.add_prefix(&match_check);
    let tuned_row = ActionRow::builder()
        .title("Tuned for this machine")
        .subtitle("Drivers for the hardware in this computer plus the kinds people plug in (USB storage, controllers, headsets, webcams, docks, VPN and container networking). Builds in a fraction of the time.")
        .activatable_widget(&tuned_check)
        .build();
    tuned_row.add_prefix(&tuned_check);
    let own_row = ActionRow::builder().title("Use my own .config").subtitle("No file chosen").activatable_widget(&own_check).build();
    own_row.add_prefix(&own_check);
    let own_btn = Button::builder().label("Choose…").valign(Align::Center).build();
    own_btn.add_css_class("flat");
    own_row.add_suffix(&own_btn);
    config_group.add(&match_row);
    config_group.add(&tuned_row);
    config_group.add(&own_row);
    build_page.append(&config_group);

    let req_group = PreferencesGroup::builder().title("Requirements").build();
    let deps_row = ActionRow::builder().title("Build tools").subtitle("Checking…").build();
    let deps_btn = Button::builder()
        .label("Install…")
        .tooltip_text("Install the missing build tools with apt (asks for authentication)")
        .valign(Align::Center)
        .visible(false)
        .build();
    deps_btn.add_css_class("flat");
    deps_row.add_suffix(&deps_btn);
    let ccache_row = ActionRow::builder().title("ccache").subtitle("Checking…").build();
    let space_row = ActionRow::builder().title("Disk space").subtitle("Checking…").build();
    req_group.add(&deps_row);
    req_group.add(&ccache_row);
    req_group.add(&space_row);
    build_page.append(&req_group);

    let stage_label = Label::builder().label("").halign(Align::Center).build();
    stage_label.add_css_class("title-4");
    stage_label.set_visible(false);
    build_page.append(&stage_label);

    let progress = ProgressBar::new();
    progress.set_show_text(true);
    progress.set_text(Some(""));
    progress.set_visible(false);
    build_page.append(&progress);

    let build_btn_box = GtkBox::new(Orientation::Horizontal, 8);
    build_btn_box.set_halign(Align::Center);
    build_btn_box.set_margin_top(8);
    let build_btn = Button::builder().label("Build Kernel").sensitive(false).build();
    build_btn.add_css_class("pill");
    build_btn.add_css_class("suggested-action");
    let cancel_btn = Button::builder().label("Cancel").visible(false).build();
    cancel_btn.add_css_class("pill");
    cancel_btn.add_css_class("destructive-action");
    build_btn_box.append(&build_btn);
    build_btn_box.append(&cancel_btn);
    build_page.append(&build_btn_box);

    // Rows that show paths, user IDs ("Name <a@kernel.org>") and other text
    // from outside: never parse it as Pango markup, or it is dropped.
    for row in [&source_row, &sig_row, &own_row, &deps_row, &space_row] {
        row.set_use_markup(false);
    }

    let build_scroll = ScrolledWindow::builder().vexpand(true).child(&build_page).build();
    stack.add_titled(&build_scroll, Some("build"), "Build");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 4: Install — finished builds and the install action
    // ═════════════════════════════════════════════════════════════════════════
    let install_page = GtkBox::new(Orientation::Vertical, 12);
    install_page.set_margin_top(12);
    install_page.set_margin_bottom(12);
    install_page.set_margin_start(12);
    install_page.set_margin_end(12);

    let built_group = PreferencesGroup::builder().title("Built Kernels").description("Finished builds in ~/Downloads/kernel-pop").build();
    let built_list = ListBox::new();
    built_list.set_selection_mode(SelectionMode::Single);
    built_list.add_css_class("boxed-list");
    built_group.add(&built_list);
    install_page.append(&built_group);

    let staged_group = PreferencesGroup::builder().title("Selected Kernel").build();
    let staged_row = ActionRow::builder().title("Nothing selected").subtitle("Build a kernel on the Build tab").build();
    let delete_built_btn =
        Button::builder().label("Delete").tooltip_text("Delete this build's packages from ~/Downloads/kernel-pop").valign(Align::Center).visible(false).build();
    delete_built_btn.add_css_class("flat");
    delete_built_btn.add_css_class("destructive-action");
    staged_row.add_suffix(&delete_built_btn);
    let status_row = ActionRow::builder().title("Version Status").subtitle("—").build();
    let source_info_row = ActionRow::builder().title("Source").subtitle("—").build();
    staged_group.add(&staged_row);
    staged_group.add(&status_row);
    staged_group.add(&source_info_row);
    install_page.append(&staged_group);

    let files_group =
        PreferencesGroup::builder().title("Packages").description("Each one checked against the SHA256SUMS manifest written when it was built").build();
    let files_list = ListBox::new();
    files_list.set_selection_mode(SelectionMode::None);
    files_list.add_css_class("boxed-list");
    files_group.add(&files_list);
    install_page.append(&files_group);

    let steps_group = PreferencesGroup::builder().title("Install Will").build();
    let step_row = ActionRow::builder()
        .title("Check the packages again as root, dpkg install, build DKMS modules, then generate and VERIFY the initramfs, then update the boot loader")
        .subtitle("The install fails loudly if a package does not match the manifest, or if initrd.img or the boot menu entry does not appear — no silent unbootable kernels. Only the kernel image and headers are installed, never linux-libc-dev.")
        .build();
    steps_group.add(&step_row);
    install_page.append(&steps_group);

    let install_progress = ProgressBar::new();
    install_progress.set_show_text(true);
    install_progress.set_text(Some(""));
    install_progress.set_visible(false);
    install_page.append(&install_progress);

    let btn_box = GtkBox::new(Orientation::Horizontal, 8);
    btn_box.set_halign(Align::Center);
    btn_box.set_margin_top(8);
    let install_btn = Button::builder().label("Install Kernel").sensitive(false).build();
    install_btn.add_css_class("pill");
    install_btn.add_css_class("suggested-action");
    btn_box.append(&install_btn);
    install_page.append(&btn_box);

    for row in [&staged_row, &status_row, &source_info_row] {
        row.set_use_markup(false);
    }

    let install_scroll = ScrolledWindow::builder().vexpand(true).child(&install_page).build();
    stack.add_titled(&install_scroll, Some("install"), "Install");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 5: Log
    // ═════════════════════════════════════════════════════════════════════════
    let log_page = GtkBox::new(Orientation::Vertical, 0);
    let log_view = TextView::builder().editable(false).monospace(true).wrap_mode(WrapMode::Word).vexpand(true).build();
    log_view.add_css_class("card");
    log_view.set_margin_top(8);
    log_view.set_margin_bottom(8);
    log_view.set_margin_start(8);
    log_view.set_margin_end(8);
    let log_scroll = ScrolledWindow::builder().vexpand(true).child(&log_view).build();
    let clear_btn = Button::builder().label("Clear Log").halign(Align::End).margin_end(8).margin_bottom(8).build();
    clear_btn.add_css_class("flat");
    log_page.append(&log_scroll);
    log_page.append(&clear_btn);
    stack.add_titled(&log_page, Some("log"), "Log");

    // ── Assemble window ───────────────────────────────────────────────────────
    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);

    let health_banner = Banner::new("");
    health_banner.set_revealed(false);
    content.append(&health_banner);

    // The privileged helper must be the one this build ships: AppRun
    // installs it at startup, and a cancelled or failed install leaves the
    // old one (or none) in place. install.rs refuses to run a mismatched one.
    let helper_banner = Banner::new("");
    match helper_problem() {
        Some(problem) => {
            helper_banner.set_title(&problem);
            helper_banner.set_revealed(true);
        }
        None => helper_banner.set_revealed(false),
    }
    content.append(&helper_banner);

    content.append(&stack);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // ─────────────────────────────────────────────────────────────────────────
    //  Helpers
    // ─────────────────────────────────────────────────────────────────────────

    // Append text to the Log tab, keeping at most LOG_MAX_LINES lines.
    let append_log: Rc<dyn Fn(&str)> = {
        let log_view = log_view.clone();
        Rc::new(move |text: &str| {
            let buf = log_view.buffer();
            let mut end = buf.end_iter();
            buf.insert(&mut end, text);
            buf.insert(&mut end, "\n");
            let lines = buf.line_count();
            if lines > LOG_MAX_LINES {
                let mut start = buf.start_iter();
                if let Some(mut cut) = buf.iter_at_line(lines - LOG_MAX_LINES) {
                    buf.delete(&mut start, &mut cut);
                }
            }
            let mark = buf.create_mark(None, &buf.end_iter(), false);
            log_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
            buf.delete_mark(&mark);
        })
    };

    let log_fn = {
        let append_log = append_log.clone();
        move |msg: String| append_log(&msg)
    };

    // Worker threads (signature check, build) report through this channel;
    // the main loop drains it in batches so a fast build cannot flood GTK.
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiMsg>();
    {
        let append_log = append_log.clone();
        let stage_label = stage_label.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Some(first) = ui_rx.recv().await {
                let mut batch = String::new();
                let handle = |m: UiMsg, batch: &mut String| match m {
                    UiMsg::Log(l) => {
                        if !batch.is_empty() {
                            batch.push('\n');
                        }
                        batch.push_str(&l);
                    }
                    UiMsg::Stage(s) => {
                        stage_label.set_label(s.label());
                        stage_label.set_visible(true);
                    }
                };
                handle(first, &mut batch);
                while let Ok(m) = ui_rx.try_recv() {
                    handle(m, &mut batch);
                }
                if !batch.is_empty() {
                    append_log(&batch);
                }
                glib::timeout_future(std::time::Duration::from_millis(150)).await;
            }
        });
    }
    let event_sink: EventSink = {
        let tx = ui_tx.clone();
        Arc::new(move |e: BuildEvent| {
            let _ = tx.send(match e {
                BuildEvent::Stage(s) => UiMsg::Stage(s),
                BuildEvent::Line(l) => UiMsg::Log(l),
            });
        })
    };

    // Sensitivity of every control that starts a download, a build or a
    // privileged action, from the state: nothing can start while something
    // is busy. The per-kernel Sign / Boot Next Time / Remove buttons live in
    // kernels_list, so making the list insensitive covers them, including
    // rows rebuilt while busy.
    let refresh_controls: Rc<dyn Fn()> = {
        let state = state.clone();
        let download_btn = download_btn.clone();
        let build_btn = build_btn.clone();
        let install_btn = install_btn.clone();
        let prune_btn = prune_btn.clone();
        let setup_signing_btn = setup_signing_btn.clone();
        let kernels_list = kernels_list.clone();
        let cancel_btn = cancel_btn.clone();
        let open_btn = open_btn.clone();
        let own_btn = own_btn.clone();
        let recheck_btn = recheck_btn.clone();
        let unverified_row = unverified_row.clone();
        let deps_btn = deps_btn.clone();
        let config_group = config_group.clone();
        let built_list = built_list.clone();
        let delete_built_btn = delete_built_btn.clone();
        Rc::new(move || {
            let s = state.borrow();
            let idle = s.busy.is_none();
            download_btn.set_sensitive(idle && s.selected_release.is_some());
            build_btn.set_sensitive(idle && s.can_build());
            install_btn.set_sensitive(idle && s.staged.is_some() && s.staged_verified);
            prune_btn.set_sensitive(idle);
            setup_signing_btn.set_sensitive(idle);
            kernels_list.set_sensitive(idle);
            open_btn.set_sensitive(idle);
            own_btn.set_sensitive(idle);
            recheck_btn.set_sensitive(idle);
            unverified_row.set_sensitive(idle);
            deps_btn.set_sensitive(idle);
            deps_btn.set_visible(!s.missing_deps.is_empty());
            config_group.set_sensitive(idle);
            built_list.set_sensitive(idle);
            delete_built_btn.set_sensitive(idle);
            delete_built_btn.set_visible(s.staged.is_some());
            cancel_btn.set_visible(s.download_cancel.is_some() || s.build_control.is_some());
            cancel_btn.set_label(if s.build_control.is_some() { "Cancel Build" } else { "Cancel Download" });
        })
    };

    let end_busy: Rc<dyn Fn()> = {
        let state = state.clone();
        let refresh_controls = refresh_controls.clone();
        Rc::new(move || {
            {
                let mut s = state.borrow_mut();
                s.busy = None;
                s.download_cancel = None;
                s.build_control = None;
                s.build_started = None;
            }
            refresh_controls();
        })
    };

    let begin_busy: BeginBusy = {
        let state = state.clone();
        let refresh_controls = refresh_controls.clone();
        let toast_overlay = toast_overlay.clone();
        Rc::new(move |what: &'static str| {
            let running = state.borrow().busy;
            if let Some(running) = running {
                toast_overlay.add_toast(Toast::new(&format!("Wait until the {running} has finished")));
                return None;
            }
            state.borrow_mut().busy = Some(what);
            refresh_controls();
            Some(BusyGuard(end_busy.clone()))
        })
    };

    // The Install tab, from the selected build.
    let update_install_tab: Rc<dyn Fn()> = {
        let staged_row = staged_row.clone();
        let status_row = status_row.clone();
        let source_info_row = source_info_row.clone();
        let files_list = files_list.clone();
        let refresh_controls = refresh_controls.clone();
        let state = state.clone();
        Rc::new(move || {
            let s = state.borrow();
            while let Some(c) = files_list.first_child() {
                files_list.remove(&c);
            }

            match &s.staged {
                Some(set) => {
                    staged_row.set_title(&format!("Linux {}", set.kernelrelease));
                    staged_row.set_subtitle(&set.dir.display().to_string());
                    let ver = &set.kernelrelease;
                    match compare_to_running(&s.sysinfo.running_kernel, ver) {
                        VersionRelation::Newer => status_row.set_subtitle(&format!("Upgrade: {} → {}", s.sysinfo.running_kernel, ver)),
                        VersionRelation::Same => status_row.set_subtitle(&format!("Same numeric version as running ({})", s.sysinfo.running_kernel)),
                        VersionRelation::Older => status_row.set_subtitle(&format!("Downgrade: {} → {}", s.sysinfo.running_kernel, ver)),
                        VersionRelation::Unknown => status_row.set_subtitle("—"),
                    }
                    source_info_row.set_subtitle(if set.source.is_empty() { "—" } else { &set.source });
                    for fname in &set.debs {
                        let row = ActionRow::builder().title(fname).use_markup(false).build();
                        let badge = Label::new(Some(if s.staged_verified { "SHA256 ✓" } else { "checking…" }));
                        badge.add_css_class(if s.staged_verified { "success" } else { "dim-label" });
                        row.add_suffix(&badge);
                        files_list.append(&row);
                    }
                }
                None => {
                    staged_row.set_title("Nothing selected");
                    staged_row.set_subtitle("Build a kernel on the Build tab");
                    status_row.set_subtitle("—");
                    source_info_row.set_subtitle("—");
                }
            }
            drop(s);
            refresh_controls();
        })
    };

    // load_sysinfo — declared as Rc so remove/install callbacks can re-run it
    let load_sysinfo: Rc<dyn Fn()> = {
        let running_row = running_row.clone();
        let kernels_list = kernels_list.clone();
        let boot_row = boot_row.clone();
        let root_row = root_row.clone();
        let secure_boot_row = secure_boot_row.clone();
        let boot_path_row = boot_path_row.clone();
        let signing_row = signing_row.clone();
        let setup_signing_btn = setup_signing_btn.clone();
        let health_banner = health_banner.clone();
        let update_banner = update_banner.clone();
        let state = state.clone();
        let update_install_tab = update_install_tab.clone();
        let log_fn = log_fn.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let begin_busy = begin_busy.clone();

        // Forward declaration hack: the remove action needs load_sysinfo,
        // which is what we're building. A Rc<RefCell<Option<…>>> slot breaks
        // the cycle: the closure looks the callable up at click time.
        let self_slot: ReloadSlot = Rc::new(RefCell::new(None));
        let self_slot_outer = self_slot.clone();

        let f: Rc<dyn Fn()> = Rc::new(move || {
            running_row.set_title("Detecting…");
            log_fn("Querying installed kernels…".to_string());

            let running_row = running_row.clone();
            let kernels_list = kernels_list.clone();
            let boot_row = boot_row.clone();
            let root_row = root_row.clone();
            let secure_boot_row = secure_boot_row.clone();
            let boot_path_row = boot_path_row.clone();
            let signing_row = signing_row.clone();
            let setup_signing_btn = setup_signing_btn.clone();
            let health_banner = health_banner.clone();
            let update_banner = update_banner.clone();
            let state = state.clone();
            let update_install_tab = update_install_tab.clone();
            let log_fn = log_fn.clone();
            let window = window.clone();
            let toast_overlay = toast_overlay.clone();
            let self_slot = self_slot.clone();
            let begin_busy = begin_busy.clone();

            spawn_async(async move { tokio::task::spawn_blocking(query_system).await.unwrap_or_default() }, move |info: SystemInfo| {
                running_row.set_title(&info.running_kernel);
                running_row.set_subtitle("uname -r");

                // Boot safety rows
                secure_boot_row.set_subtitle(secure_boot_subtitle(info.secure_boot, info.kernel_signing));
                match boot_path_subtitle(&info.boot_path) {
                    Some(text) => {
                        boot_path_row.set_subtitle(&text);
                        boot_path_row.set_visible(true);
                    }
                    None => boot_path_row.set_visible(false),
                }
                let dkms_note = match info.dkms_key {
                    Some(KeyEnrollment::NotEnrolled) => {
                        " The DKMS key (NVIDIA driver and other out-of-tree modules) is not enrolled; Set Up Signing enrolls it too."
                    }
                    Some(KeyEnrollment::Pending) => " The DKMS key (NVIDIA driver) is waiting for enrollment too.",
                    _ => "",
                };
                let base = signing_subtitle(info.kernel_signing);
                let sep = if dkms_note.is_empty() || base.ends_with('.') { "" } else { "." };
                signing_row.set_subtitle(&format!("{base}{sep}{dkms_note}"));
                setup_signing_btn.set_visible(
                    matches!(info.kernel_signing, KernelSigning::NoKey | KernelSigning::KeyNotEnrolled)
                        || info.dkms_key == Some(KeyEnrollment::NotEnrolled)
                        || info.boot_path == BootPath::DirectSystemdBoot,
                );

                // Disk rows
                match info.free_boot_bytes {
                    Some(f) if info.boot_low() => {
                        boot_row.set_subtitle(&format!("Low: {} free, a new kernel needs about {}", format_bytes(f), format_bytes(info.boot_needed_bytes)));
                    }
                    Some(f) => boot_row.set_subtitle(&format!("{} free", format_bytes(f))),
                    None => boot_row.set_subtitle("Could not determine"),
                }
                match info.free_root_bytes {
                    Some(f) if f < MIN_ROOT_BYTES => {
                        root_row.set_subtitle(&format!("Low: {} free (2 GB recommended)", format_bytes(f)));
                    }
                    Some(f) => root_row.set_subtitle(&format!("{} free", format_bytes(f))),
                    None => root_row.set_subtitle("Could not determine"),
                }

                // Kernel rows
                while let Some(c) = kernels_list.first_child() {
                    kernels_list.remove(&c);
                }
                for k in &info.kernels {
                    let mut subtitle_parts = vec![];
                    subtitle_parts.push(if k.has_initrd { "initrd ✓".to_string() } else { "INITRD MISSING".to_string() });
                    subtitle_parts.push(if k.has_modules { "modules ✓".to_string() } else { "modules missing".to_string() });
                    subtitle_parts.push(match k.boot_entry {
                        BootEntry::Present => "boot menu ✓".to_string(),
                        BootEntry::Missing => "NOT IN BOOT MENU".to_string(),
                        // How kernelstub works, not a fault.
                        BootEntry::NotKept => "not in the boot menu (kernelstub keeps only the current and previous kernel)".to_string(),
                    });

                    let row = ActionRow::builder().title(&k.version).subtitle(subtitle_parts.join(" · ")).build();

                    // Signature badge, only once signing is set up.
                    if let Some(sig) = k.signed {
                        let (text, css) = match sig {
                            KernelSignature::KernelPop => ("Signed", "success"),
                            KernelSignature::Distro => ("Signed by Ubuntu", "success"),
                            KernelSignature::Unsigned => ("Unsigned", "warning"),
                        };
                        let badge = Label::new(Some(text));
                        badge.add_css_class("caption");
                        badge.add_css_class(css);
                        badge.set_valign(Align::Center);
                        row.add_suffix(&badge);
                    }
                    if k.signed == Some(KernelSignature::Unsigned) {
                        let sign_btn = Button::builder().label("Sign").tooltip_text("Sign this kernel with the Kernel Pop key").valign(Align::Center).build();
                        sign_btn.add_css_class("flat");
                        let ver = k.version.clone();
                        let toast_overlay = toast_overlay.clone();
                        let log_fn = log_fn.clone();
                        let self_slot = self_slot.clone();
                        let begin_busy = begin_busy.clone();
                        sign_btn.connect_clicked(move |_| {
                            let Some(busy) = begin_busy("signing") else { return };
                            log_fn(format!("Signing kernel {}…", ver));
                            let ver2 = ver.clone();
                            let toast_overlay = toast_overlay.clone();
                            let log_fn = log_fn.clone();
                            let self_slot = self_slot.clone();
                            spawn_async(async move { tokio::task::spawn_blocking(move || run_privileged_sign(&ver2)).await }, move |result| {
                                drop(busy);
                                match result {
                                    Ok(Ok(out)) => {
                                        let msg = report_run(&log_fn, &out, Done::Sign);
                                        toast_overlay.add_toast(Toast::new(&msg));
                                    }
                                    Ok(Err(e)) => {
                                        log_fn(format!("Signing failed: {}", e));
                                        toast_overlay.add_toast(Toast::new("Signing failed — see Log tab"));
                                    }
                                    Err(e) => log_fn(format!("Task error: {}", e)),
                                }
                                if let Some(reload) = self_slot.borrow().clone() {
                                    reload();
                                }
                            });
                        });
                        row.add_suffix(&sign_btn);
                    }

                    if k.running {
                        let badge = Label::new(Some("running"));
                        badge.add_css_class("success");
                        row.add_suffix(&badge);
                    } else {
                        let remove_btn = Button::builder().label("Remove").valign(Align::Center).build();
                        remove_btn.add_css_class("destructive-action");
                        remove_btn.add_css_class("flat");

                        // One-time boot of this kernel; the saved default stays put.
                        let boot_btn = Button::builder()
                            .label("Boot Next Time")
                            .tooltip_text("Boot this kernel on the next restart only; the default is unchanged")
                            .valign(Align::Center)
                            .build();
                        boot_btn.add_css_class("flat");
                        {
                            let ver = k.version.clone();
                            let toast_overlay = toast_overlay.clone();
                            let log_fn = log_fn.clone();
                            let begin_busy = begin_busy.clone();
                            boot_btn.connect_clicked(move |_| {
                                let Some(busy) = begin_busy("boot selection") else { return };
                                log_fn(format!("Selecting {} for the next boot only…", ver));
                                let ver2 = ver.clone();
                                let toast_overlay = toast_overlay.clone();
                                let log_fn = log_fn.clone();
                                spawn_async(async move { tokio::task::spawn_blocking(move || run_privileged_boot_once(&ver2)).await }, move |result| {
                                    drop(busy);
                                    match result {
                                        Ok(Ok(out)) => {
                                            log_output(&log_fn, &out);
                                            toast_overlay.add_toast(Toast::new("The next restart will boot that kernel once"));
                                        }
                                        Ok(Err(e)) => {
                                            log_fn(format!("Boot-once failed: {}", e));
                                            toast_overlay.add_toast(Toast::new("Could not set next boot — see Log tab"));
                                        }
                                        Err(e) => log_fn(format!("Task error: {}", e)),
                                    }
                                });
                            });
                        }

                        let ver = k.version.clone();
                        let window = window.clone();
                        let toast_overlay = toast_overlay.clone();
                        let log_fn = log_fn.clone();
                        let self_slot = self_slot.clone();
                        let begin_busy = begin_busy.clone();

                        remove_btn.connect_clicked(move |_| {
                            let dialog = AlertDialog::builder()
                                .heading(format!("Remove kernel {}?", ver))
                                .body("Its packages will be purged and the boot loader updated. The running kernel is never touched.")
                                .build();
                            dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                            dialog.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);
                            dialog.set_default_response(Some("cancel"));

                            let ver2 = ver.clone();
                            let toast_overlay = toast_overlay.clone();
                            let log_fn = log_fn.clone();
                            let self_slot = self_slot.clone();
                            let begin_busy = begin_busy.clone();
                            dialog.connect_response(None, move |_, resp| {
                                if resp != "remove" {
                                    return;
                                }
                                let Some(busy) = begin_busy("kernel removal") else { return };
                                let ver3 = ver2.clone();
                                log_fn(format!("Removing kernel {}…", ver3));
                                let toast_overlay = toast_overlay.clone();
                                let log_fn = log_fn.clone();
                                let self_slot = self_slot.clone();
                                spawn_async(async move { tokio::task::spawn_blocking(move || run_privileged_remove(&ver3)).await }, move |result| {
                                    drop(busy);
                                    match result {
                                        Ok(Ok(out)) => {
                                            log_output(&log_fn, &out);
                                            log_fn("Kernel removed.".to_string());
                                            toast_overlay.add_toast(Toast::new("Kernel removed"));
                                        }
                                        Ok(Err(e)) => {
                                            log_fn(format!("Remove failed: {}", e));
                                            toast_overlay.add_toast(Toast::new("Remove failed — see Log tab"));
                                        }
                                        Err(e) => log_fn(format!("Task error: {}", e)),
                                    }
                                    if let Some(reload) = self_slot.borrow().clone() {
                                        reload();
                                    }
                                });
                            });
                            dialog.present(Some(&window));
                        });

                        row.add_suffix(&boot_btn);
                        row.add_suffix(&remove_btn);
                    }

                    if !k.healthy() {
                        row.add_css_class("error");
                    }
                    kernels_list.append(&row);
                }

                if info.kernels.is_empty() {
                    let row = ActionRow::builder().title("No kernels found in /boot").build();
                    kernels_list.append(&row);
                }

                match health_summary(&info.kernels) {
                    Some(text) => {
                        health_banner.set_title(&text);
                        health_banner.set_revealed(true);
                        log_fn(format!("WARNING: {text}"));
                    }
                    None => health_banner.set_revealed(false),
                }

                log_fn(format!("Running kernel: {} · {} kernels installed", info.running_kernel, info.kernels.len()));
                let running_kernel = info.running_kernel.clone();
                state.borrow_mut().sysinfo = info;
                update_install_tab();

                // Nice-to-have, not safety-critical: check kernel.org for a
                // newer stable release without blocking the System tab's
                // (local, fast) render above. A failed fetch is silent —
                // the Browse tab shows the same data and its errors.
                update_banner.set_revealed(false);
                spawn_async(fetch_releases(), move |result| {
                    let releases = match result {
                        Ok(r) => r,
                        Err(_e) => {
                            #[cfg(debug_assertions)]
                            eprintln!("update-check: fetch_releases failed: {_e}");
                            return;
                        }
                    };
                    if let Some(newest) = releases.iter().find(|r| r.moniker == Moniker::Stable) {
                        if compare_to_running(&running_kernel, &newest.version) == VersionRelation::Newer {
                            update_banner.set_title(&format!("Linux {} (stable) is out — download it on the Browse tab to build it", newest.version));
                            update_banner.set_revealed(true);
                        }
                    }
                });
            });
        });
        *self_slot_outer.borrow_mut() = Some(f.clone());
        f
    };

    load_sysinfo();

    {
        let ls = load_sysinfo.clone();
        refresh_sysinfo_btn.connect_clicked(move |_| ls());
    }

    // Remove old kernels: ask how many to keep, show exactly what would go,
    // then remove them all under one authentication. The running kernel and
    // the newest installed kernel are never candidates (and the script
    // enforces the same rule on its side).
    {
        let window = window.clone();
        let state = state.clone();
        let log_fn = log_fn.clone();
        let toast_overlay = toast_overlay.clone();
        let load_sysinfo = load_sysinfo.clone();
        let begin_busy = begin_busy.clone();
        prune_btn.connect_clicked(move |_| {
            let keep_spin = SpinButton::with_range(1.0, 10.0, 1.0);
            keep_spin.set_value(2.0);
            keep_spin.set_halign(Align::Center);

            let ask = AlertDialog::builder()
                .heading("Remove old kernels")
                .body("How many of the newest installed kernels should be kept? The running kernel is always kept as well.")
                .build();
            ask.set_extra_child(Some(&keep_spin));
            ask.add_responses(&[("cancel", "Cancel"), ("review", "Review")]);
            ask.set_default_response(Some("review"));
            ask.set_close_response("cancel");

            let window2 = window.clone();
            let state = state.clone();
            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let load_sysinfo = load_sysinfo.clone();
            let begin_busy = begin_busy.clone();
            ask.connect_response(None, move |_, resp| {
                if resp != "review" { return; }
                let keep = keep_spin.value() as usize;
                let candidates = prune_candidates(&state.borrow().sysinfo.kernels, keep);
                if candidates.is_empty() {
                    toast_overlay.add_toast(Toast::new("Nothing to remove"));
                    return;
                }

                let confirm = AlertDialog::builder()
                    .heading(format!("Remove {} kernel(s)?", candidates.len()))
                    .body(format!(
                        "These will be purged and the boot loader updated:\n\n{}\n\nKernels whose removal would also remove the kernel metapackages are skipped and reported in the Log.",
                        candidates.join("\n")
                    ))
                    .build();
                confirm.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                confirm.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);
                confirm.set_default_response(Some("cancel"));
                confirm.set_close_response("cancel");

                let log_fn = log_fn.clone();
                let toast_overlay = toast_overlay.clone();
                let load_sysinfo = load_sysinfo.clone();
                let begin_busy = begin_busy.clone();
                confirm.connect_response(None, move |_, resp| {
                    if resp != "remove" { return; }
                    let Some(busy) = begin_busy("kernel removal") else { return };
                    log_fn(format!("Removing old kernels: {}", candidates.join(", ")));
                    let list = candidates.clone();
                    let log_fn = log_fn.clone();
                    let toast_overlay = toast_overlay.clone();
                    let load_sysinfo = load_sysinfo.clone();
                    spawn_async(
                        async move {
                            tokio::task::spawn_blocking(move || run_privileged_remove_many(&list)).await
                        },
                        move |result| {
                            drop(busy);
                            match result {
                                Ok(Ok(out)) => {
                                    log_output(&log_fn, &out);
                                    toast_overlay.add_toast(Toast::new("Old kernels removed"));
                                }
                                Ok(Err(e)) => {
                                    log_fn(format!("Removal finished with problems: {}", e));
                                    toast_overlay.add_toast(Toast::new("Some kernels were not removed — see Log tab"));
                                }
                                Err(e) => log_fn(format!("Task error: {}", e)),
                            }
                            load_sysinfo();
                        },
                    );
                });
                confirm.present(Some(&window2));
            });
            ask.present(Some(&window));
        });
    }

    // Set up kernel signing: ask for a one-time MOK enrollment password
    // (twice), then create the key, sign the installed kernels and queue the
    // enrollment under one authentication. The password goes to the script on
    // stdin, never on its command line.
    {
        let window = window.clone();
        let log_fn = log_fn.clone();
        let toast_overlay = toast_overlay.clone();
        let load_sysinfo = load_sysinfo.clone();
        let begin_busy = begin_busy.clone();
        setup_signing_btn.connect_clicked(move |_| {
            let pw1 = PasswordEntry::builder()
                .placeholder_text("One-time password")
                .show_peek_icon(true)
                .build();
            let pw2 = PasswordEntry::builder()
                .placeholder_text("Same password again")
                .show_peek_icon(true)
                .activates_default(true)
                .build();
            let hint = Label::new(Some("8 to 16 characters: letters, digits and symbols, no spaces"));
            hint.add_css_class("dim-label");
            hint.add_css_class("caption");
            hint.set_wrap(true);
            let fields = GtkBox::new(Orientation::Vertical, 8);
            fields.append(&pw1);
            fields.append(&pw2);
            fields.append(&hint);

            let dialog = AlertDialog::builder()
                .heading("Set up kernel signing")
                .body("Choose a one-time password for enrolling Kernel Pop's signing key, together with the DKMS key that the NVIDIA driver and other out-of-tree modules are signed with. Any key another app has already queued is kept in the same request. On the next reboot a blue MOK Manager screen appears: choose Enroll MOK and type this password once; it enrolls all of them. A password chosen earlier, here or in another app, no longer works. Turn Secure Boot on only after this screen shows the keys as enrolled.")
                .build();
            dialog.set_extra_child(Some(&fields));
            dialog.add_responses(&[("cancel", "Cancel"), ("setup", "Set Up")]);
            dialog.set_response_appearance("setup", libadwaita::ResponseAppearance::Suggested);
            dialog.set_response_enabled("setup", false);
            dialog.set_default_response(Some("setup"));
            dialog.set_close_response("cancel");

            let revalidate = {
                let dialog = dialog.clone();
                let pw1 = pw1.clone();
                let pw2 = pw2.clone();
                let hint = hint.clone();
                move || {
                    let (a, b) = (pw1.text(), pw2.text());
                    let res = validate_mok_password(&a, &b);
                    dialog.set_response_enabled("setup", res.is_ok());
                    hint.set_label(match res {
                        Ok(()) => "Remember it: you type it once at the next reboot",
                        Err(_) if a.is_empty() => "8 to 16 characters: letters, digits and symbols, no spaces",
                        Err(msg) => msg,
                    });
                }
            };
            { let r = revalidate.clone(); pw1.connect_changed(move |_| r()); }
            { let r = revalidate.clone(); pw2.connect_changed(move |_| r()); }

            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let load_sysinfo = load_sysinfo.clone();
            let begin_busy = begin_busy.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp != "setup" { return; }
                let password = pw1.text().to_string();
                if validate_mok_password(&password, &pw2.text()).is_err() { return; }
                let Some(busy) = begin_busy("signing setup") else { return };
                pw1.set_text("");
                pw2.set_text("");
                log_fn("Setting up kernel signing…".to_string());
                let log_fn = log_fn.clone();
                let toast_overlay = toast_overlay.clone();
                let load_sysinfo = load_sysinfo.clone();
                spawn_async(
                    async move {
                        tokio::task::spawn_blocking(move || run_privileged_setup_signing(&password)).await
                    },
                    move |result| {
                        drop(busy);
                        match result {
                            Ok(Ok(out)) => {
                                let msg = report_run(&log_fn, &out, Done::SetupSigning);
                                toast_overlay.add_toast(Toast::new(&msg));
                            }
                            Ok(Err(e)) => {
                                log_fn(format!("Signing setup failed: {}", e));
                                toast_overlay.add_toast(Toast::new("Signing setup failed — see Log tab"));
                            }
                            Err(e) => log_fn(format!("Task error: {}", e)),
                        }
                        load_sysinfo();
                    },
                );
            });
            dialog.present(Some(&window));
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Browse list — kernel.org releases, badges relative to the running kernel
    // ─────────────────────────────────────────────────────────────────────────
    fn populate_list(list_box: &ListBox, releases: &[Release], filter: &str, running: &str) {
        while let Some(c) = list_box.first_child() {
            list_box.remove(&c);
        }
        let filter = filter.to_lowercase();

        for rel in releases {
            if !filter.is_empty() && !rel.version.contains(&filter) && !rel.moniker.label().contains(&filter) {
                continue;
            }
            let relation = match compare_to_running(running, &rel.version) {
                VersionRelation::Newer => "newer than running",
                VersionRelation::Same => "matches the running kernel",
                VersionRelation::Older => "older than running",
                VersionRelation::Unknown => "",
            };
            let mut parts = vec![rel.moniker.label().to_string()];
            if !rel.released.is_empty() {
                parts.push(format!("released {}", rel.released));
            }
            if !relation.is_empty() {
                parts.push(relation.to_string());
            }
            if rel.pgp.is_none() {
                parts.push("no signature published".to_string());
            }
            let row = ActionRow::builder().title(&rel.version).subtitle(parts.join(" · ")).activatable(true).build();
            if compare_to_running(running, &rel.version) == VersionRelation::Same {
                row.add_css_class("success");
            }
            if rel.is_eol {
                let badge = Label::new(Some("EOL"));
                badge.add_css_class("caption");
                badge.add_css_class("warning");
                badge.set_valign(Align::Center);
                row.add_suffix(&badge);
            }
            list_box.append(&row);
        }
    }

    let load_releases = {
        let list_box = list_box.clone();
        let list_spinner = list_spinner.clone();
        let selected_label = selected_label.clone();
        let banner = banner.clone();
        let state = state.clone();
        let log_fn = log_fn.clone();
        let search_entry = search_entry.clone();
        let refresh_controls = refresh_controls.clone();

        move || {
            list_spinner.start();
            list_spinner.set_visible(true);
            banner.set_revealed(false);
            // Removing the rows deselects, which clears the selection and
            // the Download button through row-selected.
            while let Some(c) = list_box.first_child() {
                list_box.remove(&c);
            }
            state.borrow_mut().selected_release = None;
            refresh_controls();
            selected_label.set_label("Loading…");
            log_fn("Fetching the release list from kernel.org…".to_string());

            let list_box = list_box.clone();
            let list_spinner = list_spinner.clone();
            let selected_label = selected_label.clone();
            let banner = banner.clone();
            let state = state.clone();
            let log_fn = log_fn.clone();
            let search_entry = search_entry.clone();

            spawn_async(fetch_releases(), move |result| {
                list_spinner.stop();
                list_spinner.set_visible(false);
                match result {
                    Err(e) => {
                        log_fn(format!("Error fetching releases: {e:#}"));
                        banner.set_title(&format!("Failed to load releases: {e}"));
                        banner.set_revealed(true);
                        selected_label.set_label("Could not load the release list");
                    }
                    Ok(mut releases) => {
                        // A version can be listed twice (as mainline and as
                        // stable right after a release); rows are looked up by
                        // version, so keep the first.
                        let mut seen = std::collections::HashSet::new();
                        releases.retain(|r| seen.insert(r.version.clone()));
                        log_fn(format!("kernel.org lists {} releases", releases.len()));
                        state.borrow_mut().releases = releases.clone();
                        let running = state.borrow().sysinfo.running_kernel.clone();
                        populate_list(&list_box, &releases, &search_entry.text(), &running);
                        selected_label.set_label("No release selected");
                    }
                }
            });
        }
    };

    load_releases();

    {
        let lr = load_releases.clone();
        refresh_btn.connect_clicked(move |_| lr());
    }

    // Row selection — connected exactly once, looked up by the row's title
    // (the version string), never by row index: with an active filter the
    // visible rows are a subset.
    {
        let state = state.clone();
        let refresh_controls = refresh_controls.clone();
        let selected_label = selected_label.clone();
        list_box.connect_row_selected(move |_, row| {
            let selected = row
                .and_then(|r| r.downcast_ref::<ActionRow>().map(|ar| ar.title().to_string()))
                .and_then(|title| state.borrow().releases.iter().find(|r| r.version == title).cloned());
            match &selected {
                Some(r) => selected_label.set_label(&format!("Selected: {} ({})", r.version, r.moniker.label())),
                None => selected_label.set_label("No release selected"),
            }
            state.borrow_mut().selected_release = selected;
            refresh_controls();
        });
    }

    {
        let list_box = list_box.clone();
        let state = state.clone();
        search_entry.connect_search_changed(move |entry| {
            // Clone out and drop the borrow first: populate_list can remove the
            // selected row, firing row-selected which takes borrow_mut().
            let (releases, running) = {
                let s = state.borrow();
                (s.releases.clone(), s.sysinfo.running_kernel.clone())
            };
            populate_list(&list_box, &releases, &entry.text(), &running);
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Build page: source rows, requirements
    // ─────────────────────────────────────────────────────────────────────────
    let update_source_rows: Rc<dyn Fn()> = {
        let state = state.clone();
        let source_row = source_row.clone();
        let sig_row = sig_row.clone();
        let recheck_btn = recheck_btn.clone();
        let unverified_row = unverified_row.clone();
        let refresh_controls = refresh_controls.clone();
        Rc::new(move || {
            let s = state.borrow();
            match &s.source {
                Some(t) => {
                    source_row.set_title(&format!("Linux {}", t.version.kernelversion()));
                    source_row.set_subtitle(&t.origin.display().to_string());
                }
                None => {
                    source_row.set_title("No source selected");
                    source_row.set_subtitle("Its version is read from the tarball's Makefile");
                }
            }
            sig_row.remove_css_class("success");
            sig_row.remove_css_class("error");
            match &s.signature {
                SigState::None => sig_row.set_subtitle("—"),
                SigState::Checking => sig_row.set_subtitle("Checking against the kernel.org keys…"),
                SigState::Verified(signer) => {
                    sig_row.set_subtitle(&format!("Good signature from {} ({}), key fetched from kernel.org", signer.uid, signer.grouped_fingerprint()));
                    sig_row.add_css_class("success");
                }
                SigState::Failed(why) => {
                    sig_row.set_subtitle(&format!("NOT VERIFIED — {why}"));
                    sig_row.add_css_class("error");
                }
            }
            let failed = matches!(s.signature, SigState::Failed(_));
            recheck_btn.set_visible(failed);
            unverified_row.set_visible(failed);
            if unverified_row.is_active() != s.allow_unverified {
                unverified_row.set_active(s.allow_unverified);
            }
            drop(s);
            refresh_controls();
        })
    };

    // Build tools, ccache and disk space, checked without blocking the UI.
    let refresh_requirements: Rc<dyn Fn()> = {
        let state = state.clone();
        let deps_row = deps_row.clone();
        let ccache_row = ccache_row.clone();
        let space_row = space_row.clone();
        let refresh_controls = refresh_controls.clone();
        Rc::new(move || {
            let state = state.clone();
            let deps_row = deps_row.clone();
            let ccache_row = ccache_row.clone();
            let space_row = space_row.clone();
            let refresh_controls = refresh_controls.clone();
            spawn_async(
                async move {
                    tokio::task::spawn_blocking(|| {
                        // An earlier failed build's tree is removed when the next
                        // build starts, so its space counts as free here.
                        let root = build::build_root();
                        let cache_free = free_bytes(&root).map(|f| f + build::dir_size(&root));
                        (missing_build_deps(), ccache_available(), cache_free, free_bytes(std::path::Path::new("/boot")))
                    })
                    .await
                },
                move |result| {
                    let Ok((missing, ccache, cache_free, boot_free)) = result else { return };
                    if missing.is_empty() {
                        deps_row.set_subtitle("All installed");
                    } else {
                        deps_row.set_subtitle(&format!("Missing: {}", missing.join(", ")));
                    }
                    ccache_row.set_subtitle(if ccache {
                        "Installed — used, so rebuilds and the next point release compile much faster"
                    } else {
                        "Not installed (optional) — install ccache to make rebuilds much faster"
                    });
                    let (mode, boot_need) = {
                        let mut s = state.borrow_mut();
                        s.missing_deps = missing;
                        (s.config_mode().unwrap_or(ConfigMode::MatchSystem), s.sysinfo.boot_needed_bytes)
                    };
                    match space_problem(&mode, cache_free, boot_free, boot_need) {
                        Some(p) => space_row.set_subtitle(&p),
                        None => space_row.set_subtitle(&format!(
                            "{} free for the build (about {} needed){}",
                            cache_free.map(format_bytes).unwrap_or_else(|| "?".into()),
                            format_bytes(mode.disk_need()),
                            boot_free.map(|b| format!(", {} free in /boot", format_bytes(b))).unwrap_or_default()
                        )),
                    }
                    refresh_controls();
                },
            );
        })
    };
    refresh_requirements();

    // Look into a tarball, find or fetch its signature, verify it.
    let set_source: Rc<dyn Fn(PathBuf)> = {
        let state = state.clone();
        let begin_busy = begin_busy.clone();
        let update_source_rows = update_source_rows.clone();
        let log_fn = log_fn.clone();
        let ui_tx = ui_tx.clone();
        let toast_overlay = toast_overlay.clone();
        let stack = stack.clone();
        Rc::new(move |path: PathBuf| {
            let Some(busy) = begin_busy("signature check") else { return };
            {
                let mut s = state.borrow_mut();
                s.source = None;
                s.signature = SigState::Checking;
                s.allow_unverified = false;
            }
            update_source_rows();
            stack.set_visible_child_name("build");
            log_fn(format!("Source: {}", path.display()));
            let state = state.clone();
            let update_source_rows = update_source_rows.clone();
            let log_fn = log_fn.clone();
            let tx = ui_tx.clone();
            let toast_overlay = toast_overlay.clone();
            spawn_async(
                async move {
                    let p = path.clone();
                    // Checked and later built: Kernel Pop's private copy, so the
                    // file cannot be swapped between the check and the build.
                    let tarball = tokio::task::spawn_blocking(move || inspect(&p).and_then(|t| private_copy(&t))).await??;
                    let sig = match tarball.sibling_signature() {
                        Some(sig) => sig,
                        None => {
                            // No .tar.sign next to it: fetch kernel.org's by the
                            // version in the Makefile.
                            let _ = tx.send(UiMsg::Log(format!(
                                "No signature next to the tarball; fetching kernel.org's for Linux {}",
                                tarball.version.release_name()
                            )));
                            match fetch_signature(&tarball.version, build::downloads_root()).await {
                                Ok(sig) => sig,
                                Err(e) => return Ok::<_, anyhow::Error>((tarball, Err(format!("{e:#}")))),
                            }
                        }
                    };
                    let t2 = tarball.clone();
                    let verdict = tokio::task::spawn_blocking(move || {
                        let log = move |l: String| {
                            let _ = tx.send(UiMsg::Log(l));
                        };
                        verify_tarball(&t2, &sig, &log).map_err(|e| format!("{e:#}"))
                    })
                    .await?;
                    Ok((tarball, verdict))
                },
                move |result: anyhow::Result<(Tarball, Result<Signer, String>)>| {
                    drop(busy);
                    match result {
                        Err(e) => {
                            log_fn(format!("Not a usable kernel source: {e:#}"));
                            toast_overlay.add_toast(Toast::new("Not a kernel source tarball — see Log tab"));
                            state.borrow_mut().signature = SigState::None;
                        }
                        Ok((tarball, verdict)) => {
                            log_fn(format!("Kernel version from the Makefile: {}", tarball.version.kernelversion()));
                            let mut s = state.borrow_mut();
                            s.source = Some(tarball);
                            match verdict {
                                Ok(signer) => {
                                    log_fn(format!("Signature: good, from {} ({})", signer.uid, signer.grouped_fingerprint()));
                                    s.signature = SigState::Verified(signer);
                                }
                                Err(why) => {
                                    log_fn(format!("Signature: NOT VERIFIED — {why}. The build stays blocked unless \"Build unverified source\" is turned on for this tarball."));
                                    toast_overlay.add_toast(Toast::new("The source could not be verified — see the Build tab"));
                                    s.signature = SigState::Failed(why);
                                }
                            }
                        }
                    }
                    update_source_rows();
                },
            );
        })
    };

    // Open Tarball…
    {
        let window = window.clone();
        let set_source = set_source.clone();
        open_btn.connect_clicked(move |_| {
            let filters = gio::ListStore::new::<gtk4::FileFilter>();
            let tarballs = gtk4::FileFilter::new();
            tarballs.set_name(Some("Kernel source tarballs"));
            for p in ["*.tar.xz", "*.tar.gz", "*.tgz"] {
                tarballs.add_pattern(p);
            }
            filters.append(&tarballs);
            let all = gtk4::FileFilter::new();
            all.set_name(Some("All files"));
            all.add_pattern("*");
            filters.append(&all);
            let dialog = gtk4::FileDialog::builder()
                .title("Open a kernel source tarball")
                .filters(&filters)
                .initial_folder(&gio::File::for_path(build::downloads_root()))
                .build();
            let set_source = set_source.clone();
            dialog.open(Some(&window), None::<&gio::Cancellable>, move |res| {
                if let Some(path) = res.ok().and_then(|f| f.path()) {
                    set_source(path);
                }
            });
        });
    }

    // Drop a tarball anywhere on the window.
    {
        let set_source = set_source.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let drop_target = gtk4::DropTarget::new(gio::File::static_type(), gtk4::gdk::DragAction::COPY);
        drop_target.connect_drop(move |_, value, _, _| {
            let Some(path) = value.get::<gio::File>().ok().and_then(|f| f.path()) else { return false };
            if let Some(running) = state.borrow().busy {
                toast_overlay.add_toast(Toast::new(&format!("Wait until the {running} has finished")));
                return false;
            }
            set_source(path);
            true
        });
        window.add_controller(drop_target);
    }

    // Check Again (a failed fetch of the keys or the signature).
    {
        let set_source = set_source.clone();
        let state = state.clone();
        recheck_btn.connect_clicked(move |_| {
            // The user's file, not the private copy: set_source copies it
            // into the private directory again.
            let path = state.borrow().source.as_ref().map(|t| t.origin.clone());
            if let Some(p) = path {
                set_source(p);
            }
        });
    }

    // Build unverified source: one explicit override per tarball.
    {
        let state = state.clone();
        let window = window.clone();
        let update_source_rows = update_source_rows.clone();
        let log_fn = log_fn.clone();
        unverified_row.connect_active_notify(move |row| {
            let want = row.is_active();
            if want == state.borrow().allow_unverified {
                return;
            }
            if !want {
                state.borrow_mut().allow_unverified = false;
                update_source_rows();
                return;
            }
            let dialog = AlertDialog::builder()
                .heading("Build source that is not verified?")
                .body("This tarball has no valid signature from a kernel.org developer, so nothing shows that it is the code kernel.org released. Only go on for a tree you made or patched yourself. The build log and the package folder will say the source was unverified.")
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("allow", "Build Unverified")]);
            dialog.set_response_appearance("allow", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let state = state.clone();
            let update_source_rows = update_source_rows.clone();
            let log_fn = log_fn.clone();
            dialog.connect_response(None, move |_, resp| {
                let allow = resp == "allow";
                state.borrow_mut().allow_unverified = allow;
                if allow {
                    let path = state.borrow().source.as_ref().map(|t| t.path.display().to_string()).unwrap_or_default();
                    log_fn(format!("WARNING: building {path} without a verified signature was allowed for this tarball"));
                }
                update_source_rows();
            });
            dialog.present(Some(&window));
        });
    }

    // Configuration mode
    {
        let set_mode = {
            let state = state.clone();
            let refresh_controls = refresh_controls.clone();
            let refresh_requirements = refresh_requirements.clone();
            move |m: ModeChoice| {
                state.borrow_mut().mode = m;
                refresh_controls();
                refresh_requirements();
            }
        };
        for (check, m) in [(&match_check, ModeChoice::Match), (&tuned_check, ModeChoice::Tuned), (&own_check, ModeChoice::Own)] {
            let set_mode = set_mode.clone();
            check.connect_toggled(move |c| {
                if c.is_active() {
                    set_mode(m);
                }
            });
        }
    }
    {
        let window = window.clone();
        let state = state.clone();
        let own_row = own_row.clone();
        let own_check = own_check.clone();
        let refresh_controls = refresh_controls.clone();
        own_btn.connect_clicked(move |_| {
            let dialog = gtk4::FileDialog::builder().title("Choose a kernel .config").build();
            let state = state.clone();
            let own_row = own_row.clone();
            let own_check = own_check.clone();
            let refresh_controls = refresh_controls.clone();
            dialog.open(Some(&window), None::<&gio::Cancellable>, move |res| {
                if let Some(path) = res.ok().and_then(|f| f.path()) {
                    own_row.set_subtitle(&format!("{} — the same fixes are applied, then make olddefconfig", path.display()));
                    state.borrow_mut().own_config = Some(path);
                    own_check.set_active(true);
                    refresh_controls();
                }
            });
        });
    }

    // Install the missing build tools.
    {
        let state = state.clone();
        let begin_busy = begin_busy.clone();
        let log_fn = log_fn.clone();
        let toast_overlay = toast_overlay.clone();
        let refresh_requirements = refresh_requirements.clone();
        deps_btn.connect_clicked(move |_| {
            let missing = state.borrow().missing_deps.clone();
            if missing.is_empty() {
                return;
            }
            let Some(busy) = begin_busy("build tools install") else { return };
            log_fn(format!("Installing build tools: {}", missing.join(" ")));
            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let refresh_requirements = refresh_requirements.clone();
            spawn_async(async move { tokio::task::spawn_blocking(move || run_privileged_install_build_deps(&missing)).await }, move |result| {
                drop(busy);
                match result {
                    Ok(Ok(out)) => {
                        log_output(&log_fn, &out);
                        toast_overlay.add_toast(Toast::new("Build tools installed"));
                    }
                    Ok(Err(e)) => {
                        log_fn(format!("Installing the build tools failed: {e}"));
                        toast_overlay.add_toast(Toast::new("Installing the build tools failed — see Log tab"));
                    }
                    Err(e) => log_fn(format!("Task error: {e}")),
                }
                refresh_requirements();
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Install tab: the list of finished builds
    // ─────────────────────────────────────────────────────────────────────────
    // Re-read ~/Downloads/kernel-pop and select `select` (or the newest).
    let load_built: Rc<dyn Fn(Option<PathBuf>)> = {
        let state = state.clone();
        let built_list = built_list.clone();
        let update_install_tab = update_install_tab.clone();
        Rc::new(move |select: Option<PathBuf>| {
            let state = state.clone();
            let built_list = built_list.clone();
            let update_install_tab = update_install_tab.clone();
            spawn_async(async move { tokio::task::spawn_blocking(built_sets).await.unwrap_or_default() }, move |sets: Vec<BuiltSet>| {
                let keep = select.or_else(|| state.borrow().staged.as_ref().map(|s| s.dir.clone()));
                {
                    // The files may have changed (a rebuild into the same
                    // folder): whatever gets selected is checked again.
                    let mut s = state.borrow_mut();
                    s.built = sets.clone();
                    s.staged = None;
                    s.staged_verified = false;
                }
                while let Some(c) = built_list.first_child() {
                    built_list.remove(&c);
                }
                let mut to_select = None;
                for (i, set) in sets.iter().enumerate() {
                    let row = ActionRow::builder()
                        .title(format!("Linux {}", set.kernelrelease))
                        .subtitle(set.dir.display().to_string())
                        .use_markup(false)
                        .activatable(true)
                        .build();
                    if set.source.starts_with("UNVERIFIED") {
                        let badge = Label::new(Some("unverified source"));
                        badge.add_css_class("caption");
                        badge.add_css_class("warning");
                        badge.set_valign(Align::Center);
                        row.add_suffix(&badge);
                    }
                    if keep.as_ref() == Some(&set.dir) || (keep.is_none() && i == 0) {
                        to_select = Some(row.clone());
                    }
                    built_list.append(&row);
                }
                if sets.is_empty() {
                    let row = ActionRow::builder().title("No builds yet").subtitle("Build a kernel on the Build tab").build();
                    row.set_activatable(false);
                    row.set_selectable(false);
                    built_list.append(&row);
                    let mut s = state.borrow_mut();
                    s.staged = None;
                    s.staged_verified = false;
                }
                match to_select {
                    Some(row) => built_list.select_row(Some(&row)),
                    None => {
                        built_list.unselect_all();
                        update_install_tab();
                    }
                }
            });
        })
    };

    {
        let state = state.clone();
        let update_install_tab = update_install_tab.clone();
        let log_fn = log_fn.clone();
        built_list.connect_row_selected(move |list, row| {
            let index = row.map(|r| r.index()).filter(|i| *i >= 0).map(|i| i as usize);
            let set = index.and_then(|i| state.borrow().built.get(i).cloned());
            let same = set.is_some() && state.borrow().staged == set;
            if same && state.borrow().staged_verified {
                return;
            }
            {
                let mut s = state.borrow_mut();
                s.staged = set.clone();
                s.staged_verified = false;
            }
            update_install_tab();
            let Some(set) = set else { return };
            let state = state.clone();
            let update_install_tab = update_install_tab.clone();
            let log_fn = log_fn.clone();
            let list = list.clone();
            spawn_async(
                async move {
                    let s2 = set.clone();
                    (set, tokio::task::spawn_blocking(move || verify_built_set(&s2)).await)
                },
                move |(set, result)| {
                    // Only if it is still the selected one.
                    if state.borrow().staged.as_ref() != Some(&set) {
                        return;
                    }
                    match result {
                        Ok(Ok(())) => state.borrow_mut().staged_verified = true,
                        Ok(Err(e)) => {
                            log_fn(format!("{}: {e:#} — not installable; build it again", set.dir.display()));
                            list.unselect_all();
                        }
                        Err(e) => log_fn(format!("Task error: {e}")),
                    }
                    update_install_tab();
                },
            );
        });
    }

    // Delete a finished build.
    {
        let state = state.clone();
        let window = window.clone();
        let load_built = load_built.clone();
        let log_fn = log_fn.clone();
        delete_built_btn.connect_clicked(move |_| {
            let Some(set) = state.borrow().staged.clone() else { return };
            let dialog = AlertDialog::builder()
                .heading(format!("Delete the build of Linux {}?", set.kernelrelease))
                .body(format!("{} and everything in it (packages, config, build log) is deleted. Installed kernels are not touched.", set.dir.display()))
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            dialog.set_response_appearance("delete", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let state = state.clone();
            let load_built = load_built.clone();
            let log_fn = log_fn.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp != "delete" {
                    return;
                }
                match std::fs::remove_dir_all(&set.dir) {
                    Ok(()) => log_fn(format!("Deleted {}", set.dir.display())),
                    Err(e) => log_fn(format!("Could not delete {}: {e}", set.dir.display())),
                }
                {
                    let mut s = state.borrow_mut();
                    s.staged = None;
                    s.staged_verified = false;
                }
                load_built(None);
            });
            dialog.present(Some(&window));
        });
    }

    load_built(None);

    // ─────────────────────────────────────────────────────────────────────────
    //  Download a release's tarball and signature
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let progress = progress.clone();
        let log_fn = log_fn.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let begin_busy = begin_busy.clone();
        let refresh_controls = refresh_controls.clone();
        let set_source = set_source.clone();

        download_btn.connect_clicked(move |_| {
            let rel = { state.borrow().selected_release.clone() };
            let Some(rel) = rel else { return };
            let mut files = vec![];
            for url in std::iter::once(&rel.source).chain(rel.pgp.as_ref()) {
                let Some(name) = url_file_name(url) else {
                    toast_overlay.add_toast(Toast::new("kernel.org listed an unusual download URL"));
                    return;
                };
                files.push(Remote { url: url.clone(), filename: name.to_string() });
            }
            let Some(busy) = begin_busy("download") else { return };

            progress.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_text(Some(&format!("Downloading Linux {}…", rel.version)));
            stack.set_visible_child_name("build");
            log_fn(format!("Downloading Linux {} from cdn.kernel.org…", rel.version));

            let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            state.borrow_mut().download_cancel = Some(cancel_flag.clone());
            refresh_controls();

            // Progress channel — sent from the Tokio side, drained on the GTK
            // main context; the loop exits when the sender drops.
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
            let download_start = std::time::Instant::now();
            {
                let progress = progress.clone();
                glib::MainContext::default().spawn_local(async move {
                    let mut last_ui = std::time::Instant::now();
                    while let Some(mut msg) = prog_rx.recv().await {
                        while let Ok(newer) = prog_rx.try_recv() {
                            msg = newer;
                        }
                        if last_ui.elapsed() < std::time::Duration::from_millis(100) {
                            continue;
                        }
                        last_ui = std::time::Instant::now();
                        let label_prefix = format!("[{}/{}] {}", msg.file_index + 1, msg.file_count, msg.filename);
                        if let Some(t) = msg.total {
                            if t > 0 {
                                progress.set_fraction(msg.downloaded as f64 / t as f64);
                            }
                            let elapsed = download_start.elapsed().as_secs_f64();
                            let speed = if elapsed > 0.0 { msg.downloaded as f64 / elapsed } else { 0.0 };
                            progress.set_text(Some(&format!(
                                "{}  {:.1} / {:.1} MB ({:.1} MB/s)",
                                label_prefix,
                                msg.downloaded as f64 / 1_000_000.0,
                                t as f64 / 1_000_000.0,
                                speed / 1_000_000.0,
                            )));
                        } else {
                            progress.set_text(Some(&label_prefix));
                            progress.pulse();
                        }
                    }
                });
            }

            let progress = progress.clone();
            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let set_source = set_source.clone();
            spawn_async(
                download_files(
                    files,
                    build::downloads_root(),
                    move |p| {
                        let _ = prog_tx.send(p);
                    },
                    cancel_flag,
                ),
                move |result| {
                    drop(busy);
                    match result {
                        Err(e) => {
                            if is_cancelled(&e) {
                                log_fn("Download cancelled.".to_string());
                                progress.set_text(Some("Download cancelled"));
                            } else {
                                log_fn(format!("Download failed: {e:#}"));
                                progress.set_text(Some("Download failed"));
                                toast_overlay.add_toast(Toast::new("Download failed — see Log tab"));
                            }
                        }
                        Ok(paths) => {
                            log_fn(format!("Downloaded {}", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
                            progress.set_visible(false);
                            if let Some(tarball) = paths.first() {
                                set_source(tarball.clone());
                            }
                        }
                    }
                },
            );
        });
    }

    // Cancel a download or a build
    {
        let state = state.clone();
        let log_fn = log_fn.clone();
        cancel_btn.connect_clicked(move |_| {
            let s = state.borrow();
            if let Some(flag) = &s.download_cancel {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(control) = &s.build_control {
                log_fn("Cancelling the build…".to_string());
                control.cancel();
            }
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Build: prepare (extract + configure), show the summary, build
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let window = window.clone();
        let log_fn = log_fn.clone();
        let toast_overlay = toast_overlay.clone();
        let begin_busy = begin_busy.clone();
        let refresh_controls = refresh_controls.clone();
        let stage_label = stage_label.clone();
        let progress = progress.clone();
        let event_sink = event_sink.clone();
        let load_built = load_built.clone();
        let stack = stack.clone();
        let refresh_requirements = refresh_requirements.clone();

        build_btn.connect_clicked(move |_| {
            let req = {
                let s = state.borrow();
                let (Some(tarball), Some(trust), Some(mode)) = (s.source.clone(), s.source_trust(), s.config_mode()) else { return };
                if !s.missing_deps.is_empty() {
                    return;
                }
                BuildRequest { tarball, trust, mode, running_kernel: s.sysinfo.running_kernel.clone() }
            };
            let boot_need = state.borrow().sysinfo.boot_needed_bytes;
            let tree_log = build::tree_dir(&req.tarball.version).join("build.log");
            let Some(busy) = begin_busy("build") else { return };
            let control = BuildControl::new();
            {
                let mut s = state.borrow_mut();
                s.build_control = Some(control.clone());
                s.build_started = Some(std::time::Instant::now());
            }
            refresh_controls();
            log_fn(format!("Preparing the build of Linux {}…", req.tarball.version.kernelversion()));
            stage_label.set_label("Preparing");
            stage_label.set_visible(true);
            progress.set_visible(true);
            progress.set_fraction(0.0);

            // Elapsed time and a pulsing bar while the build runs.
            {
                let state = state.clone();
                let progress = progress.clone();
                // Runs while the build holds the busy state; between the
                // configure step and the yes to the summary it just waits.
                glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                    let s = state.borrow();
                    if s.build_control.is_none() {
                        return glib::ControlFlow::Break;
                    }
                    match s.build_started {
                        Some(start) => {
                            let secs = start.elapsed().as_secs();
                            progress.set_text(Some(&format!("{:02}:{:02}:{:02} elapsed", secs / 3600, secs / 60 % 60, secs % 60)));
                            progress.pulse();
                        }
                        None => progress.set_text(Some("Waiting for you to confirm the configuration")),
                    }
                    glib::ControlFlow::Continue
                });
            }

            let busy_slot: Rc<RefCell<Option<BusyGuard>>> = Rc::new(RefCell::new(Some(busy)));
            let finish = {
                let busy_slot = busy_slot.clone();
                let progress = progress.clone();
                let stage_label = stage_label.clone();
                let refresh_requirements = refresh_requirements.clone();
                move |label: &str| {
                    busy_slot.borrow_mut().take();
                    stage_label.set_label(label);
                    progress.set_visible(false);
                    refresh_requirements();
                }
            };

            let window = window.clone();
            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let event_sink = event_sink.clone();
            let load_built = load_built.clone();
            let stack = stack.clone();
            let state = state.clone();
            let sink = event_sink.clone();
            let c2 = control.clone();
            spawn_async(async move { tokio::task::spawn_blocking(move || build::prepare(req, sink, c2, boot_need)).await }, move |result| {
                let prepared: Prepared = match result {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => {
                        if is_build_cancelled(&e) {
                            log_fn("Build cancelled.".to_string());
                            finish("Build cancelled");
                        } else {
                            log_fn(format!("Preparing the build failed: {e:#}"));
                            if tree_log.exists() {
                                log_fn(format!("The output is in {}", tree_log.display()));
                            }
                            toast_overlay.add_toast(Toast::new("Preparing the build failed — see Log tab"));
                            finish("Preparing the build failed");
                        }
                        return;
                    }
                    Err(e) => {
                        log_fn(format!("Task error: {e}"));
                        finish("Build failed");
                        return;
                    }
                };
                // Show what will be built and wait for a yes.
                state.borrow_mut().build_started = None;
                let dialog = AlertDialog::builder().heading(format!("Build Linux {}?", prepared.summary.kernelrelease)).body(prepared.summary.text()).build();
                dialog.add_responses(&[("cancel", "Cancel"), ("build", "Build")]);
                dialog.set_response_appearance("build", libadwaita::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("build"));
                dialog.set_close_response("cancel");
                let prepared_slot = Rc::new(RefCell::new(Some(prepared)));
                let finish = Rc::new(finish);
                dialog.connect_response(None, move |_, resp| {
                    let Some(prepared) = prepared_slot.borrow_mut().take() else { return };
                    if resp != "build" {
                        log_fn("Build not started; the configured tree is deleted.".to_string());
                        spawn_async(async move { tokio::task::spawn_blocking(move || prepared.discard()).await }, |_| {});
                        finish("");
                        return;
                    }
                    let krel = prepared.summary.kernelrelease.clone();
                    log_fn(format!("Building Linux {krel} — this takes a while; the full output goes to build.log"));
                    state.borrow_mut().build_started = Some(std::time::Instant::now());
                    let state = state.clone();
                    let log_fn = log_fn.clone();
                    let toast_overlay = toast_overlay.clone();
                    let load_built = load_built.clone();
                    let stack = stack.clone();
                    let finish = finish.clone();
                    let tree_log = tree_log.clone();
                    let sink = event_sink.clone();
                    let control = control.clone();
                    spawn_async(async move { tokio::task::spawn_blocking(move || build::build(prepared, sink, control)).await }, move |result| {
                        let elapsed = state.borrow().build_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                        let took = format!("{}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60);
                        match result {
                            Ok(Ok(set)) => {
                                log_fn(format!("Built Linux {} in {took}: {}", set.kernelrelease, set.dir.display()));
                                toast_overlay.add_toast(Toast::new("Kernel built — ready to install"));
                                finish(&format!("Built in {took}"));
                                load_built(Some(set.dir.clone()));
                                stack.set_visible_child_name("install");
                            }
                            Ok(Err(e)) if is_build_cancelled(&e) => {
                                log_fn("Build cancelled; its files were removed.".to_string());
                                finish("Build cancelled");
                            }
                            Ok(Err(e)) => {
                                log_fn(format!("Build failed after {took}: {e:#}"));
                                log_fn(format!("The full output is in {}", tree_log.display()));
                                toast_overlay.add_toast(Toast::new("Build failed — see Log tab"));
                                finish("Build failed");
                            }
                            Err(e) => {
                                log_fn(format!("Task error: {e}"));
                                finish("Build failed");
                            }
                        }
                    });
                });
                dialog.present(Some(&window));
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Install
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let log_fn = log_fn.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let progress = install_progress.clone();
        let load_sysinfo = load_sysinfo.clone();
        let window = window.clone();
        let preflight_state = state.clone();
        let blocker_stack = stack.clone();
        let begin_busy = begin_busy.clone();

        let start_install: Rc<dyn Fn()> = Rc::new(move || {
            let dir = {
                let s = state.borrow();
                if !s.staged_verified {
                    return;
                }
                s.staged.as_ref().map(|set| set.dir.clone())
            };
            let Some(dir) = dir else { return };
            let Some(busy) = begin_busy("kernel install") else { return };
            let dir_str = dir.display().to_string();

            progress.set_visible(true);
            progress.set_text(Some("Installing… initramfs is generated and verified before success"));
            stack.set_visible_child_name("log");
            log_fn(format!("Starting kernel install from {}", dir_str));

            {
                let progress = progress.clone();
                let state = state.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
                    if state.borrow().busy.is_none() {
                        glib::ControlFlow::Break
                    } else {
                        progress.pulse();
                        glib::ControlFlow::Continue
                    }
                });
            }

            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let progress = progress.clone();
            let load_sysinfo = load_sysinfo.clone();

            spawn_async(async move { tokio::task::spawn_blocking(move || run_privileged_install(&dir_str)).await }, move |result| {
                drop(busy);
                progress.set_visible(false);
                match result {
                    Ok(Ok(out)) => {
                        // DKMS and signing failures never fail the
                        // install, and neither does a kernel that will
                        // not boot under Secure Boot, but none of them
                        // may go unnoticed before a reboot.
                        let msg = report_run(&log_fn, &out, Done::Install);
                        log_fn("Install completed — initramfs verified in /boot.".to_string());
                        toast_overlay.add_toast(Toast::new(&msg));
                        load_sysinfo();
                    }
                    Ok(Err(e)) => {
                        log_fn(format!("Install failed: {}", e));
                        toast_overlay.add_toast(Toast::new("Install failed — see Log tab"));
                    }
                    Err(e) => log_fn(format!("Task error: {}", e)),
                }
            });
        });

        // Preflight: Secure Boot and /boot space need an explicit "install
        // anyway" before the install starts.
        // A kernel that could not boot under Secure Boot is not installed at
        // all: the dialog explains why and leads to Set Up Signing.
        install_btn.connect_clicked(move |_| {
            let blocker = install_blocker(&preflight_state.borrow().sysinfo);
            if let Some(why) = blocker {
                let dialog = AlertDialog::builder().heading("This kernel could not boot").body(why).build();
                dialog.add_responses(&[("close", "Close"), ("system", "Go to System Tab")]);
                dialog.set_response_appearance("system", libadwaita::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("system"));
                dialog.set_close_response("close");
                let stack = blocker_stack.clone();
                dialog.connect_response(None, move |_, resp| {
                    if resp == "system" {
                        stack.set_visible_child_name("system");
                    }
                });
                dialog.present(Some(&window));
                return;
            }
            let mut warnings = install_warnings(&preflight_state.borrow().sysinfo);
            if let Some(f) = preflight_state.borrow().sysinfo.free_root_bytes {
                if f < MIN_ROOT_BYTES {
                    warnings.push(format!("/ has only {} free; the modules and initramfs need room too.", format_bytes(f)));
                }
            }
            if preflight_state.borrow().staged.as_ref().is_some_and(|s| s.source.starts_with("UNVERIFIED")) {
                warnings.push("This kernel was built from source that was not verified against a kernel.org signature.".to_string());
            }
            if warnings.is_empty() {
                start_install();
                return;
            }
            let dialog = AlertDialog::builder().heading("Before you install").body(warnings.join("\n\n")).build();
            dialog.add_responses(&[("cancel", "Cancel"), ("install", "Install Anyway")]);
            dialog.set_response_appearance("install", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let start_install = start_install.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "install" {
                    start_install();
                }
            });
            dialog.present(Some(&window));
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  About dialog
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let state = state.clone();
        about_btn.connect_clicked(move |_| {
            let (running, count) = {
                let s = state.borrow();
                (s.sysinfo.running_kernel.clone(), s.sysinfo.kernels.len())
            };
            let dialog = AboutDialog::builder()
                .application_name("Kernel Pop")
                .version(env!("CARGO_PKG_VERSION"))
                .developers(vec!["Linnard Alex Brown Jr."])
                .license_type(gtk4::License::Agpl30)
                .website("https://github.com/labj1987/kernel-pop")
                .issue_url("https://github.com/labj1987/kernel-pop/issues")
                .comments(format!(
                    "GTK4 + Rust GUI that builds kernel.org kernels for this machine and installs them with a verified initramfs.\n\nRunning kernel: {}\nInstalled kernels: {}",
                    running, count
                ))
                .build();
            dialog.add_credit_section(Some("Built with"), &["Claude Code (Anthropic)", "Codex (OpenAI)"]);
            dialog.present(Some(&window));
        });
    }

    // Clear log
    {
        let lv = log_view.clone();
        clear_btn.connect_clicked(move |_| {
            lv.buffer().set_text("");
        });
    }

    // Closing the window stops a running build first: its process group
    // would otherwise keep compiling with nobody to collect the packages.
    // The window closes once the build has stopped and cleaned up.
    {
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        window.connect_close_request(move |w| {
            let Some(control) = state.borrow().build_control.clone() else {
                return glib::Propagation::Proceed;
            };
            if !control.is_cancelled() {
                control.cancel();
                toast_overlay.add_toast(Toast::new("Stopping the build before closing…"));
            }
            let state = state.clone();
            let w = w.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                if state.borrow().build_control.is_some() {
                    return glib::ControlFlow::Continue;
                }
                w.destroy();
                glib::ControlFlow::Break
            });
            glib::Propagation::Stop
        });
    }

    window.present();
}
