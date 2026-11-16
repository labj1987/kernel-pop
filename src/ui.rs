use crate::build::{
    self, built_sets, free_bytes, is_build_cancelled, missing_build_deps, space_problem, verify_built_set, BuildControl, BuildEvent, BuildRequest, BuiltSet,
    ConfigMode, EventSink, Prepared, SourceTrust, Stage,
};
use crate::download::{download_files, is_cancelled, remove_stale_parts, Progress as DownloadProgress, Remote};
use crate::install::{
    helper_problem, run_privileged_boot_once, run_privileged_install, run_privileged_install_build_deps, run_privileged_remove, run_privileged_remove_many,
    run_privileged_setup_signing, run_privileged_sign, validate_mok_password, BootOutlook, ScriptReport,
};
use crate::instance::Activations;
use crate::kernelorg::{fetch_releases, fetch_signature, url_file_name, Moniker, Release};
use crate::source::{inspect, private_copy, Tarball};
use crate::system::{
    compare_to_running, format_bytes, health_banner_text, install_blocker, install_warnings, prune_candidates, query_system, BootEntry, BootPath,
    KernelSignature, KernelSigning, KeyEnrollment, SecureBoot, SystemInfo, VersionRelation, MIN_ROOT_BYTES,
};
use crate::verify::{verify_tarball, Signer};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::link::Link;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Root, Sizable as _, Theme, ThemeMode, ThemeRegistry, TitleBar, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    accesskit, div, img, point, px, size, Anchor, AnyElement, App, AppContext as _, AssetSource, Bounds, Context, Div, Element, ElementId, Entity,
    ExternalPaths, FocusHandle, Focusable as _, FontWeight, GlobalElementId, Hsla, Image, ImageFormat, InspectorElementId, InteractiveElement as _,
    IntoElement, LayoutId, MouseButton, ParentElement as _, PathPromptOptions, Pixels, Position, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Style, Styled as _, Subscription, Text, Window, WindowBounds, WindowDecorations, WindowOptions,
};

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WEBSITE: &str = "https://github.com/labj1987/kernel-pop";
const ISSUES: &str = "https://github.com/labj1987/kernel-pop/issues";

// ─────────────────────────────────────────────────────────────────────────────
//  Assets: GPUI Kit's component icons plus the one this app adds.
// ─────────────────────────────────────────────────────────────────────────────

gpui_kit::assets::icon_assets!(ExtraIcons, [RefreshCw]);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  App state
// ─────────────────────────────────────────────────────────────────────────────

/// The busy states that run the privileged script: the window is not closed under them.
const PRIVILEGED_ACTIONS: &[&str] = &["signing", "boot selection", "kernel removal", "signing setup", "build tools install", "kernel install"];

/// What closing the window does, from what is running.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CloseDecision {
    /// A privileged action runs: closing would cut the script off halfway.
    Refuse(&'static str),
    /// A build (or its preparation) runs: stop it, then close.
    StopBuildFirst,
    /// A source download runs: stop it (which deletes its `.part` file), then close.
    StopDownloadFirst,
    Close,
}

fn close_decision(busy: Option<&'static str>, build_running: bool, download_running: bool) -> CloseDecision {
    match busy {
        Some(what) if PRIVILEGED_ACTIONS.contains(&what) => CloseDecision::Refuse(what),
        _ if build_running => CloseDecision::StopBuildFirst,
        _ if download_running => CloseDecision::StopDownloadFirst,
        _ => CloseDecision::Close,
    }
}

/// How long closing waits for a cancelled download to stop and delete its
/// `.part` file before deleting it itself and closing anyway.
const DOWNLOAD_STOP_WAIT: Duration = Duration::from_secs(5);

/// The busy state of one download, build or privileged action. It lasts as
/// long as this guard: it is moved into the action's completion callback, so
/// it is released even when that callback never runs (a panicking task drops
/// the callback, and with it the guard).
struct BusyGuard {
    state: Rc<RefCell<AppState>>,
    changed: Rc<Cell<bool>>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.busy = None;
            s.download_cancel = None;
            s.build_control = None;
            s.build_started = None;
        }
        // The window is redrawn on the next tick of the message loop.
        self.changed.set(true);
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
    build_started: Option<Instant>,
    /// What is running right now, if anything: one download, build or
    /// privileged action at a time. While set, every control that would
    /// start another one is disabled (see `MainView::usable`).
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

/// Messages from worker threads to the UI thread: log lines (batched into
/// the Log tab) and build stages.
enum UiMsg {
    Log(String),
    Stage(Stage),
    Progress(f64),
}

/// The Log tab keeps this many lines; the full build output is in build.log.
const LOG_MAX_LINES: usize = 6000;

/// How often worker messages are taken in and the log redrawn.
const UI_TICK: Duration = Duration::from_millis(150);

/// Show what the privileged script printed, line by line, in the Log tab.
fn log_output(log: &mut impl FnMut(String), output: &str) {
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        log(line.to_string());
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
        KernelSigning::KeyNotEnrolled => "Key created but not enrolled — set up again to queue the enrollment",
        KernelSigning::EnrollmentPending => "Enrollment pending — confirm it at the blue MOK Manager screen on the next reboot",
        KernelSigning::Enrolled => "Set up — every kernel Kernel Pop installs is signed with an enrolled key",
        KernelSigning::EfiVarsNotMounted => {
            "Key present; the EFI variables are not mounted, so its enrollment cannot be read. Mount them (sudo mount -t efivarfs efivarfs /sys/firmware/efi/efivars) or reboot"
        }
        KernelSigning::Unknown => "Key present; could not read its enrollment state (mokutil missing?)",
    }
}

/// The Kernel signing row's subtitle: the signing state plus the DKMS key's.
fn signing_text(info: &SystemInfo) -> String {
    let dkms_note = match info.dkms_key {
        Some(KeyEnrollment::NotEnrolled) => " The DKMS key (NVIDIA driver and other out-of-tree modules) is not enrolled; Set Up Signing enrolls it too.",
        Some(KeyEnrollment::Pending) => " The DKMS key (NVIDIA driver) is waiting for enrollment too.",
        _ => "",
    };
    let base = signing_subtitle(info.kernel_signing);
    let sep = if dkms_note.is_empty() || base.ends_with('.') { "" } else { "." };
    format!("{base}{sep}{dkms_note}")
}

fn offers_signing_setup(info: &SystemInfo) -> bool {
    matches!(info.kernel_signing, KernelSigning::NoKey | KernelSigning::KeyNotEnrolled)
        || info.dkms_key == Some(KeyEnrollment::NotEnrolled)
        || info.boot_path == BootPath::DirectSystemdBoot
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
fn report_run(log: &mut impl FnMut(String), out: &str, done: Done) -> String {
    log_output(log, out);
    let r = ScriptReport::parse(out);
    if r.dkms_problems > 0 {
        log(format!(
            "WARNING: {} DKMS problem(s) — out-of-tree modules such as the NVIDIA driver may be missing after reboot. See the DKMS lines above.",
            r.dkms_problems
        ));
    }
    if r.sign_problems > 0 {
        log(format!("WARNING: {} signing problem(s) — a kernel or its boot loader copy may be unsigned. See the SIGN lines above.", r.sign_problems));
    }
    outcome_message(done, &r)
}

/// The Browse list's line under a release.
fn release_subtitle(rel: &Release, running: &str) -> String {
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
    parts.join(" · ")
}

/// Whether a release matches the Browse tab's filter.
fn release_matches(rel: &Release, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty() || rel.version.contains(&filter) || rel.moniker.label().contains(&filter)
}

/// The health line of an installed kernel.
fn kernel_subtitle(k: &crate::system::InstalledKernel) -> String {
    let parts = [
        if k.has_initrd { "initrd ✓" } else { "INITRD MISSING" },
        if k.has_modules { "modules ✓" } else { "modules missing" },
        match k.boot_entry {
            BootEntry::Present => "boot menu ✓",
            BootEntry::Missing => "NOT IN BOOT MENU",
            // How kernelstub works, not a fault.
            BootEntry::NotKept => "not in the boot menu (kernelstub keeps only the current and previous kernel)",
        },
    ];
    parts.join(" · ")
}

/// A build's elapsed time as H:MM:SS.
fn took(elapsed: u64) -> String {
    format!("{}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60)
}

/// Where the build is, 0.0 to 1.0, after a stage starts: extracting 0%,
/// configuring 2%, compiling 5-95% by objects compiled (None while the
/// object count is unknown: the bar runs), packaging 95%.
fn stage_fraction(stage: Stage) -> Option<f64> {
    match stage {
        Stage::Extract => Some(0.0),
        Stage::Configure => Some(0.02),
        Stage::Build => None,
        Stage::Package => Some(0.95),
    }
}

/// The fraction after a compile-progress report; a late compile line never
/// moves the bar back from packaging.
fn progress_fraction(current: Option<f64>, compiled: f64) -> Option<f64> {
    if current.is_none_or(|b| b < 0.95) {
        Some(0.05 + 0.90 * compiled.clamp(0.0, 1.0))
    } else {
        current
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Notifications
// ─────────────────────────────────────────────────────────────────────────────

/// A message this long stays until it is closed; shorter ones go away on their own.
const LONG_TOAST: usize = 120;

// Notifications show their text as plain text, so paths, user IDs
// ("Name <a@kernel.org>") and error text need no escaping.
fn toast(window: &mut Window, cx: &mut App, text: &str) {
    show_toast(window, cx, text);
}

/// False when the message could not be shown: a window has no place for
/// notifications until the toolkit's root view is in it, which is after the
/// main view is built (pushing one earlier panics in the toolkit).
fn show_toast(window: &mut Window, cx: &mut App, text: &str) -> bool {
    if window.root::<Root>().flatten().is_none() {
        eprintln!("kernel-pop: not shown (the window is not up yet): {text}");
        return false;
    }
    window.push_notification(Notification::new().message(text.to_string()).placement(Anchor::BottomCenter).autohide(text.chars().count() <= LONG_TOAST), cx);
    announce(text);
    window.refresh();
    true
}

// ─────────────────────────────────────────────────────────────────────────────
//  Screen readers. GPUI reports an element to assistive technology only when
//  it has an id and a role, so the text, dialog names and announcements below
//  are spelled out; a bare string child is drawn but never read.
// ─────────────────────────────────────────────────────────────────────────────

/// Text a screen reader can read (a label node holding the text).
fn label(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Text {
    Text::new(id.into(), text.into())
}

/// GPUI Kit 0.9.1 gives a dialog no accessible name and has no way to set
/// one, so everything in a dialog, its buttons included, sits in a group
/// named after the title: a screen reader says the name when the keyboard
/// focus moves into the group, which is when the dialog opens.
fn named_group(id: &'static str, name: impl Into<SharedString>) -> Stateful<Div> {
    v_flex().id(id).role(Role::Group).aria_label(name).gap_3()
}

// Every dialog is opened with `.overlay(false)`: the toolkit's dimming layer
// (0.9.1) also covers the invisible margin a client-decorated window keeps
// around itself for its shadow, which shows as a dark frame around the
// window on KDE and GNOME. Without it a dialog is as modal as with it (it
// takes the input and the focus); gone are the dimming, click-outside-to-close
// and dragging the window by its title bar or the dimmed area, for which the
// dialog's own title stands in (`dialog_title`).

/// Puts the keyboard focus on a dialog's main button when the dialog opens.
/// Left alone the focus sits on the dialog itself, which has no name; on a
/// button it is inside the named group, so a screen reader announces the
/// title, and Space acts on the button straight away.
struct InitialFocus {
    handle: FocusHandle,
    pending: Cell<bool>,
}

impl InitialFocus {
    fn new(cx: &mut App) -> Rc<Self> {
        Rc::new(Self { handle: cx.focus_handle(), pending: Cell::new(true) })
    }

    /// Call from the dialog's builder, which runs on every frame: only the
    /// first call moves the focus.
    fn apply(&self, window: &mut Window, cx: &mut App) {
        if self.pending.replace(false) {
            self.handle.focus(window, cx);
        }
    }
}

/// The height a dialog's text may take before it scrolls. The toolkit cuts
/// off a dialog that is taller than the window instead of scrolling it
/// (0.9.1), which would take the buttons out of reach, so the part above
/// the buttons gets what the window leaves after the dialog's margin, title
/// and button row.
fn dialog_room(window: &Window) -> Pixels {
    (window.viewport_size().height - px(DIALOG_CHROME)).max(px(60.))
}

/// What a dialog needs of the window's height besides its text.
const DIALOG_CHROME: f32 = 220.;

/// What Enter does in a dialog. The toolkit hands Enter to the dialog (its
/// "confirm") before the focused button sees it, so the dialog looks at
/// where the focus is and acts as that button would: Enter on Cancel
/// cancels, Enter on the action confirms, and Enter anywhere else (the close
/// button, a text field) is not a decision.
#[derive(Clone, Copy, Debug, PartialEq)]
enum EnterKey {
    Confirm,
    Cancel,
    Nothing,
}

fn enter_key(action_focused: bool, cancel_focused: bool) -> EnterKey {
    if action_focused {
        EnterKey::Confirm
    } else if cancel_focused {
        EnterKey::Cancel
    } else {
        EnterKey::Nothing
    }
}

/// Which button of a question has the focus when it opens, and so which one
/// Enter and Space act on if the user just presses them.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DefaultButton {
    Action,
    Cancel,
}

/// A dialog's title, which is also the handle to move the window by: while a
/// dialog is open the toolkit keeps the pointer off everything under it, the
/// window's own title bar included.
fn dialog_title(title: impl Into<SharedString>) -> impl IntoElement {
    div().w_full().on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()).child(label("title", title))
}

/// The button row at the bottom of a dialog.
fn dialog_buttons() -> Div {
    h_flex().justify_end().gap_2().pt_1()
}

thread_local! {
    /// The newest notification, for `Announcer`.
    static ANNOUNCEMENT: RefCell<SharedString> = RefCell::default();
}

/// Has the screen reader read `text` out (through the main window's `Announcer`).
fn announce(text: &str) {
    ANNOUNCEMENT.with(|current| {
        let mut current = current.borrow_mut();
        // The announcement is the change of the node's name, so the same
        // message twice in a row needs a name that differs: an invisible
        // zero-width space is added or dropped.
        *current = if current.as_ref() == text { format!("{text}\u{200b}").into() } else { text.to_string().into() };
    });
}

/// A live region: an invisible node whose accessible name is the newest
/// notification, marked "polite" so a screen reader reads each change out
/// once it has finished speaking. GPUI Kit's own notifications are not
/// announced (no live regions in 0.9.1).
struct Announcer(SharedString);

impl IntoElement for Announcer {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Announcer {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("announcer".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(Role::Status)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        node.set_live(accesskit::Live::Polite);
        if !self.0.is_empty() {
            node.set_label(self.0.to_string());
        }
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        // Out of the flow and with no size: it takes no room in the window.
        let style = Style { position: Position::Absolute, ..Style::default() };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut Window, _: &mut App) {}

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), _: &mut Window, _: &mut App) {}
}

// ─────────────────────────────────────────────────────────────────────────────
//  Questions: Cancel and one action
// ─────────────────────────────────────────────────────────────────────────────

type Continuation = Box<dyn FnOnce(&mut Window, &mut App)>;

/// A question's two outcomes, of which at most one runs.
type Outcomes = Rc<RefCell<Option<(Continuation, Option<Continuation>)>>>;

/// What the signing dialog does with a good password.
type Submit = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// A question with Cancel (or Close) and one action. `on_ok` runs at most
/// once, after the dialog is out of the way; cancelling it any way (the
/// button, Escape, the close button) runs `on_cancel` instead, when there is
/// one.
struct Question {
    id: &'static str,
    title: SharedString,
    body: SharedString,
    ok_text: &'static str,
    cancel_text: &'static str,
    default: DefaultButton,
    /// The action removes or risks something: drawn as a danger button.
    destructive: bool,
}

fn show_question(question: Question, on_ok: Continuation, on_cancel: Option<Continuation>, window: &mut Window, cx: &mut App) {
    let Question { id, title, body, ok_text, cancel_text, default, destructive } = question;
    let slot: Outcomes = Rc::new(RefCell::new(Some((on_ok, on_cancel))));
    let action_focus = cx.focus_handle();
    let cancel_focus = cx.focus_handle();
    let first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            match default {
                DefaultButton::Action => action_focus.focus(window, cx),
                DefaultButton::Cancel => cancel_focus.focus(window, cx),
            }
        }
        let confirm = {
            let slot = slot.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some((ok, _)) = slot.borrow_mut().take() {
                    window.defer(cx, move |window, cx| ok(window, cx));
                }
            }
        };
        let cancel = {
            let slot = slot.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some((_, Some(cancel))) = slot.borrow_mut().take() {
                    window.defer(cx, move |window, cx| cancel(window, cx));
                }
            }
        };
        let (enter_ok, enter_cancel, button_cancel, dismissed) = (confirm.clone(), cancel.clone(), cancel.clone(), cancel);
        let (on_action, on_cancel) = (action_focus.clone(), cancel_focus.clone());
        let ok_button = Button::new("ok").label(ok_text).track_focus(&action_focus).on_click(move |_, window, cx| {
            window.close_dialog(cx);
            confirm(window, cx);
        });
        let ok_button = if destructive { ok_button.danger() } else { ok_button.primary() };
        dialog
            .title(dialog_title(title.clone()))
            .overlay(false)
            .w(px(440.))
            // Enter: what the focused button says (see `EnterKey`).
            .on_ok(move |_, window, cx| {
                match enter_key(on_action.is_focused(window), on_cancel.is_focused(window)) {
                    EnterKey::Confirm => enter_ok(window, cx),
                    EnterKey::Cancel => enter_cancel(window, cx),
                    EnterKey::Nothing => return false,
                }
                true
            })
            // Escape and the close button; after a confirm there is nothing left to run.
            .on_close(move |_, window, cx| dismissed(window, cx))
            .child(
                named_group(id, title.clone()).child(div().id("body").max_h(dialog_room(window)).overflow_y_scroll().child(label("text", body.clone()))).child(
                    dialog_buttons()
                        .child(Button::new("cancel").label(cancel_text).track_focus(&cancel_focus).on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            button_cancel(window, cx);
                        }))
                        .child(ok_button),
                ),
            )
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  No window at all
// ─────────────────────────────────────────────────────────────────────────────

/// The window is drawn with the GPU, so with no usable Vulkan or OpenGL ES
/// driver (not even Mesa's software ones) there is nothing to draw an error
/// in. Says so through whatever the desktop offers, then exits: a process
/// left behind with no window would also keep later launches from starting.
fn no_window(error: &str) -> ! {
    let message = no_window_message(error, crate::notice::has_display());
    eprintln!("kernel-pop: {message}");
    // With no display there is nowhere to show a dialog either.
    crate::notice::show_error(&message);
    std::process::exit(1);
}

/// The System tab's note that a newer stable kernel is out. Where Kernel Pop
/// can't build kernels (not Debian or Ubuntu) the Browse tab's Download button
/// is off, so the note does not send anyone there.
fn update_banner_text(version: &str, unsupported_system: bool) -> String {
    if unsupported_system {
        format!("Linux {version} (stable) is out")
    } else {
        format!("Linux {version} (stable) is out — download it on the Browse tab to build it")
    }
}

fn no_window_message(error: &str, has_display: bool) -> String {
    if has_display {
        format!(
            "Kernel Pop could not open its window: no working graphics driver was found \
             (Vulkan or OpenGL ES). Install or repair your GPU driver, or install Mesa's \
             software renderer (mesa-vulkan-drivers on Debian and Ubuntu).\n\nDetails: {error}"
        )
    } else {
        format!(
            "Kernel Pop could not open its window: there is no display to open it on \
             (neither WAYLAND_DISPLAY nor DISPLAY is set). Start it from your desktop session.\
             \n\nDetails: {error}"
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

/// The families tried for the UI font, in the order GPUI itself falls back to them.
const UI_FONTS: [&str; 9] = ["Ubuntu", "Adwaita Sans", "Cantarell", "Noto Sans", "DejaVu Sans", "Liberation Sans", "Arial", "Helvetica", "FreeSans"];

/// The families tried for fixed-width text (the Log tab), the toolkit's own default first.
const MONO_FONTS: [&str; 10] = [
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "Ubuntu Sans Mono",
    "Adwaita Mono",
    "Source Code Pro",
    "Hack",
    "FreeMono",
    "Nimbus Mono PS",
];

/// The first of `families` that is installed.
fn first_installed(families: &[&'static str], installed: &[String]) -> Option<&'static str> {
    families.iter().copied().find(|family| installed.iter().any(|name| name == family))
}

/// Starts the toolkit, with installed families as its fonts. Left alone, the
/// theme asks for GPUI's "system UI font", a family no Linux desktop ships,
/// and for a fixed-width family that many lack; GPUI and the toolkit then
/// log warnings and fall back to the first family they find. Naming those
/// up front gives the same text and no warnings.
pub fn init(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let ui_font = first_installed(&UI_FONTS, &installed);
    if let Some(ui_font) = ui_font {
        // The toolkit keeps a theme that is already there, and families set
        // on it are used as they are.
        let mut theme = Theme::default();
        theme.font_family = ui_font.into();
        theme.mono_font_family = first_installed(&MONO_FONTS, &installed).unwrap_or(ui_font).into();
        cx.set_global(theme);
    }
    gpui_kit::init(cx);
    if ui_font.is_some() {
        // What the toolkit does for a theme it creates itself: its light and
        // dark colours, then the mode.
        let registry = ThemeRegistry::global(cx);
        let light = registry.default_light_theme().clone();
        let dark = registry.default_dark_theme().clone();
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
        Theme::change(ThemeMode::Light, None, cx);
    }
}

/// The size the window asks for: room for every tab's content, but never
/// more than the display's usable area (`usable`, when GPUI knows it: on
/// Wayland, the smallest output) less
/// room for the desktop's panels: on a 1280x800 screen with a 44 px KDE panel
/// a full-height window would put the bottom buttons (Download Source, Clear
/// Log) under the panel. Each tab scrolls its content above those buttons,
/// so a shorter window still reaches them.
fn initial_window_size(usable: Option<gpui_kit::Size<Pixels>>) -> gpui_kit::Size<Pixels> {
    const WANT: (f32, f32) = (860., 820.);
    const MIN: (f32, f32) = (WINDOW_MIN.0, WINDOW_MIN.1);
    let Some(usable) = usable else { return size(px(WANT.0), px(WANT.1)) };
    let (w, h) = (usable.width.as_f32(), usable.height.as_f32());
    // Panels are usually on the top or bottom edge; GPUI reports the whole
    // display on Linux, so the margin is what keeps clear of them.
    let width = WANT.0.min(w - 48.).max(MIN.0);
    let height = WANT.1.min(h - (h * 0.12).max(96.)).max(MIN.1);
    size(px(width), px(height))
}

/// The smallest window size the layout works in.
const WINDOW_MIN: (f32, f32) = (520., 420.);

/// Light or dark, as the desktop prefers, with radio buttons that can be seen.
fn follow_appearance(window: &mut Window, cx: &mut App) {
    Theme::sync_system_appearance(Some(window), cx);
    let theme = Theme::global_mut(cx);
    theme.input = input_border(theme.is_dark());
}

/// The colour of an unselected radio button's ring (and of checkbox and text
/// field borders: the theme's `input` colour). The toolkit's own is barely
/// different from the background (#2f2f2f on #0a0a0a in dark, #e5e5e5 on
/// white in light), so an unselected choice on the Build and Install tabs
/// looked like no choice at all. These keep at least 3:1 contrast, what a
/// control's outline needs to be seen.
fn input_border(dark: bool) -> Hsla {
    gpui_kit::rgb(if dark { INPUT_BORDER_DARK } else { INPUT_BORDER_LIGHT }).into()
}

const INPUT_BORDER_DARK: u32 = 0x737373;
const INPUT_BORDER_LIGHT: u32 = 0x8a8a8a;

pub fn build_ui(cx: &mut App, mut activations: Activations) {
    // With no display GPUI would run on with no window and no word about it.
    if ["WAYLAND_DISPLAY", "DISPLAY"].into_iter().all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty())) {
        no_window("no display server to connect to");
    }
    // GPUI has no primary display on Wayland; the smallest output is then the
    // one the window must fit on wherever the compositor puts it.
    let usable = cx
        .primary_display()
        .map(|display| display.visible_bounds().size)
        .or_else(|| cx.displays().iter().map(|display| display.visible_bounds().size).min_by(|a, b| a.height.cmp(&b.height)));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(initial_window_size(usable), cx)),
        window_min_size: Some(size(px(WINDOW_MIN.0), px(WINDOW_MIN.1))),
        app_id: Some(crate::APP_ID.to_string()),
        // The app draws its own title bar, so it looks and works the same on
        // every desktop, including compositors with no server-side decorations.
        window_decorations: Some(WindowDecorations::Client),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("Kernel Pop");
        // Follow the desktop's light/dark preference, now and when it changes.
        follow_appearance(window, cx);
        window.observe_window_appearance(follow_appearance).detach();
        cx.new(|cx| MainView::new(window, cx))
    });
    let (window, view) = match opened {
        Ok(opened) => opened,
        Err(e) => no_window(&format!("{e:#}")),
    };
    // The first reads, now that the window can show what goes wrong with them.
    let _ = window.update(cx, |_, window, cx| {
        view.update(cx, |view, cx| view.start(window, cx));
    });

    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();

    // A second launch asks this one to show its window, with the activation
    // token the desktop gave that launch when there is one: on Wayland only
    // that raises the window (see activation.rs). Without a token, or off
    // Wayland, GPUI's own request is what there is; a Wayland compositor then
    // marks the window as wanting attention instead of raising it.
    cx.spawn(async move |cx| {
        // From here a later launch gets "ok" once its request is handled;
        // until now the listener told it this copy was still starting.
        activations.ui_is_up();
        while let Some(activation) = activations.recv().await {
            let _ = window.update(cx, |_, window, _| {
                let raised = activation.token.as_deref().is_some_and(|token| crate::activation::activate(window, token));
                if !raised {
                    window.activate_window();
                }
            });
            // The later launch waits for this; without it, it reports this copy as hung.
            activation.handled();
        }
    })
    .detach();
}

// ─────────────────────────────────────────────────────────────────────────────
//  The main window
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Page {
    System,
    Browse,
    Build,
    Install,
    Log,
}

const PAGES: [(Page, &str); 5] = [(Page::System, "System"), (Page::Browse, "Browse"), (Page::Build, "Build"), (Page::Install, "Install"), (Page::Log, "Log")];

/// The download's progress bar: how far (None: size unknown, the bar runs) and its text.
struct DownloadBar {
    fraction: Option<f32>,
    text: String,
}

pub struct MainView {
    state: Rc<RefCell<AppState>>,
    /// Set when something outside the view (a dropped busy guard) changed the
    /// state; the message loop redraws the window then.
    changed: Rc<Cell<bool>>,
    page: Page,

    // System tab
    running_title: String,
    sys_loaded: bool,
    update_banner: Option<String>,
    health_banner: Option<String>,
    helper_banner: Option<String>,

    // Browse tab
    releases_loading: bool,
    browse_error: Option<String>,
    selected_text: String,
    filter: Entity<InputState>,

    // Build tab
    deps_text: String,
    space_text: String,
    own_config_text: String,
    stage_text: Option<String>,
    download_bar: Option<DownloadBar>,
    build_fraction: Option<f64>,

    // Install tab
    installing: bool,

    // Log tab
    log_lines: VecDeque<String>,
    log_dirty: bool,
    log_view: Entity<TextareaState>,

    ui_tx: tokio::sync::mpsc::UnboundedSender<UiMsg>,
    /// Holds the keyboard focus until a control takes it, so Tab and the
    /// other keys work from the moment the window opens.
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter releases…"));
        let filter_changed = cx.subscribe_in(&filter, window, |_, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        let log_view = cx.new(|cx| TextareaState::new(window, cx));

        // Worker threads (signature check, build) report through this
        // channel; it is drained in batches so a fast build cannot flood the
        // window.
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiMsg>();
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(UI_TICK).await;
            let mut batch = Vec::new();
            while let Ok(m) = ui_rx.try_recv() {
                batch.push(m);
            }
            if this.update_in(cx, |this, window, cx| this.tick(batch, window, cx)).is_err() {
                break;
            }
        })
        .detach();

        // The privileged helper must be the one this build ships: AppRun
        // installs it at startup, and a cancelled or failed install leaves the
        // old one (or none) in place. install.rs refuses to run a mismatched one.
        // Not on a system Kernel Pop doesn't support: AppRun installs no helper there, and the
        // unsupported-system banner says it all (asked now, so it never flashes up first).
        let helper_banner = helper_problem().filter(|_| !crate::system::unsupported_system());

        let view = Self {
            state: Rc::new(RefCell::new(AppState::default())),
            changed: Rc::new(Cell::new(false)),
            page: Page::System,
            running_title: "Detecting…".into(),
            sys_loaded: false,
            update_banner: None,
            health_banner: None,
            helper_banner,
            releases_loading: false,
            browse_error: None,
            selected_text: "No release selected".into(),
            filter,
            deps_text: "Checking…".into(),
            space_text: "Checking…".into(),
            own_config_text: "No file chosen".into(),
            stage_text: None,
            download_bar: None,
            build_fraction: None,
            installing: false,
            log_lines: VecDeque::new(),
            log_dirty: false,
            log_view,
            ui_tx,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![filter_changed],
        };
        view.focus_handle.focus(window, cx);
        view
    }

    /// The first reads, and the close guard; from `build_ui` once the window is up.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| this.update(cx, |this, cx| this.may_close(window, cx)).unwrap_or(true));
        // Nothing downloads yet, so a `.part` file is one an earlier run left
        // (killed, or crashed, mid-download).
        for part in remove_stale_parts(&build::downloads_root()) {
            self.log(format!("Removed {}, left by an unfinished download", part.display()));
        }
        self.load_sysinfo(window, cx);
        self.load_releases(window, cx);
        self.refresh_requirements(window, cx);
        self.load_built(None, window, cx);
    }

    // ── Plumbing ────────────────────────────────────────────────────────────

    /// Runs `future` on the shared Tokio runtime and hands its result to
    /// `done` on the UI thread. When the window is gone, `done` is dropped
    /// (and with it any busy guard it holds).
    fn run<F, T>(&self, window: &mut Window, cx: &mut Context<Self>, future: F, done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static)
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel::<T>();
        crate::runtime().spawn(async move {
            let _ = tx.send(future.await);
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(value) = rx.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    done(this, value, window, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Appends a line to the Log tab, keeping at most LOG_MAX_LINES lines.
    fn log(&mut self, line: impl Into<String>) {
        for line in line.into().split('\n') {
            self.log_lines.push_back(line.to_string());
        }
        while self.log_lines.len() > LOG_MAX_LINES {
            self.log_lines.pop_front();
        }
        self.log_dirty = true;
    }

    /// One tick of the message loop: worker messages, the log, and a redraw
    /// when a busy guard was dropped.
    fn tick(&mut self, batch: Vec<UiMsg>, window: &mut Window, cx: &mut Context<Self>) {
        let mut redraw = self.changed.replace(false);
        for m in batch {
            redraw = true;
            match m {
                UiMsg::Log(l) => self.log(l),
                UiMsg::Stage(s) => {
                    self.stage_text = Some(s.label().to_string());
                    self.build_fraction = stage_fraction(s);
                }
                UiMsg::Progress(f) => self.build_fraction = progress_fraction(self.build_fraction, f),
            }
        }
        if self.log_dirty {
            self.log_dirty = false;
            let text = self.log_lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
            self.log_view.update(cx, |log, cx| {
                log.set_value(text, window, cx);
                // Follow the newest line (clamped to the end after layout).
                log.set_scroll_offset(point(px(0.), px(-1.0e9)), cx);
            });
        }
        // The build's progress bar moves with the clock while it runs.
        if redraw || self.state.borrow().build_control.is_some() || self.installing {
            cx.notify();
        }
    }

    /// Starts a download, build or privileged action under the global busy
    /// state, or returns None (with a toast) when another one is still running.
    fn begin_busy(&mut self, what: &'static str, window: &mut Window, cx: &mut Context<Self>) -> Option<BusyGuard> {
        let running = self.state.borrow().busy;
        if let Some(running) = running {
            toast(window, cx, &format!("Wait until the {running} has finished"));
            return None;
        }
        self.state.borrow_mut().busy = Some(what);
        cx.notify();
        Some(BusyGuard { state: self.state.clone(), changed: self.changed.clone() })
    }

    /// A closure that runs `f` on this view later (from a dialog button).
    fn later(&self, cx: &Context<Self>, f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static) -> Continuation {
        let this = cx.weak_entity();
        Box::new(move |window, cx| {
            let _ = this.update(cx, |this, cx| {
                f(this, window, cx);
                cx.notify();
            });
        })
    }

    /// Closing the window stops a running build first: its process group
    /// would otherwise keep compiling with nobody to collect the packages.
    /// The window closes once the build has stopped and cleaned up.
    /// A privileged action is not closed under: the script writes its output
    /// to the app, and with nobody reading it the next write would end the
    /// script halfway (in dpkg, or with the ESP half updated).
    fn may_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (busy, control, download) = {
            let s = self.state.borrow();
            (s.busy, s.build_control.clone(), s.download_cancel.clone())
        };
        let control = match close_decision(busy, control.is_some(), download.is_some()) {
            CloseDecision::Close => return true,
            CloseDecision::Refuse(what) => {
                toast(window, cx, &format!("Wait until the {what} has finished before closing"));
                return false;
            }
            CloseDecision::StopDownloadFirst => {
                self.stop_download_then_close(download.expect("a download runs"), window, cx);
                return false;
            }
            CloseDecision::StopBuildFirst => control.expect("a build runs"),
        };
        if !control.is_cancelled() {
            control.cancel();
            toast(window, cx, "Stopping the build before closing…");
        }
        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            if state.borrow().build_control.is_none() {
                let _ = cx.update(|window, _| window.remove_window());
                break;
            }
        })
        .detach();
        false
    }

    /// Cancels the running download and closes the window once it has stopped
    /// and deleted its `.part` file. A download stuck in a network call that
    /// does not end gets `DOWNLOAD_STOP_WAIT`; then its `.part` file is
    /// deleted here and the window closes anyway.
    fn stop_download_then_close(&mut self, flag: Arc<std::sync::atomic::AtomicBool>, window: &mut Window, cx: &mut Context<Self>) {
        if !flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
            self.log("Stopping the download before closing…");
            toast(window, cx, "Stopping the download before closing…");
        }
        let state = self.state.clone();
        let started = Instant::now();
        cx.spawn_in(window, async move |_, cx| loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let stopped = state.borrow().download_cancel.is_none();
            if stopped || started.elapsed() >= DOWNLOAD_STOP_WAIT {
                if !stopped {
                    remove_stale_parts(&build::downloads_root());
                }
                let _ = cx.update(|window, _| window.remove_window());
                break;
            }
        })
        .detach();
    }

    // ── System tab ──────────────────────────────────────────────────────────

    fn load_sysinfo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.running_title = "Detecting…".into();
        self.log("Querying installed kernels…");
        self.run(window, cx, async move { tokio::task::spawn_blocking(query_system).await.unwrap_or_default() }, |this, info: SystemInfo, window, cx| {
            this.sysinfo_loaded(info, window, cx)
        });
    }

    fn sysinfo_loaded(&mut self, info: SystemInfo, window: &mut Window, cx: &mut Context<Self>) {
        self.running_title = info.running_kernel.clone();
        self.sys_loaded = true;
        if info.unsupported_system {
            // AppRun doesn't install the helper here, and nothing would use it: the
            // unsupported-system banner says all there is to say.
            self.helper_banner = None;
        }
        self.health_banner = health_banner_text(&info);
        if let Some(text) = &self.health_banner {
            let text = format!("WARNING: {text}");
            self.log(text);
        }
        self.log(format!("Running kernel: {} · {} kernels installed", info.running_kernel, info.kernels.len()));
        let running_kernel = info.running_kernel.clone();
        self.state.borrow_mut().sysinfo = info;
        if self.state.borrow().staged.is_none() {
            // Nothing to compare yet; the Install tab reads the running kernel when it draws.
        }

        // Nice-to-have, not safety-critical: check kernel.org for a newer
        // stable release without holding up the System tab. A failed fetch
        // is silent — the Browse tab shows the same data and its errors.
        self.update_banner = None;
        self.run(window, cx, fetch_releases(), move |this, result, _, _| {
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
                    this.update_banner = Some(update_banner_text(&newest.version, this.state.borrow().sysinfo.unsupported_system));
                }
            }
        });
    }

    fn sign_clicked(&mut self, ver: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(busy) = self.begin_busy("signing", window, cx) else { return };
        self.log(format!("Signing kernel {ver}…"));
        self.run(window, cx, async move { tokio::task::spawn_blocking(move || run_privileged_sign(&ver)).await }, move |this, result, window, cx| {
            drop(busy);
            match result {
                Ok(Ok(out)) => {
                    let msg = report_run(&mut |l| this.log(l), &out, Done::Sign);
                    toast(window, cx, &msg);
                }
                Ok(Err(e)) => {
                    this.log(format!("Signing failed: {e}"));
                    toast(window, cx, "Signing failed — see Log tab");
                }
                Err(e) => this.log(format!("Task error: {e}")),
            }
            this.load_sysinfo(window, cx);
        });
    }

    /// One-time boot of this kernel; the saved default stays put.
    fn boot_once_clicked(&mut self, ver: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(busy) = self.begin_busy("boot selection", window, cx) else { return };
        self.log(format!("Selecting {ver} for the next boot only…"));
        self.run(window, cx, async move { tokio::task::spawn_blocking(move || run_privileged_boot_once(&ver)).await }, move |this, result, window, cx| {
            drop(busy);
            match result {
                Ok(Ok(out)) => {
                    log_output(&mut |l| this.log(l), &out);
                    toast(window, cx, "The next restart will boot that kernel once");
                }
                Ok(Err(e)) => {
                    this.log(format!("Boot-once failed: {e}"));
                    toast(window, cx, "Could not set next boot — see Log tab");
                }
                Err(e) => this.log(format!("Task error: {e}")),
            }
        });
    }

    fn remove_clicked(&mut self, ver: String, window: &mut Window, cx: &mut Context<Self>) {
        let question = Question {
            id: "remove-kernel",
            title: format!("Remove kernel {ver}?").into(),
            body: "Its packages will be purged and the boot loader updated. The running kernel is never touched.".into(),
            ok_text: "Remove",
            cancel_text: "Cancel",
            default: DefaultButton::Cancel,
            destructive: true,
        };
        let go = self.later(cx, move |this, window, cx| {
            let Some(busy) = this.begin_busy("kernel removal", window, cx) else { return };
            this.log(format!("Removing kernel {ver}…"));
            this.run(window, cx, async move { tokio::task::spawn_blocking(move || run_privileged_remove(&ver)).await }, move |this, result, window, cx| {
                drop(busy);
                match result {
                    Ok(Ok(out)) => {
                        log_output(&mut |l| this.log(l), &out);
                        this.log("Kernel removed.");
                        toast(window, cx, "Kernel removed");
                    }
                    Ok(Err(e)) => {
                        this.log(format!("Remove failed: {e}"));
                        toast(window, cx, "Remove failed — see Log tab");
                    }
                    Err(e) => this.log(format!("Task error: {e}")),
                }
                this.load_sysinfo(window, cx);
            });
        });
        show_question(question, go, None, window, cx);
    }

    /// Remove old kernels: ask how many to keep, show exactly what would go,
    /// then remove them all under one authentication. The running kernel and
    /// the newest installed kernel are never candidates (and the script
    /// enforces the same rule on its side).
    fn prune_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const TITLE: &str = "Remove old kernels";
        let keep = Rc::new(Cell::new(2usize));
        let review_focus = cx.focus_handle();
        let cancel_focus = cx.focus_handle();
        let first = Cell::new(true);
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                review_focus.focus(window, cx);
            }
            let review = {
                let (keep, this) = (keep.clone(), this.clone());
                move |window: &mut Window, cx: &mut App| {
                    let (keep, this) = (keep.get(), this.clone());
                    window.defer(cx, move |window, cx| {
                        let _ = this.update(cx, |this, cx| this.prune_review(keep, window, cx));
                    });
                }
            };
            let enter_review = review.clone();
            let (on_review, on_cancel) = (review_focus.clone(), cancel_focus.clone());
            let count = keep.get();
            let (less, more) = (keep.clone(), keep.clone());
            dialog
                .title(dialog_title(TITLE))
                .overlay(false)
                .w(px(420.))
                .on_ok(move |_, window, cx| {
                    match enter_key(on_review.is_focused(window), on_cancel.is_focused(window)) {
                        EnterKey::Confirm => enter_review(window, cx),
                        EnterKey::Cancel => {}
                        EnterKey::Nothing => return false,
                    }
                    true
                })
                .child(
                    named_group("prune", TITLE)
                        .child(label("text", "How many of the newest installed kernels should be kept? The running kernel is always kept as well."))
                        .child(
                            h_flex()
                                .justify_center()
                                .gap_3()
                                .child(Button::new("fewer").outline().label("−").accessibility_label("Keep fewer").disabled(count <= 1).on_click(
                                    move |_, window, _| {
                                        less.set(less.get().saturating_sub(1).max(1));
                                        window.refresh();
                                    },
                                ))
                                .child(div().min_w(px(90.)).text_center().child(label("keep", format!("Keep {count}"))))
                                .child(Button::new("more").outline().label("+").accessibility_label("Keep more").disabled(count >= 10).on_click(
                                    move |_, window, _| {
                                        more.set((more.get() + 1).min(10));
                                        window.refresh();
                                    },
                                )),
                        )
                        .child(
                            dialog_buttons()
                                .child(Button::new("cancel").label("Cancel").track_focus(&cancel_focus).on_click(|_, window, cx| window.close_dialog(cx)))
                                .child(Button::new("review").primary().label("Review").track_focus(&review_focus).on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    review(window, cx);
                                })),
                        ),
                )
        });
    }

    fn prune_review(&mut self, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        let candidates = prune_candidates(&self.state.borrow().sysinfo.kernels, keep);
        if candidates.is_empty() {
            toast(window, cx, "Nothing to remove");
            return;
        }
        let question = Question {
            id: "prune-confirm",
            title: format!("Remove {} kernel(s)?", candidates.len()).into(),
            body: format!(
                "These will be purged and the boot loader updated:\n\n{}\n\nKernels whose removal would also remove the kernel metapackages are skipped and reported in the Log.",
                candidates.join("\n")
            )
            .into(),
            ok_text: "Remove",
            cancel_text: "Cancel",
            default: DefaultButton::Cancel,
            destructive: true,
        };
        let go = self.later(cx, move |this, window, cx| {
            let Some(busy) = this.begin_busy("kernel removal", window, cx) else { return };
            this.log(format!("Removing old kernels: {}", candidates.join(", ")));
            this.run(
                window,
                cx,
                async move { tokio::task::spawn_blocking(move || run_privileged_remove_many(&candidates)).await },
                move |this, result, window, cx| {
                    drop(busy);
                    match result {
                        Ok(Ok(out)) => {
                            log_output(&mut |l| this.log(l), &out);
                            toast(window, cx, "Old kernels removed");
                        }
                        Ok(Err(e)) => {
                            this.log(format!("Removal finished with problems: {e}"));
                            toast(window, cx, "Some kernels were not removed — see Log tab");
                        }
                        Err(e) => this.log(format!("Task error: {e}")),
                    }
                    this.load_sysinfo(window, cx);
                },
            );
        });
        show_question(question, go, None, window, cx);
    }

    /// Set up kernel signing: ask for a one-time MOK enrollment password
    /// (twice), then create the key, sign the installed kernels and queue the
    /// enrollment under one authentication. The password goes to the script
    /// on stdin, never on its command line.
    fn setup_signing_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let submit: Submit = Rc::new(move |password, window, cx| {
            window.close_dialog(cx);
            let this = this.clone();
            window.defer(cx, move |window, cx| {
                let _ = this.update(cx, |this, cx| this.setup_signing(password, window, cx));
            });
        });
        let prompt = cx.new(|cx| SigningPrompt::new(submit.clone(), window, cx));
        let setup_focus = cx.focus_handle();
        let cancel_focus = cx.focus_handle();
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                let field = prompt.read(cx).pw1.focus_handle(cx);
                field.focus(window, cx);
            }
            let ready = prompt.read(cx).password_in(cx);
            let (on_setup, on_cancel) = (setup_focus.clone(), cancel_focus.clone());
            let (enter_submit, click_submit) = (submit.clone(), submit.clone());
            let (enter_ready, click_ready) = (ready.clone(), ready.clone());
            dialog
                .title(dialog_title(SIGNING_TITLE))
                .overlay(false)
                .w(px(480.))
                .on_ok(move |_, window, cx| {
                    match enter_key(on_setup.is_focused(window), on_cancel.is_focused(window)) {
                        EnterKey::Confirm => match &enter_ready {
                            Some(password) => enter_submit(password.clone(), window, cx),
                            None => return false,
                        },
                        EnterKey::Cancel => window.close_dialog(cx),
                        // The fields take Enter themselves (the second one submits).
                        EnterKey::Nothing => return false,
                    }
                    true
                })
                .child(
                    named_group("signing", SIGNING_TITLE).child(div().id("body").max_h(dialog_room(window)).overflow_y_scroll().child(prompt.clone())).child(
                        dialog_buttons()
                            .child(Button::new("cancel").label("Cancel").track_focus(&cancel_focus).on_click(|_, window, cx| window.close_dialog(cx)))
                            .child(Button::new("setup").primary().label("Set Up").track_focus(&setup_focus).disabled(click_ready.is_none()).on_click(
                                move |_, window, cx| {
                                    if let Some(password) = &click_ready {
                                        click_submit(password.clone(), window, cx);
                                    }
                                },
                            )),
                    ),
                )
        });
    }

    fn setup_signing(&mut self, password: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(busy) = self.begin_busy("signing setup", window, cx) else { return };
        self.log("Setting up kernel signing…");
        self.run(
            window,
            cx,
            async move { tokio::task::spawn_blocking(move || run_privileged_setup_signing(&password)).await },
            move |this, result, window, cx| {
                drop(busy);
                match result {
                    Ok(Ok(out)) => {
                        let msg = report_run(&mut |l| this.log(l), &out, Done::SetupSigning);
                        toast(window, cx, &msg);
                    }
                    Ok(Err(e)) => {
                        this.log(format!("Signing setup failed: {e}"));
                        toast(window, cx, "Signing setup failed — see Log tab");
                    }
                    Err(e) => this.log(format!("Task error: {e}")),
                }
                this.load_sysinfo(window, cx);
            },
        );
    }

    // ── Browse tab ──────────────────────────────────────────────────────────

    fn load_releases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.releases_loading = true;
        self.browse_error = None;
        {
            let mut s = self.state.borrow_mut();
            s.releases.clear();
            s.selected_release = None;
        }
        self.selected_text = "Loading…".into();
        self.log("Fetching the release list from kernel.org…");
        self.run(window, cx, fetch_releases(), |this, result, _, _| {
            this.releases_loading = false;
            match result {
                Err(e) => {
                    this.log(format!("Error fetching releases: {e:#}"));
                    this.browse_error = Some(format!("Failed to load releases: {e}"));
                    this.selected_text = "Could not load the release list".into();
                }
                Ok(mut releases) => {
                    // A version can be listed twice (as mainline and as
                    // stable right after a release); rows are keyed by
                    // version, so keep the first.
                    let mut seen = std::collections::HashSet::new();
                    releases.retain(|r| seen.insert(r.version.clone()));
                    this.log(format!("kernel.org lists {} releases", releases.len()));
                    this.state.borrow_mut().releases = releases;
                    this.selected_text = "No release selected".into();
                }
            }
        });
    }

    fn select_release(&mut self, version: &str) {
        let selected = self.state.borrow().releases.iter().find(|r| r.version == version).cloned();
        self.selected_text = match &selected {
            Some(r) => format!("Selected: {} ({})", r.version, r.moniker.label()),
            None => "No release selected".into(),
        };
        self.state.borrow_mut().selected_release = selected;
    }

    fn download_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rel) = self.state.borrow().selected_release.clone() else { return };
        let mut files = vec![];
        for url in std::iter::once(&rel.source).chain(rel.pgp.as_ref()) {
            let Some(name) = url_file_name(url) else {
                toast(window, cx, "kernel.org listed an unusual download URL");
                return;
            };
            files.push(Remote { url: url.clone(), filename: name.to_string() });
        }
        let Some(busy) = self.begin_busy("download", window, cx) else { return };
        self.download_bar = Some(DownloadBar { fraction: Some(0.0), text: format!("Downloading Linux {}…", rel.version) });
        self.page = Page::Build;
        self.log(format!("Downloading Linux {} from cdn.kernel.org…", rel.version));
        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.state.borrow_mut().download_cancel = Some(cancel_flag.clone());

        // Progress reports, from the Tokio side; the loop ends when the sender drops.
        let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel::<DownloadProgress>();
        let download_start = Instant::now();
        cx.spawn_in(window, async move |this, cx| {
            while let Some(mut msg) = prog_rx.recv().await {
                while let Ok(newer) = prog_rx.try_recv() {
                    msg = newer;
                }
                let alive = this.update(cx, |this, cx| {
                    // A report that arrives after the download ended (its busy
                    // guard clears download_cancel) must not bring the bar back.
                    if this.state.borrow().download_cancel.is_none() {
                        return;
                    }
                    let prefix = format!("[{}/{}] {}", msg.file_index + 1, msg.file_count, msg.filename);
                    let bar = match msg.total {
                        Some(t) => {
                            let elapsed = download_start.elapsed().as_secs_f64();
                            let speed = if elapsed > 0.0 { msg.downloaded as f64 / elapsed } else { 0.0 };
                            DownloadBar {
                                fraction: (t > 0).then(|| msg.downloaded as f32 / t as f32),
                                text: format!(
                                    "{prefix}  {:.1} / {:.1} MB ({:.1} MB/s)",
                                    msg.downloaded as f64 / 1_000_000.0,
                                    t as f64 / 1_000_000.0,
                                    speed / 1_000_000.0
                                ),
                            }
                        }
                        None => DownloadBar { fraction: None, text: prefix },
                    };
                    this.download_bar = Some(bar);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
        })
        .detach();

        self.run(
            window,
            cx,
            download_files(
                files,
                build::downloads_root(),
                move |p| {
                    let _ = prog_tx.send(p);
                },
                cancel_flag,
            ),
            move |this, result, window, cx| {
                drop(busy);
                match result {
                    Err(e) => {
                        let fraction = this.download_bar.as_ref().and_then(|b| b.fraction);
                        if is_cancelled(&e) {
                            this.log("Download cancelled.");
                            this.download_bar = Some(DownloadBar { fraction, text: "Download cancelled".into() });
                        } else {
                            this.log(format!("Download failed: {e:#}"));
                            this.download_bar = Some(DownloadBar { fraction, text: "Download failed".into() });
                            toast(window, cx, "Download failed — see Log tab");
                        }
                    }
                    Ok(paths) => {
                        this.log(format!("Downloaded {}", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
                        this.download_bar = None;
                        if let Some(tarball) = paths.first() {
                            this.set_source(tarball.clone(), window, cx);
                        }
                    }
                }
            },
        );
    }

    fn cancel_clicked(&mut self) {
        let (flag, control) = {
            let s = self.state.borrow();
            (s.download_cancel.clone(), s.build_control.clone())
        };
        if let Some(flag) = flag {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(control) = control {
            self.log("Cancelling the build…");
            control.cancel();
        }
    }

    // ── Build tab ───────────────────────────────────────────────────────────

    /// Build tools (ccache among them) and disk space, checked off the UI thread.
    fn refresh_requirements(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(
            window,
            cx,
            async move {
                tokio::task::spawn_blocking(|| {
                    // An earlier failed build's tree is removed when the next
                    // build starts, so its space counts as free here.
                    let root = build::build_root();
                    let cache_free = free_bytes(&root).map(|f| f + build::dir_size(&root));
                    (missing_build_deps(), cache_free, free_bytes(std::path::Path::new("/boot")))
                })
                .await
            },
            |this, result, _, _| {
                let Ok((missing, cache_free, boot_free)) = result else { return };
                this.deps_text = if missing.is_empty() { "All installed".into() } else { format!("Missing: {}", missing.join(", ")) };
                let (mode, boot_need) = {
                    let mut s = this.state.borrow_mut();
                    s.missing_deps = missing;
                    (s.config_mode().unwrap_or(ConfigMode::MatchSystem), s.sysinfo.boot_needed_bytes)
                };
                this.space_text = match space_problem(&mode, cache_free, boot_free, boot_need) {
                    Some(p) => p,
                    None => format!(
                        "{} free for the build (about {} needed){}",
                        cache_free.map(format_bytes).unwrap_or_else(|| "?".into()),
                        format_bytes(mode.disk_need()),
                        boot_free.map(|b| format!(", {} free in /boot", format_bytes(b))).unwrap_or_default()
                    ),
                };
            },
        );
    }

    /// Look into a tarball, find or fetch its signature, verify it.
    fn set_source(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(busy) = self.begin_busy("signature check", window, cx) else { return };
        {
            let mut s = self.state.borrow_mut();
            s.source = None;
            s.signature = SigState::Checking;
            s.allow_unverified = false;
        }
        self.page = Page::Build;
        self.log(format!("Source: {}", path.display()));
        let tx = self.ui_tx.clone();
        self.run(
            window,
            cx,
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
                        let _ = tx
                            .send(UiMsg::Log(format!("No signature next to the tarball; fetching kernel.org's for Linux {}", tarball.version.release_name())));
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
            move |this, result: anyhow::Result<(Tarball, Result<Signer, String>)>, window, cx| {
                drop(busy);
                match result {
                    Err(e) => {
                        this.log(format!("Not a usable kernel source: {e:#}"));
                        toast(window, cx, "Not a kernel source tarball — see Log tab");
                        this.state.borrow_mut().signature = SigState::None;
                    }
                    Ok((tarball, verdict)) => {
                        this.log(format!("Kernel version from the Makefile: {}", tarball.version.kernelversion()));
                        let signature = match verdict {
                            Ok(signer) => {
                                this.log(format!("Signature: good, from {} ({})", signer.uid, signer.grouped_fingerprint()));
                                SigState::Verified(signer)
                            }
                            Err(why) => {
                                this.log(format!(
                                    "Signature: NOT VERIFIED — {why}. The build stays blocked unless \"Build unverified source\" is turned on for this tarball."
                                ));
                                toast(window, cx, "The source could not be verified — see the Build tab");
                                SigState::Failed(why)
                            }
                        };
                        let mut s = this.state.borrow_mut();
                        s.source = Some(tarball);
                        s.signature = signature;
                    }
                }
            },
        );
    }

    fn open_tarball_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Open".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let result = picked.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.set_source(path, window, cx);
                    }
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(e)) => toast(window, cx, &format!("Could not open the file dialog: {e:#}. Drop the tarball on the window instead.")),
            });
        })
        .detach();
    }

    fn choose_config_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let result = picked.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.own_config_text = format!("{} — the same fixes are applied, then make olddefconfig", path.display());
                        {
                            let mut s = this.state.borrow_mut();
                            s.own_config = Some(path);
                            s.mode = ModeChoice::Own;
                        }
                        this.refresh_requirements(window, cx);
                        cx.notify();
                    }
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(e)) => toast(window, cx, &format!("Could not open the file dialog: {e:#}")),
            });
        })
        .detach();
    }

    fn set_mode(&mut self, mode: ModeChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.state.borrow_mut().mode = mode;
        self.refresh_requirements(window, cx);
    }

    /// Build unverified source: one explicit override per tarball.
    fn unverified_toggled(&mut self, want: bool, window: &mut Window, cx: &mut Context<Self>) {
        if want == self.state.borrow().allow_unverified {
            return;
        }
        if !want {
            self.state.borrow_mut().allow_unverified = false;
            return;
        }
        let question = Question {
            id: "unverified",
            title: "Build source that is not verified?".into(),
            body: "This tarball has no valid signature from a kernel.org developer, so nothing shows that it is the code kernel.org released. Only go on for a tree you made or patched yourself. The build log and the package folder will say the source was unverified.".into(),
            ok_text: "Build Unverified",
            cancel_text: "Cancel",
            default: DefaultButton::Cancel,
            destructive: true,
        };
        let allow = self.later(cx, |this, _, _| {
            this.state.borrow_mut().allow_unverified = true;
            let path = this.state.borrow().source.as_ref().map(|t| t.path.display().to_string()).unwrap_or_default();
            this.log(format!("WARNING: building {path} without a verified signature was allowed for this tarball"));
        });
        show_question(question, allow, None, window, cx);
    }

    fn install_deps_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing = self.state.borrow().missing_deps.clone();
        if missing.is_empty() {
            return;
        }
        let Some(busy) = self.begin_busy("build tools install", window, cx) else { return };
        self.log(format!("Installing build tools: {}", missing.join(" ")));
        self.run(
            window,
            cx,
            async move { tokio::task::spawn_blocking(move || run_privileged_install_build_deps(&missing)).await },
            move |this, result, window, cx| {
                drop(busy);
                match result {
                    Ok(Ok(out)) => {
                        log_output(&mut |l| this.log(l), &out);
                        toast(window, cx, "Build tools installed");
                    }
                    Ok(Err(e)) => {
                        this.log(format!("Installing the build tools failed: {e}"));
                        toast(window, cx, "Installing the build tools failed — see Log tab");
                    }
                    Err(e) => this.log(format!("Task error: {e}")),
                }
                this.refresh_requirements(window, cx);
            },
        );
    }

    /// Prepare (extract + configure), show the summary, build.
    fn build_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let req = {
            let s = self.state.borrow();
            let (Some(tarball), Some(trust), Some(mode)) = (s.source.clone(), s.source_trust(), s.config_mode()) else { return };
            if !s.missing_deps.is_empty() {
                return;
            }
            BuildRequest { tarball, trust, mode, running_kernel: s.sysinfo.running_kernel.clone() }
        };
        let boot_need = self.state.borrow().sysinfo.boot_needed_bytes;
        let tree_log = build::tree_dir(&req.tarball.version).join("build.log");
        let Some(busy) = self.begin_busy("build", window, cx) else { return };
        let control = BuildControl::new();
        {
            let mut s = self.state.borrow_mut();
            s.build_control = Some(control.clone());
            s.build_started = Some(Instant::now());
        }
        self.log(format!("Preparing the build of Linux {}…", req.tarball.version.kernelversion()));
        self.stage_text = Some("Preparing".into());
        self.download_bar = None;
        self.build_fraction = Some(0.0);

        let sink = self.event_sink();
        let c2 = control.clone();
        self.run(
            window,
            cx,
            async move { tokio::task::spawn_blocking(move || build::prepare(req, sink, c2, boot_need)).await },
            move |this, result, window, cx| {
                let prepared: Prepared = match result {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => {
                        if is_build_cancelled(&e) {
                            this.log("Build cancelled.");
                            this.finish_build(busy, "Build cancelled", window, cx);
                        } else {
                            this.log(format!("Preparing the build failed: {e:#}"));
                            if tree_log.exists() {
                                this.log(format!("The output is in {}", tree_log.display()));
                            }
                            toast(window, cx, "Preparing the build failed — see Log tab");
                            this.finish_build(busy, "Preparing the build failed", window, cx);
                        }
                        return;
                    }
                    Err(e) => {
                        this.log(format!("Task error: {e}"));
                        this.finish_build(busy, "Build failed", window, cx);
                        return;
                    }
                };
                // Show what will be built and wait for a yes.
                this.state.borrow_mut().build_started = None;
                let question = Question {
                    id: "build-summary",
                    title: format!("Build Linux {}?", prepared.summary.kernelrelease).into(),
                    body: prepared.summary.text().into(),
                    ok_text: "Build",
                    cancel_text: "Cancel",
                    default: DefaultButton::Action,
                    destructive: false,
                };
                let slot = Rc::new(RefCell::new(Some((prepared, busy))));
                let cancel_slot = slot.clone();
                let go = this.later(cx, move |this, window, cx| {
                    let Some((prepared, busy)) = slot.borrow_mut().take() else { return };
                    this.start_build(prepared, busy, control, tree_log, window, cx);
                });
                let cancel = this.later(cx, move |this, window, cx| {
                    let Some((prepared, busy)) = cancel_slot.borrow_mut().take() else { return };
                    this.log("Build not started; the configured tree is deleted.");
                    // Busy until the tree is gone: a new build at once would extract into it.
                    this.run(window, cx, async move { tokio::task::spawn_blocking(move || prepared.discard()).await }, move |this, _, window, cx| {
                        this.finish_build(busy, "", window, cx)
                    });
                });
                show_question(question, go, Some(cancel), window, cx);
            },
        );
    }

    fn start_build(&mut self, prepared: Prepared, busy: BusyGuard, control: Arc<BuildControl>, tree_log: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let krel = prepared.summary.kernelrelease.clone();
        self.log(format!("Building Linux {krel} — this takes a while; the full output goes to build.log"));
        self.state.borrow_mut().build_started = Some(Instant::now());
        let sink = self.event_sink();
        self.run(
            window,
            cx,
            async move { tokio::task::spawn_blocking(move || build::build(prepared, sink, control)).await },
            move |this, result, window, cx| {
                let elapsed = this.state.borrow().build_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                let took = took(elapsed);
                match result {
                    Ok(Ok(set)) => {
                        this.log(format!("Built Linux {} in {took}: {}", set.kernelrelease, set.dir.display()));
                        toast(window, cx, "Kernel built — ready to install");
                        this.finish_build(busy, &format!("Built in {took}"), window, cx);
                        this.load_built(Some(set.dir.clone()), window, cx);
                        this.page = Page::Install;
                    }
                    Ok(Err(e)) if is_build_cancelled(&e) => {
                        this.log("Build cancelled; its files were removed.");
                        this.finish_build(busy, "Build cancelled", window, cx);
                    }
                    Ok(Err(e)) => {
                        this.log(format!("Build failed after {took}: {e:#}"));
                        this.log(format!("The full output is in {}", tree_log.display()));
                        toast(window, cx, "Build failed — see Log tab");
                        this.finish_build(busy, "Build failed", window, cx);
                    }
                    Err(e) => {
                        this.log(format!("Task error: {e}"));
                        this.finish_build(busy, "Build failed", window, cx);
                    }
                }
            },
        );
    }

    fn finish_build(&mut self, busy: BusyGuard, stage: &str, window: &mut Window, cx: &mut Context<Self>) {
        drop(busy);
        self.stage_text = (!stage.is_empty()).then(|| stage.to_string());
        self.build_fraction = None;
        self.refresh_requirements(window, cx);
    }

    fn event_sink(&self) -> EventSink {
        let tx = self.ui_tx.clone();
        Arc::new(move |e: BuildEvent| {
            let _ = tx.send(match e {
                BuildEvent::Stage(s) => UiMsg::Stage(s),
                BuildEvent::Line(l) => UiMsg::Log(l),
                BuildEvent::Progress(f) => UiMsg::Progress(f),
            });
        })
    }

    // ── Install tab ─────────────────────────────────────────────────────────

    /// Re-read ~/Downloads/kernel-pop and select `select` (or the one
    /// selected before, or the newest).
    fn load_built(&mut self, select: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        self.run(window, cx, async move { tokio::task::spawn_blocking(built_sets).await.unwrap_or_default() }, move |this, sets: Vec<BuiltSet>, window, cx| {
            let keep = select.or_else(|| this.state.borrow().staged.as_ref().map(|s| s.dir.clone()));
            let pick = sets.iter().position(|set| keep.as_ref() == Some(&set.dir)).or(if keep.is_none() && !sets.is_empty() { Some(0) } else { None });
            {
                // The files may have changed (a rebuild into the same
                // folder): whatever gets selected is checked again.
                let mut s = this.state.borrow_mut();
                s.built = sets;
                s.staged = None;
                s.staged_verified = false;
            }
            if let Some(index) = pick {
                this.select_built(index, window, cx);
            }
        });
    }

    fn select_built(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let set = self.state.borrow().built.get(index).cloned();
        let same = set.is_some() && self.state.borrow().staged == set;
        if same && self.state.borrow().staged_verified {
            return;
        }
        {
            let mut s = self.state.borrow_mut();
            s.staged = set.clone();
            s.staged_verified = false;
        }
        let Some(set) = set else { return };
        self.run(
            window,
            cx,
            async move {
                let s2 = set.clone();
                (set, tokio::task::spawn_blocking(move || verify_built_set(&s2)).await)
            },
            |this, (set, result), _, _| {
                // Only if it is still the selected one.
                if this.state.borrow().staged.as_ref() != Some(&set) {
                    return;
                }
                match result {
                    Ok(Ok(())) => this.state.borrow_mut().staged_verified = true,
                    Ok(Err(e)) => {
                        this.log(format!("{}: {e:#} — not installable; build it again", set.dir.display()));
                        let mut s = this.state.borrow_mut();
                        s.staged = None;
                        s.staged_verified = false;
                    }
                    Err(e) => this.log(format!("Task error: {e}")),
                }
            },
        );
    }

    fn delete_built_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(set) = self.state.borrow().staged.clone() else { return };
        let question = Question {
            id: "delete-build",
            title: format!("Delete the build of Linux {}?", set.kernelrelease).into(),
            body: format!("{} and everything in it (packages, config, build log) is deleted. Installed kernels are not touched.", set.dir.display()).into(),
            ok_text: "Delete",
            cancel_text: "Cancel",
            default: DefaultButton::Cancel,
            destructive: true,
        };
        let go = self.later(cx, move |this, window, cx| {
            match std::fs::remove_dir_all(&set.dir) {
                Ok(()) => this.log(format!("Deleted {}", set.dir.display())),
                Err(e) => this.log(format!("Could not delete {}: {e}", set.dir.display())),
            }
            {
                let mut s = this.state.borrow_mut();
                s.staged = None;
                s.staged_verified = false;
            }
            this.load_built(None, window, cx);
        });
        show_question(question, go, None, window, cx);
    }

    /// Preflight: Secure Boot and /boot space need an explicit "install
    /// anyway" before the install starts. A kernel that could not boot under
    /// Secure Boot is not installed at all: the dialog explains why and leads
    /// to Set Up Signing.
    fn install_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocker = install_blocker(&self.state.borrow().sysinfo);
        if let Some(why) = blocker {
            let question = Question {
                id: "install-blocked",
                title: "This kernel could not boot".into(),
                body: why.into(),
                ok_text: "Go to System Tab",
                cancel_text: "Close",
                default: DefaultButton::Action,
                destructive: false,
            };
            let go = self.later(cx, |this, _, _| this.page = Page::System);
            show_question(question, go, None, window, cx);
            return;
        }
        let mut warnings = install_warnings(&self.state.borrow().sysinfo);
        if let Some(f) = self.state.borrow().sysinfo.free_root_bytes {
            if f < MIN_ROOT_BYTES {
                warnings.push(format!("/ has only {} free; the modules and initramfs need room too.", format_bytes(f)));
            }
        }
        if self.state.borrow().staged.as_ref().is_some_and(|s| s.source.starts_with("UNVERIFIED")) {
            warnings.push("This kernel was built from source that was not verified against a kernel.org signature.".to_string());
        }
        if warnings.is_empty() {
            self.start_install(window, cx);
            return;
        }
        let question = Question {
            id: "install-warnings",
            title: "Before you install".into(),
            body: warnings.join("\n\n").into(),
            ok_text: "Install Anyway",
            cancel_text: "Cancel",
            default: DefaultButton::Cancel,
            destructive: true,
        };
        let go = self.later(cx, |this, window, cx| this.start_install(window, cx));
        show_question(question, go, None, window, cx);
    }

    fn start_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = {
            let s = self.state.borrow();
            if !s.staged_verified {
                return;
            }
            s.staged.as_ref().map(|set| set.dir.clone())
        };
        let Some(dir) = dir else { return };
        let Some(busy) = self.begin_busy("kernel install", window, cx) else { return };
        let dir_str = dir.display().to_string();
        self.installing = true;
        self.page = Page::Log;
        self.log(format!("Starting kernel install from {dir_str}"));
        self.run(window, cx, async move { tokio::task::spawn_blocking(move || run_privileged_install(&dir_str)).await }, move |this, result, window, cx| {
            drop(busy);
            this.installing = false;
            match result {
                Ok(Ok(out)) => {
                    // DKMS and signing failures never fail the install, and
                    // neither does a kernel that will not boot under Secure
                    // Boot, but none of them may go unnoticed before a reboot.
                    let msg = report_run(&mut |l| this.log(l), &out, Done::Install);
                    this.log("Install completed — initramfs verified in /boot.");
                    toast(window, cx, &msg);
                }
                Ok(Err(e)) => {
                    this.log(format!("Install failed: {e}"));
                    toast(window, cx, "Install failed — see Log tab");
                }
                Err(e) => this.log(format!("Task error: {e}")),
            }
            // A failed install can still have changed /boot (or rolled a kernel back out).
            this.load_sysinfo(window, cx);
        });
    }

    fn drop_received(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = paths.paths().first().cloned() else { return };
        if let Some(running) = self.state.borrow().busy {
            toast(window, cx, &format!("Wait until the {running} has finished"));
            return;
        }
        self.set_source(path, window, cx);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Drawing helpers: groups of rows, in the style of a settings page
// ─────────────────────────────────────────────────────────────────────────────

/// A titled group of rows with an optional line under the title.
fn group(id: &'static str, title: &'static str, description: Option<&'static str>, rows: Vec<AnyElement>, cx: &App) -> Stateful<Div> {
    v_flex()
        .id(id)
        .role(Role::Group)
        .aria_label(title)
        .gap_1()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(label(SharedString::from(format!("{id}-title")), title)))
        .when_some(description, |g, d| {
            g.child(div().text_sm().text_color(cx.theme().muted_foreground).child(label(SharedString::from(format!("{id}-desc")), d)))
        })
        .child(boxed(rows, cx))
}

/// Rows in one bordered box, divided by lines.
fn boxed(rows: Vec<AnyElement>, cx: &App) -> Div {
    let border = cx.theme().border;
    v_flex()
        .mt_1()
        .w_full()
        .border_1()
        .border_color(border)
        .rounded(cx.theme().radius)
        .bg(cx.theme().secondary)
        .children(rows.into_iter().enumerate().map(move |(i, row)| div().when(i > 0, |d| d.border_t_1().border_color(border)).child(row)))
}

/// One row: a title, a line under it, and controls on the right.
fn row(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    suffix: Vec<AnyElement>,
    title_color: Option<Hsla>,
    cx: &App,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    h_flex()
        .id(ElementId::Name(id.clone()))
        .w_full()
        .gap_3()
        .px_3()
        .py_2()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().when_some(title_color, |d, c| d.text_color(c)).child(label(ElementId::Name(format!("{id}-t").into()), title)))
                .when_some(subtitle, |v, s| {
                    v.child(div().text_sm().text_color(cx.theme().muted_foreground).child(label(ElementId::Name(format!("{id}-s").into()), s)))
                }),
        )
        .children(suffix)
}

/// A small coloured word next to a row (Signed, running, EOL, ...).
fn badge(id: impl Into<SharedString>, text: &'static str, color: Hsla) -> AnyElement {
    let id: SharedString = id.into();
    div().flex_none().text_xs().text_color(color).child(label(ElementId::Name(id), text)).into_any_element()
}

/// A full-width notice at the top of a page.
fn banner(id: &'static str, text: &str, cx: &App) -> AnyElement {
    div()
        .id(id)
        .role(Role::Status)
        .w_full()
        .px_3()
        .py_2()
        .bg(cx.theme().warning.opacity(0.18))
        .text_sm()
        .child(label(SharedString::from(format!("{id}-text")), text.to_string()))
        .into_any_element()
}

fn page_scroll(id: &'static str) -> Stateful<Div> {
    div().id(id).flex_1().min_h_0().overflow_y_scroll()
}

impl MainView {
    fn render_system(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.borrow();
        let info = &s.sysinfo;
        let usable = s.busy.is_none() && !info.unsupported_system;
        let theme = cx.theme().clone();

        let mut kernel_rows: Vec<AnyElement> = Vec::new();
        for k in &info.kernels {
            let ver = k.version.clone();
            let key = |part: &str| SharedString::from(format!("{part}:{ver}"));
            if info.unsupported_system {
                // The checks below look for Debian's files: say nothing rather than raise a false alarm.
                kernel_rows.push(row(key("kernel"), ver.clone(), Some("installed in /boot".into()), vec![], None, cx).into_any_element());
                continue;
            }
            let mut suffix = Vec::new();
            // Signature badge, only once signing is set up.
            if let Some(sig) = k.signed {
                let (text, color) = match sig {
                    KernelSignature::KernelPop => ("Signed", theme.success),
                    KernelSignature::Distro => ("Signed by Ubuntu", theme.success),
                    KernelSignature::Unsigned => ("Unsigned", theme.warning),
                };
                suffix.push(badge(key("sig"), text, color));
            }
            if k.signed == Some(KernelSignature::Unsigned) {
                let v = ver.clone();
                suffix.push(
                    Button::new(ElementId::Name(key("sign")))
                        .ghost()
                        .small()
                        .label("Sign")
                        .tooltip("Sign this kernel with the Kernel Pop key")
                        .accessibility_label(format!("Sign {ver}"))
                        .disabled(!usable)
                        .on_click(cx.listener(move |this, _, window, cx| this.sign_clicked(v.clone(), window, cx)))
                        .into_any_element(),
                );
            }
            if k.running {
                suffix.push(badge(key("running"), "running", theme.success));
            } else {
                let (v1, v2) = (ver.clone(), ver.clone());
                suffix.push(
                    Button::new(ElementId::Name(key("boot")))
                        .ghost()
                        .small()
                        .label("Boot Next Time")
                        .tooltip("Boot this kernel on the next restart only; the default is unchanged")
                        .accessibility_label(format!("Boot {ver} next time"))
                        .disabled(!usable)
                        .on_click(cx.listener(move |this, _, window, cx| this.boot_once_clicked(v1.clone(), window, cx)))
                        .into_any_element(),
                );
                suffix.push(
                    Button::new(ElementId::Name(key("remove")))
                        .ghost()
                        .small()
                        .text_color(theme.danger)
                        .label("Remove")
                        .accessibility_label(format!("Remove {ver}"))
                        .disabled(!usable)
                        .on_click(cx.listener(move |this, _, window, cx| this.remove_clicked(v2.clone(), window, cx)))
                        .into_any_element(),
                );
            }
            let color = (!k.healthy()).then_some(theme.danger);
            kernel_rows.push(row(key("kernel"), ver.clone(), Some(kernel_subtitle(k).into()), suffix, color, cx).into_any_element());
        }
        if info.kernels.is_empty() {
            let text = if self.sys_loaded { "No kernels found in /boot" } else { "Checking…" };
            kernel_rows.push(row("no-kernels", text, None, vec![], None, cx).into_any_element());
        }

        let checking = |text: String| -> Option<SharedString> { Some(if self.sys_loaded { text.into() } else { "Checking…".into() }) };
        let mut safety =
            vec![row("secure-boot", "Secure Boot", checking(secure_boot_subtitle(info.secure_boot, info.kernel_signing).into()), vec![], None, cx)
                .into_any_element()];
        if let Some(text) = boot_path_subtitle(&info.boot_path).filter(|_| self.sys_loaded) {
            safety.push(row("boot-path", "Boot path", Some(text.into()), vec![], None, cx).into_any_element());
        }
        let setup_button = (self.sys_loaded && offers_signing_setup(info)).then(|| {
            Button::new("setup-signing")
                .outline()
                .small()
                .label("Set Up Signing")
                .tooltip(
                    "Create a signing key, put shim in front of systemd-boot where it is missing, sign the installed kernels and queue the key for enrollment",
                )
                .disabled(!usable)
                .on_click(cx.listener(|this, _, window, cx| this.setup_signing_clicked(window, cx)))
                .into_any_element()
        });
        safety.push(row("signing", "Kernel signing", checking(signing_text(info)), setup_button.into_iter().collect(), None, cx).into_any_element());

        let boot_text = match info.free_boot_bytes {
            Some(f) if info.boot_low() => format!("Low: {} free, a new kernel needs about {}", format_bytes(f), format_bytes(info.boot_needed_bytes)),
            Some(f) => format!("{} free", format_bytes(f)),
            None => "Could not determine".into(),
        };
        let root_text = match info.free_root_bytes {
            Some(f) if f < MIN_ROOT_BYTES => format!("Low: {} free (2 GB recommended)", format_bytes(f)),
            Some(f) => format!("{} free", format_bytes(f)),
            None => "Could not determine".into(),
        };
        let disk = vec![
            row("boot-free", "/boot Free Space", checking(boot_text), vec![], None, cx).into_any_element(),
            row("root-free", "/ Free Space", checking(root_text), vec![], None, cx).into_any_element(),
        ];
        let running = vec![row("running", self.running_title.clone(), self.sys_loaded.then(|| "uname -r".into()), vec![], None, cx).into_any_element()];
        let update_banner = self.update_banner.clone();
        drop(s);

        page_scroll("system-page")
            .child(
                v_flex()
                    .p_3()
                    .pb_6()
                    .gap_4()
                    .when_some(update_banner, |v, text| v.child(banner("update-banner", &text, cx)))
                    .child(group("running-group", "Running Kernel", None, running, cx))
                    .child(group(
                        "kernels-group",
                        "Installed Kernels",
                        Some("Every kernel in /boot, checked for a matching initramfs and modules directory"),
                        kernel_rows,
                        cx,
                    ))
                    .child(group("safety-group", "Boot Safety", None, safety, cx))
                    .child(group("disk-group", "Disk", None, disk, cx))
                    .child(
                        h_flex()
                            .justify_center()
                            .gap_2()
                            .child(
                                Button::new("refresh-sysinfo")
                                    .ghost()
                                    .label("Refresh System Info")
                                    .on_click(cx.listener(|this, _, window, cx| this.load_sysinfo(window, cx))),
                            )
                            .child(
                                Button::new("prune")
                                    .ghost()
                                    .label("Remove Old Kernels…")
                                    .disabled(!usable)
                                    .on_click(cx.listener(|this, _, window, cx| this.prune_clicked(window, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_browse(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.borrow();
        let filter = self.filter.read(cx).value().to_string();
        let running = s.sysinfo.running_kernel.clone();
        let selected = s.selected_release.as_ref().map(|r| r.version.clone());
        let theme = cx.theme().clone();
        let rows: Vec<AnyElement> = s
            .releases
            .iter()
            .filter(|rel| release_matches(rel, &filter))
            .map(|rel| {
                let version = rel.version.clone();
                let is_running = compare_to_running(&running, &rel.version) == VersionRelation::Same;
                let pick = version.clone();
                h_flex()
                    .id(ElementId::Name(format!("release:{version}").into()))
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_3()
                    .when(selected.as_deref() == Some(version.as_str()), |r| r.bg(theme.accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_release(&pick);
                        cx.notify();
                    }))
                    .child(
                        Radio::new(ElementId::Name(format!("pick:{version}").into()))
                            .flex_1()
                            .checked(selected.as_deref() == Some(version.as_str()))
                            .label(version.clone())
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if is_running { theme.success } else { theme.muted_foreground })
                                    .child(label(ElementId::Name(format!("d:{version}").into()), release_subtitle(rel, &running))),
                            )
                            .on_click({
                                let pick = version.clone();
                                cx.listener(move |this, _, _, cx| {
                                    this.select_release(&pick);
                                    cx.notify();
                                })
                            }),
                    )
                    .when(rel.is_eol, |r| r.child(badge(format!("eol:{version}"), "EOL", theme.warning)))
                    .into_any_element()
            })
            .collect();
        let has_selection = s.selected_release.is_some();
        let usable = s.busy.is_none() && !s.sysinfo.unsupported_system;
        drop(s);
        v_flex()
            .flex_1()
            .min_h_0()
            .when_some(self.browse_error.clone(), |v, text| v.child(banner("browse-banner", &text, cx)))
            .child(div().px_3().pt_3().child(Input::new(&self.filter).aria_label("Filter releases")))
            .child(
                page_scroll("browse-list").child(
                    div()
                        .p_3()
                        .when(!rows.is_empty(), |d| {
                            d.child(v_flex().id("releases").role(Role::RadioGroup).aria_label("kernel.org releases").child(boxed(rows, cx)))
                        })
                        .when(self.releases_loading, |d| d.child(h_flex().justify_center().p_4().child(Spinner::new().large()))),
                ),
            )
            .child(div().text_center().text_sm().text_color(theme.muted_foreground).child(label("selected", self.selected_text.clone())))
            .child(
                h_flex().justify_end().p_3().border_t_1().border_color(theme.border).child(
                    Button::new("download")
                        .primary()
                        .label("Download Source")
                        .tooltip("Download the tarball and its signature from cdn.kernel.org to ~/Downloads/kernel-pop")
                        .disabled(!(usable && has_selection))
                        .on_click(cx.listener(|this, _, window, cx| this.download_clicked(window, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_build(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.borrow();
        let idle = s.busy.is_none();
        let usable = idle && !s.sysinfo.unsupported_system;
        let theme = cx.theme().clone();

        let (source_title, source_sub): (SharedString, SharedString) = match &s.source {
            Some(t) => (format!("Linux {}", t.version.kernelversion()).into(), t.origin.display().to_string().into()),
            None => ("No source selected".into(), "Its version is read from the tarball's Makefile".into()),
        };
        let (sig_text, sig_color): (SharedString, Option<Hsla>) = match &s.signature {
            SigState::None => ("—".into(), None),
            SigState::Checking => ("Checking against the kernel.org keys…".into(), None),
            SigState::Verified(signer) => {
                (format!("Good signature from {} ({}), key fetched from kernel.org", signer.uid, signer.grouped_fingerprint()).into(), Some(theme.success))
            }
            SigState::Failed(why) => (format!("NOT VERIFIED — {why}").into(), Some(theme.danger)),
        };
        let failed = matches!(s.signature, SigState::Failed(_));
        let mut source_rows = vec![row(
            "source",
            source_title,
            Some(source_sub),
            vec![Button::new("open-tarball")
                .ghost()
                .small()
                .label("Open Tarball…")
                .disabled(!idle)
                .on_click(cx.listener(|this, _, window, cx| this.open_tarball_clicked(window, cx)))
                .into_any_element()],
            None,
            cx,
        )
        .into_any_element()];
        let recheck = failed.then(|| {
            Button::new("recheck")
                .ghost()
                .small()
                .label("Check Again")
                .disabled(!idle)
                .on_click(cx.listener(|this, _, window, cx| {
                    // The user's file, not the private copy: set_source copies it
                    // into the private directory again.
                    let path = this.state.borrow().source.as_ref().map(|t| t.origin.clone());
                    if let Some(p) = path {
                        this.set_source(p, window, cx);
                    }
                }))
                .into_any_element()
        });
        source_rows.push(
            h_flex()
                .id("signature")
                .w_full()
                .gap_3()
                .px_3()
                .py_2()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(label("signature-t", "Signature"))
                        .child(div().text_sm().text_color(sig_color.unwrap_or(theme.muted_foreground)).child(label("signature-s", sig_text))),
                )
                .children(recheck)
                .into_any_element(),
        );
        if failed {
            source_rows.push(
                row(
                    "unverified",
                    "Build unverified source",
                    Some("For your own patched tree only. The build log and the package folder record that the source was not verified.".into()),
                    vec![Switch::new("unverified-switch")
                        .checked(s.allow_unverified)
                        .accessibility_label("Build unverified source")
                        .disabled(!idle)
                        .on_click(cx.listener(|this, want: &bool, window, cx| this.unverified_toggled(*want, window, cx)))
                        .into_any_element()],
                    None,
                    cx,
                )
                .into_any_element(),
            );
        }

        let mode = s.mode;
        let mode_row = |m: ModeChoice, id: &'static str, title: &'static str, sub: SharedString, cx: &mut Context<Self>| -> AnyElement {
            div()
                .px_3()
                .py_2()
                .child(
                    Radio::new(id)
                        .w_full()
                        .checked(mode == m)
                        .disabled(!idle)
                        .label(title)
                        .child(div().pr_6().text_sm().text_color(theme.muted_foreground).child(label(SharedString::from(format!("{id}-s")), sub)))
                        .on_click(cx.listener(move |this, _, window, cx| this.set_mode(m, window, cx))),
                )
                .into_any_element()
        };
        let mut config_rows = vec![
            mode_row(ModeChoice::Match, "mode-match", "Match this system", "Start from the running kernel's /boot config, so everything that works now keeps working".into(), cx),
            mode_row(
                ModeChoice::Tuned,
                "mode-tuned",
                "Tuned for this machine",
                "Drivers for the hardware in this computer plus the kinds people plug in (USB storage, controllers, headsets, webcams, docks, VPN and container networking). Builds in a fraction of the time.".into(),
                cx,
            ),
        ];
        config_rows.push(
            h_flex()
                .w_full()
                .child(div().flex_1().child(mode_row(ModeChoice::Own, "mode-own", "Use my own .config", self.own_config_text.clone().into(), cx)))
                .child(
                    div().pr_3().child(
                        Button::new("choose-config")
                            .ghost()
                            .small()
                            .label("Choose…")
                            .disabled(!idle)
                            .on_click(cx.listener(|this, _, window, cx| this.choose_config_clicked(window, cx))),
                    ),
                )
                .into_any_element(),
        );

        let deps_text: SharedString =
            if s.sysinfo.unsupported_system { "Not checked: building needs a Debian- or Ubuntu-based system".into() } else { self.deps_text.clone().into() };
        let deps_button = (!s.missing_deps.is_empty()).then(|| {
            Button::new("install-deps")
                .ghost()
                .small()
                .label("Install…")
                .tooltip("Install the missing build tools with apt (asks for authentication)")
                .disabled(!usable)
                .on_click(cx.listener(|this, _, window, cx| this.install_deps_clicked(window, cx)))
                .into_any_element()
        });
        let req_rows = vec![
            row("deps", "Build tools", Some(deps_text), deps_button.into_iter().collect(), None, cx).into_any_element(),
            row("space", "Disk space", Some(self.space_text.clone().into()), vec![], None, cx).into_any_element(),
        ];

        // The progress bar: the download's, or the build's from the stage and
        // the objects compiled.
        let bar: Option<(Option<f32>, String)> = if let Some(d) = &self.download_bar {
            Some((d.fraction, d.text.clone()))
        } else if s.build_control.is_some() {
            match (s.build_started, self.build_fraction) {
                (None, f) => Some((f.map(|f| f as f32), "Waiting for you to confirm the configuration".into())),
                (Some(_), Some(f)) => Some((Some(f as f32), format!("{:.0}%", (f * 100.0).floor()))),
                (Some(_), None) => Some((None, String::new())),
            }
        } else {
            None
        };
        let can_build = usable && s.can_build();
        let cancel_label = if s.build_control.is_some() { "Cancel Build" } else { "Cancel Download" };
        let cancellable = s.download_cancel.is_some() || s.build_control.is_some();
        drop(s);

        page_scroll("build-page")
            .child(
                v_flex()
                    .p_3()
                    .pb_6()
                    .gap_4()
                    .child(group(
                        "source-group",
                        "Kernel Source",
                        Some("Pick a release on the Browse tab, open a tarball, or drop a linux-*.tar.xz or .tar.gz onto this window"),
                        source_rows,
                        cx,
                    ))
                    .child(group(
                        "config-group",
                        "Configuration",
                        None,
                        vec![v_flex().id("modes").role(Role::RadioGroup).aria_label("Configuration").children(config_rows).into_any_element()],
                        cx,
                    ))
                    .child(group("req-group", "Requirements", None, req_rows, cx))
                    .when_some(self.stage_text.clone(), |v, stage| {
                        v.child(div().text_center().text_lg().font_weight(FontWeight::SEMIBOLD).child(label("stage", stage)))
                    })
                    .when_some(bar, |v, (fraction, text)| {
                        v.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Progress::new("build-progress")
                                        .value(fraction.unwrap_or(0.) * 100.)
                                        .loading(fraction.is_none())
                                        .accessibility_label(if text.is_empty() { "Building".to_string() } else { text.clone() }),
                                )
                                .child(div().text_center().text_sm().child(label("progress-text", text))),
                        )
                    })
                    .child(
                        h_flex()
                            .justify_center()
                            .gap_2()
                            .child(
                                Button::new("build")
                                    .primary()
                                    .label("Build Kernel")
                                    .disabled(!can_build)
                                    .on_click(cx.listener(|this, _, window, cx| this.build_clicked(window, cx))),
                            )
                            .when(cancellable, |h| {
                                h.child(Button::new("cancel-run").danger().label(cancel_label).on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_clicked();
                                    cx.notify();
                                })))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_install(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.borrow();
        let idle = s.busy.is_none();
        let usable = idle && !s.sysinfo.unsupported_system;
        let theme = cx.theme().clone();
        let staged_dir = s.staged.as_ref().map(|set| set.dir.clone());

        let built_rows: Vec<AnyElement> = if s.built.is_empty() {
            vec![row("no-builds", "No builds yet", Some("Build a kernel on the Build tab".into()), vec![], None, cx).into_any_element()]
        } else {
            s.built
                .iter()
                .enumerate()
                .map(|(i, set)| {
                    let key = set.dir.display().to_string();
                    let checked = staged_dir.as_ref() == Some(&set.dir);
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_2()
                        .gap_3()
                        .child(
                            Radio::new(ElementId::Name(format!("built:{key}").into()))
                                .flex_1()
                                .checked(checked)
                                .disabled(!idle)
                                .label(format!("Linux {}", set.kernelrelease))
                                .child(
                                    div().text_sm().text_color(theme.muted_foreground).child(label(ElementId::Name(format!("bs:{key}").into()), key.clone())),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| this.select_built(i, window, cx))),
                        )
                        .when(set.source.starts_with("UNVERIFIED"), |r| r.child(badge(format!("unv:{key}"), "unverified source", theme.warning)))
                        .into_any_element()
                })
                .collect()
        };

        let delete = s.staged.is_some().then(|| {
            Button::new("delete-build")
                .ghost()
                .small()
                .text_color(theme.danger)
                .label("Delete")
                .tooltip("Delete this build's packages from ~/Downloads/kernel-pop")
                .disabled(!idle)
                .on_click(cx.listener(|this, _, window, cx| this.delete_built_clicked(window, cx)))
                .into_any_element()
        });
        let (staged_title, staged_sub, status, source_info): (SharedString, SharedString, SharedString, SharedString) = match &s.staged {
            Some(set) => {
                let ver = &set.kernelrelease;
                let running = &s.sysinfo.running_kernel;
                let status = match compare_to_running(running, ver) {
                    VersionRelation::Newer => format!("Upgrade: {running} → {ver}"),
                    VersionRelation::Same => format!("Same numeric version as running ({running})"),
                    VersionRelation::Older => format!("Downgrade: {running} → {ver}"),
                    VersionRelation::Unknown => "—".into(),
                };
                (
                    format!("Linux {ver}").into(),
                    set.dir.display().to_string().into(),
                    status.into(),
                    if set.source.is_empty() { "—".into() } else { set.source.clone().into() },
                )
            }
            None => ("Nothing selected".into(), "Build a kernel on the Build tab".into(), "—".into(), "—".into()),
        };
        let staged_rows = vec![
            row("staged", staged_title, Some(staged_sub), delete.into_iter().collect(), None, cx).into_any_element(),
            row("status", "Version Status", Some(status), vec![], None, cx).into_any_element(),
            row("source-info", "Source", Some(source_info), vec![], None, cx).into_any_element(),
        ];
        let file_rows: Vec<AnyElement> = s
            .staged
            .iter()
            .flat_map(|set| set.debs.iter())
            .map(|fname| {
                let (text, color) = if s.staged_verified { ("SHA256 ✓", theme.success) } else { ("checking…", theme.muted_foreground) };
                row(SharedString::from(format!("deb:{fname}")), fname.clone(), None, vec![badge(format!("sum:{fname}"), text, color)], None, cx)
                    .into_any_element()
            })
            .collect();
        let can_install = usable && s.staged.is_some() && s.staged_verified;
        drop(s);

        page_scroll("install-page")
            .child(
                v_flex()
                    .p_3()
                    .pb_6()
                    .gap_4()
                    .child(group("built-group", "Built Kernels", Some("Finished builds in ~/Downloads/kernel-pop"), vec![v_flex().id("builds").role(Role::RadioGroup).aria_label("Built kernels").children(built_rows).into_any_element()], cx))
                    .child(group("staged-group", "Selected Kernel", None, staged_rows, cx))
                    .when(!file_rows.is_empty(), |v| v.child(group("files-group", "Packages", Some("Each one checked against the SHA256SUMS manifest written when it was built"), file_rows, cx)))
                    .child(group(
                        "steps-group",
                        "Install Will",
                        None,
                        vec![row(
                            "steps",
                            "Check the packages again as root, dpkg install, build DKMS modules, then generate and VERIFY the initramfs, then update the boot loader",
                            Some("The install fails loudly if a package does not match the manifest, or if initrd.img or the boot menu entry does not appear — no silent unbootable kernels. Only the kernel image and headers are installed, never linux-libc-dev.".into()),
                            vec![],
                            None,
                            cx,
                        )
                        .into_any_element()],
                        cx,
                    ))
                    .when(self.installing, |v| {
                        v.child(
                            v_flex()
                                .gap_1()
                                .child(Progress::new("install-progress").loading(true).accessibility_label("Installing"))
                                .child(div().text_center().text_sm().child(label("install-text", "Installing… initramfs is generated and verified before success"))),
                        )
                    })
                    .child(h_flex().justify_center().child(Button::new("install").primary().label("Install Kernel").disabled(!can_install).on_click(cx.listener(|this, _, window, cx| this.install_clicked(window, cx))))),
            )
            .into_any_element()
    }

    fn render_log(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .p_2()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(Textarea::new(&self.log_view).readonly(true).h_full().aria_label("Log")),
            )
            .child(h_flex().justify_end().child(Button::new("clear-log").ghost().label("Clear Log").on_click(cx.listener(|this, _, _, cx| {
                this.log_lines.clear();
                this.log_dirty = true;
                cx.notify();
            }))))
            .into_any_element()
    }
}

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page_index = PAGES.iter().position(|(p, _)| *p == self.page).unwrap_or(0);
        let title_bar = TitleBar::new()
            // The close button goes through the same guard as the desktop's close
            // request; left alone the toolkit removes the window straight away.
            .on_close_window(cx.listener(|this, _, window, cx| {
                if this.may_close(window, cx) {
                    window.remove_window();
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    // Clicks on the buttons must not start a window drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("refresh-releases")
                            .ghost()
                            .small()
                            .icon(Icon::new(gpui_kit::assets::IconName::RefreshCw))
                            .tooltip("Refresh kernel list")
                            .accessibility_label("Refresh kernel list")
                            .on_click(cx.listener(|this, _, window, cx| this.load_releases(window, cx))),
                    ),
            )
            .child(div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
                TabBar::new("pages").segmented().small().selected_index(page_index).children(PAGES.iter().map(|(_, name)| *name)).on_click(cx.listener(
                    |this, index: &usize, _, cx| {
                        this.page = PAGES.get(*index).map(|(p, _)| *p).unwrap_or(Page::System);
                        cx.notify();
                    },
                )),
            ))
            .child(
                h_flex().px_2().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
                    Button::new("about")
                        .ghost()
                        .small()
                        .icon(Icon::new(gpui_kit::component::IconName::Info))
                        .tooltip("About")
                        .accessibility_label("About")
                        .on_click(cx.listener(|this, _, window, cx| this.show_about(window, cx))),
                ),
            );

        let content = match self.page {
            Page::System => self.render_system(cx),
            Page::Browse => self.render_browse(cx),
            Page::Build => self.render_build(cx),
            Page::Install => self.render_install(cx),
            Page::Log => self.render_log(cx),
        };

        // Drop a tarball anywhere on the window.
        v_flex()
            .id("main")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(title_bar)
            .when_some(self.health_banner.clone(), |v, text| v.child(banner("health-banner", &text, cx)))
            .when_some(self.helper_banner.clone(), |v, text| v.child(banner("helper-banner", &text, cx)))
            .child(content)
            .child(Announcer(ANNOUNCEMENT.with(|text| text.borrow().clone())))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.drop_received(paths, window, cx)))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  About
// ─────────────────────────────────────────────────────────────────────────────

const ABOUT_TITLE: &str = "About Kernel Pop";

impl MainView {
    fn show_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (running, count) = {
            let s = self.state.borrow();
            (s.sysinfo.running_kernel.clone(), s.sysinfo.kernels.len())
        };
        let icon = Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../data/kernel-pop-128.png").to_vec()));
        let focus = InitialFocus::new(cx);
        window.open_dialog(cx, move |dialog, window, cx| {
            focus.apply(window, cx);
            let muted = cx.theme().muted_foreground;
            dialog.title(dialog_title(ABOUT_TITLE)).overlay(false).w(px(420.)).child(
                named_group("about", ABOUT_TITLE)
                    .child(
                        v_flex()
                            .id("body")
                            .max_h(dialog_room(window))
                            .overflow_y_scroll()
                            .gap_1()
                            .items_center()
                            .child(img(icon.clone()).w(px(64.)).h(px(64.)))
                            .child(div().text_xl().font_weight(FontWeight::BOLD).child(label("name", "Kernel Pop")))
                            .child(div().text_color(muted).child(label("version", env!("CARGO_PKG_VERSION"))))
                            .child(
                                div()
                                    .text_center()
                                    .child(label("summary", "Builds kernel.org kernels for this machine and installs them with a verified initramfs.")),
                            )
                            .child(div().text_center().text_sm().child(label("running", format!("Running kernel: {running}"))))
                            .child(div().text_center().text_sm().child(label("installed", format!("Installed kernels: {count}"))))
                            .child(
                                h_flex()
                                    .gap_4()
                                    .child(Link::new("about-website").href(WEBSITE).child("Website"))
                                    .child(Link::new("about-issues").href(ISSUES).child("Report an Issue")),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .items_center()
                                    .text_sm()
                                    .text_color(muted)
                                    .text_center()
                                    .child(label("developer", "Developer: Linnard Alex Brown Jr."))
                                    .child(label("built-with", "Built with: Claude Code (Anthropic), Codex (OpenAI)"))
                                    .child(label("license", "License: AGPL-3.0-or-later")),
                            ),
                    )
                    .child(
                        dialog_buttons()
                            .child(Button::new("about-close").track_focus(&focus.handle).label("Close").on_click(|_, window, cx| window.close_dialog(cx))),
                    ),
            )
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Set Up Signing: the password dialog's fields
// ─────────────────────────────────────────────────────────────────────────────

const SIGNING_TITLE: &str = "Set up kernel signing";

const SIGNING_TEXT: &str = "Choose a one-time password for enrolling Kernel Pop's signing key, together with the DKMS key that the NVIDIA driver and other out-of-tree modules are signed with. Any key another app has already queued is kept in the same request. On the next reboot a blue MOK Manager screen appears: choose Enroll MOK and type this password once; it enrolls all of them. A password chosen earlier, here or in another app, no longer works. Turn Secure Boot on only after this screen shows the keys as enrolled.";

const PASSWORD_RULES: &str = "8 to 16 characters: letters, digits and symbols, no spaces";

/// The hint under the password fields.
fn password_hint(first: &str, second: &str) -> &'static str {
    match validate_mok_password(first, second) {
        Ok(()) => "Remember it: you type it once at the next reboot",
        Err(_) if first.is_empty() => PASSWORD_RULES,
        Err(msg) => msg,
    }
}

struct SigningPrompt {
    pw1: Entity<InputState>,
    pw2: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl SigningPrompt {
    fn new(submit: Submit, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pw1 = cx.new(|cx| InputState::new(window, cx).placeholder("One-time password").masked(true));
        let pw2 = cx.new(|cx| InputState::new(window, cx).placeholder("Same password again").masked(true));
        let changed = |_: &mut Self, _: &Entity<InputState>, event: &InputEvent, _: &mut Window, cx: &mut Context<Self>| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        };
        let first = cx.subscribe_in(&pw1, window, changed);
        // Enter in the second field sets up, once the password is good.
        let second = cx.subscribe_in(&pw2, window, move |this: &mut Self, _, event: &InputEvent, window, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter { .. } => {
                if let Some(password) = this.password_in(cx) {
                    submit(password, window, cx);
                }
            }
            _ => {}
        });
        Self { pw1, pw2, _subscriptions: vec![first, second] }
    }

    /// The password, when both fields hold the same valid one.
    fn password_in(&self, cx: &App) -> Option<String> {
        let (a, b) = (self.pw1.read(cx).value(), self.pw2.read(cx).value());
        validate_mok_password(&a, &b).is_ok().then(|| a.to_string())
    }
}

impl Render for SigningPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (a, b) = (self.pw1.read(cx).value().to_string(), self.pw2.read(cx).value().to_string());
        v_flex()
            .gap_3()
            .child(label("text", SIGNING_TEXT))
            .child(Input::new(&self.pw1).mask_toggle().aria_label("One-time password"))
            .child(Input::new(&self.pw2).mask_toggle().aria_label("Same password again"))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(label("hint", password_hint(&a, &b))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every busy state that runs the privileged script blocks closing the window; the others are a
    /// download, a signature check and a build (which is stopped first).
    #[test]
    fn every_privileged_busy_state_blocks_closing() {
        let source = include_str!("ui.rs");
        let labels: Vec<&str> = source.split("begin_busy(\"").skip(1).filter_map(|rest| rest.split('"').next()).collect();
        assert!(labels.len() >= 9, "found the busy states: {labels:?}");
        for label in labels {
            let unprivileged = ["download", "signature check", "build"].contains(&label);
            assert_eq!(PRIVILEGED_ACTIONS.contains(&label), !unprivileged, "{label}");
        }
    }

    #[test]
    fn the_update_note_points_at_download_only_where_it_works() {
        assert_eq!(update_banner_text("7.3.1", false), "Linux 7.3.1 (stable) is out — download it on the Browse tab to build it");
        let elsewhere = update_banner_text("7.3.1", true);
        assert!(elsewhere.contains("7.3.1") && !elsewhere.contains("download") && !elsewhere.contains("Browse"), "{elsewhere}");
    }

    #[test]
    fn the_window_fits_the_screen() {
        let fit = |w: f32, h: f32| {
            let s = initial_window_size(Some(size(px(w), px(h))));
            (s.width.as_f32(), s.height.as_f32())
        };
        assert_eq!(initial_window_size(None), size(px(860.), px(820.)));
        assert_eq!(fit(2560., 1440.), (860., 820.));
        // 1280x800 with a 44 px panel: the whole window stays above it.
        let (w, h) = fit(1280., 800.);
        assert!(w == 860. && h + 44. + 32. <= 800., "{h}");
        // Smaller than the layout works in: the minimum, not less.
        assert_eq!(fit(500., 400.), WINDOW_MIN);
    }

    /// WCAG contrast ratio of two sRGB colours given as 0xRRGGBB.
    fn contrast(a: u32, b: u32) -> f32 {
        let lum = |c: u32| {
            let ch = |v: u32| {
                let v = v as f32 / 255.;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(c >> 16 & 0xff) + 0.7152 * ch(c >> 8 & 0xff) + 0.0722 * ch(c & 0xff)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn unselected_radio_buttons_stand_out_from_the_background() {
        // The toolkit's default backgrounds: neutral-950 in dark, white in light.
        assert!(contrast(INPUT_BORDER_DARK, 0x0a0a0a) >= 3.0, "{}", contrast(INPUT_BORDER_DARK, 0x0a0a0a));
        assert!(contrast(INPUT_BORDER_LIGHT, 0xffffff) >= 3.0, "{}", contrast(INPUT_BORDER_LIGHT, 0xffffff));
        // What the toolkit had: far below that.
        assert!(contrast(0x2f2f2f, 0x0a0a0a) < 1.6 && contrast(0xe5e5e5, 0xffffff) < 1.3);
        assert_eq!(input_border(true), gpui_kit::rgb(INPUT_BORDER_DARK).into());
    }

    #[test]
    fn closing_waits_for_privileged_runs_and_stops_a_build_first() {
        for what in PRIVILEGED_ACTIONS {
            assert_eq!(close_decision(Some(what), false, false), CloseDecision::Refuse(what), "{what}");
        }
        assert_eq!(close_decision(Some("build"), true, false), CloseDecision::StopBuildFirst);
        assert_eq!(close_decision(None, true, false), CloseDecision::StopBuildFirst, "the build summary is up: the tree is configured");
        assert_eq!(close_decision(Some("download"), false, true), CloseDecision::StopDownloadFirst, "a download is stopped, and its .part deleted, first");
        assert_eq!(close_decision(Some("signature check"), false, false), CloseDecision::Close);
        assert_eq!(close_decision(None, false, false), CloseDecision::Close);
    }

    /// The title bar's close button must go through `may_close`: without
    /// `on_close_window` the toolkit removes the window itself. GPUI's test
    /// platform draws no window controls (it reports server-side decorations,
    /// and the toolkit shows its buttons only with client-side ones), so the
    /// click cannot be simulated; the wiring is checked in the source instead.
    #[test]
    fn the_close_button_asks_may_close() {
        let source = include_str!("ui.rs");
        let bar = source.split("let title_bar = TitleBar::new()").nth(1).expect("the title bar");
        let close = bar.split(".on_close_window(").nth(1).expect("the title bar sets on_close_window");
        let handler = &close[..close.find("}))").expect("the handler's end")];
        assert!(handler.contains("this.may_close(window, cx)") && handler.contains("window.remove_window()"), "{handler}");
        assert_eq!(source.matches("TitleBar::new()").count(), 3, "one title bar (the other two are this test's)");
    }

    #[test]
    fn enter_acts_as_the_focused_button() {
        assert_eq!(enter_key(true, false), EnterKey::Confirm);
        assert_eq!(enter_key(false, true), EnterKey::Cancel);
        assert_eq!(enter_key(false, false), EnterKey::Nothing);
    }

    #[test]
    fn the_no_window_message_names_the_real_cause() {
        assert!(no_window_message("x", true).contains("graphics driver"));
        assert!(no_window_message("x", false).contains("no display"));
    }

    #[test]
    fn fonts_are_the_first_installed_of_the_list() {
        let installed = vec!["Noto Sans".to_string(), "Cantarell".to_string()];
        assert_eq!(first_installed(&UI_FONTS, &installed), Some("Cantarell"));
        assert_eq!(first_installed(&MONO_FONTS, &installed), None);
    }

    #[test]
    fn build_progress_follows_the_stages_and_never_moves_back() {
        assert_eq!(stage_fraction(Stage::Extract), Some(0.0));
        assert_eq!(stage_fraction(Stage::Build), None);
        assert_eq!(progress_fraction(None, 0.5), Some(0.05 + 0.45));
        assert_eq!(progress_fraction(Some(0.95), 0.1), Some(0.95), "a late compile line never moves the bar back from packaging");
        assert_eq!(took(3725), "1:02:05");
    }

    #[test]
    fn password_hint_follows_the_rules() {
        assert_eq!(password_hint("", ""), PASSWORD_RULES);
        assert_eq!(password_hint("goodpass1", "goodpass1"), "Remember it: you type it once at the next reboot");
    }
}
