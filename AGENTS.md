# Kernel Pop (formerly MKI / Mainline Kernel Installer)

GPUI Kit GUI, written in Rust, that builds kernel.org kernels for
this machine and installs them. The user picks a release from kernel.org's
releases.json (or drops a source tarball onto the window); Kernel Pop checks
its signature against the kernel.org developers' keys (fetched over WKD),
configures it from the running kernel's config, builds Debian packages with
`make bindeb-pkg` as the user, and installs them through the privileged
script. Distributed as a single AppImage. The core safety feature: every
kernel in `/boot` is checked for a matching `initrd.img` and `/lib/modules`
directory, so a kernel that would fail to boot is flagged *before* the
reboot, not after. The flow, the verification procedure, the config fixes
and the debug-info measurement are in `docs/SOURCE_BUILD.md`.

## Naming convention

- Display name: "Kernel Pop" (window title, About dialog, desktop Name).
- Binary, crate, repo, AppImage filename, `.desktop` and icon filenames,
  installed paths and log: hyphenated lowercase `kernel-pop`
  (`kernel-pop-$VERSION-x86_64.AppImage`, `/usr/lib/kernel-pop/`,
  `/var/log/kernel-pop.log`).
- The app ID `io.github.labj1987.KernelPop` and the polkit action ids stay
  PascalCase and UNCHANGED.

## Module layout (`src/`)

- `main.rs` — entry point: the single-instance check, the stderr logger
  (`KERNEL_POP_LOG`), the shared Tokio runtime, then GPUI.
- `instance.rs` — one running copy per login: a socket in
  `$XDG_RUNTIME_DIR`; a second launch hands over its activation token and
  waits for the "ok" the UI loop sends once it handled it. Until the window has drawn its first
  frame (`UiUp`, set from `window.on_next_frame` in `build_ui`) the listener answers "starting" at
  once; a launch waits 2 s for any answer and, after "starting", 15 s in all for the "ok" (a cold
  start on a software renderer can take seconds). No answer at all: the running copy is reported as
  not responding, with its PID, through `notice.rs` as an error, exit 1. Only "starting" in those
  15 s: it is still starting, reported as information (`notice::show_info`), exit 0. A running copy
  that closes the connection unanswered (end of file, ECONNRESET, EPIPE: it was exiting) is asked
  again every 75 ms until nobody listens (this launch becomes the primary) or a copy answers; after
  2 s counted from the first such close, the launch exits quietly. `instance::release()` removes the
  socket file, then `shutdown(SHUT_RDWR)`s the listener (that order) and drops the `.alive` lock; it
  runs when the last window closes and at the start of `ui::no_window`, which then drops the
  `Activations` so queued launches get end of file and start afresh. The listener thread drops a
  connection accepted after the release. The primary holds an exclusive `flock` on
  `kernel-pop.sock.alive` until then, so AppRun (`instance_running`, a shared non-blocking probe, so
  two launchers don't see each other) can skip its setup and exec the binary at once. Without a
  socket (no `XDG_RUNTIME_DIR`, or the bind failed) the claim is not exclusive and startup leaves
  `.part` files alone. `activation.rs` raises the window with that token on Wayland
  (`xdg_activation_v1` on GPUI's own connection).
- `notice.rs` — errors and information with no window to show them in: zenity, kdialog,
  notify-send, the notification service via `gdbus call` (Pop!_OS COSMIC has none of the others),
  xmessage, in that order until one shows it. Errors: `--error` dialogs, critical notifications with
  no expiry. Information: `zenity --info`, `kdialog --msgbox`, normal urgency with the desktop's
  default expiry (`int32 -1` over gdbus). A dialog's stderr goes to an unlinked 0600 temporary file
  read after it exits (never a pipe read to its end: a child it left running would hold it), and
  "cannot open display" there means it showed nothing even though it ended with 1. AppRun's
  `libcheck_show` keeps the same chain, in the same order, in bash (`libcheck_dialog`: stderr to a
  `mktemp` file). Tests run every dialog program as a stub on a private PATH with `DISPLAY=:9999`.
- `ui.rs` — the GPUI Kit UI (see UI below): system/browse/build/install/log
  tabs, plus a window-wide drop target for tarballs. One global busy state
  (`AppState::busy`, `MainView::begin_busy` and its `BusyGuard`)
  serialises downloads, signature checks, builds and every privileged
  action: while one runs, every control that starts another is disabled,
  including the per-kernel buttons. Worker threads report through the
  `UiMsg` channel, drained every `UI_TICK` so a build's output cannot flood
  the window; the Log tab keeps the last `LOG_MAX_LINES` lines (the whole
  output is in build.log). Toasts for runs that can report problems come
  from `report_run`/`outcome_message`.
- `kernelorg.rs` — kernel.org's releases.json (mainline, stable, longterm;
  linux-next is left out) and the cdn.kernel.org `.tar.sign` URL for a
  version (`fetch_signature`, for a dropped tarball without one).
- `source.rs` — a tarball's compression (from its magic bytes) and kernel
  version (VERSION/PATCHLEVEL/SUBLEVEL/EXTRAVERSION from the top-level
  Makefile, streamed through `xz`/`gzip` only that far). Never from the file
  name. `SourceVersion` gives kernel.org's name (7.3), `make kernelversion`'s
  (7.3.0) and the Debian version (7.3.0-1, `~rcN` for a pre-release).
- `verify.rs` — the kernel.org signature check: WKD key fetch for
  torvalds@ and gregkh@kernel.org (and an @kernel.org signer the signature
  names) into a throwaway GNUPGHOME, export to a keyring, `gpgv` over the
  decompressed tar on stdin. `parse_gpgv_status` needs GOODSIG and VALIDSIG;
  expired/revoked keys are refused, and only a release signer
  (`SIGNER_ADDRESSES`) counts as verified. The check runs on the private
  copy `source::private_copy` makes, which is also what the build extracts.
- `build.rs` — `BUILD_DEPS` (equal to the script's allowlist, a test checks),
  the config modes (`ConfigMode`; Tuned feeds localmodconfig the loaded
  modules, the modalias-resolved drivers of every device and
  `TUNED_EXTRA_MODULES`),
  `prepare` (disk preflight, extract into `~/.cache/kernel-pop/build/`,
  `make prepare` + a dry run that counts the objects for the progress bar,
  configure, `ConfigSummary`) and `build` (`make bindeb-pkg`, publish to
  `~/Downloads/kernel-pop/linux-<release>/` with `SHA256SUMS`, the final
  config, build-info.txt and build.log, then delete the tree). Each command
  runs in its own process group so `BuildControl::cancel` kills all of it.
  Refuses to run as root.
- `download.rs` — downloads files with progress, cancel (the typed
  `Cancelled` error, which also ends a hung request or read at once) and retries, into `.part`
  files renamed when complete, with explicit 0755/0644 modes; `remove_stale_parts` (only the app's
  own `linux-*.tar.*.part` names, at startup when no download runs); `part_paths`/`remove_parts` (one
  download's own `.part` files, for closing); `sha256_file`.
- `system.rs` — inventories installed kernels and their boot health (the
  initrd/modules/boot-entry check described above; for kernelstub, whether
  the kernel is the `vmlinuz` or `vmlinuz.old` one and its ESP copy
  exists); disk space; Secure Boot detection; kernel-signing state
  (`KernelSigning`, from the key files in `/var/lib/kernel-pop/mok/` plus
  `mokutil --test-key`, whose "already in the enrollment request" means
  pending, and the shared list of waiting keys, see Kernel signing); per-kernel `KernelSignature` (Kernel Pop's
  signature from the size/mtime markers in `/var/lib/kernel-pop/signed/`,
  since `/boot/vmlinuz-*` is 0600, or Ubuntu's from the dpkg source
  package); install preflight warnings; which old kernels can be pruned.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec` and
  returns its output, in a session of its own (`without_terminal`: an app from a TTY-started
  Hyprland inherits that console, and pkexec would prompt there unseen); refuses to run when the installed helper differs from
  the bundled one (`helper_problem`); tells a cancelled authentication from
  a refused one and from a failed script; parses the output protocol into
  a `ScriptReport`. `--install-build-deps` takes only names from the
  script's `BUILD_DEPS_ALLOWED`. The signing setup's MOK password goes to the script on
  stdin, never in argv.

## UI (GPUI Kit)

- The toolkit is `gpui-kit`, pinned to an exact version in Cargo.toml (0.9.x releases weekly and
  pins GPUI itself as the `gpui-pre` snapshot crates). It draws with wgpu: Vulkan first, OpenGL ES
  through EGL second; Mesa's software drivers count, which matters here: the app must still open
  after a kernel or driver update left the NVIDIA driver broken (lavapipe, or GLES on llvmpipe).
  Bump it on purpose, then re-run the checks below. Its sources are the reference:
  `~/.cargo/registry/src/*/gpui-component-*`, `gpui-base-*`, `gpui-pre-*`.
- Shape: one view (`MainView`) holding the shared `AppState` (`Rc<RefCell<..>>`, so a `BusyGuard`
  dropped anywhere can clear it) and the per-tab display state, and one small view for dialog
  content that changes while open (`SigningPrompt`). Dialog builders run on every frame (`Fn`), so
  one-shot state travels in `Rc<RefCell<Option<..>>>`. `MainView::run` runs a future on Tokio and
  hands the result to a `&mut MainView` callback on the UI thread; `MainView::later` wraps view
  code for dialog buttons.
- Dialogs: always through `window.open_dialog` with `.overlay(false)` (the toolkit's dimming layer
  also paints the client-side shadow margin, a dark frame around the window on KDE and GNOME), the
  content in `named_group(..)`, the buttons in `dialog_buttons()` inside that group (not the
  toolkit's footer), the default button given the focus at opening, and the text above the
  buttons capped with `dialog_room(window)` and scrollable: the toolkit cuts off a dialog taller
  than the window. Yes/no questions go through `show_question` (`Question`: title, body, button
  texts, default button, destructive); what a button starts runs through `window.defer` after
  `close_dialog`, so a follow-up dialog is not the one that gets closed. A cancel action
  (`on_cancel`) also runs on Escape and the close button (the build summary discards the
  configured tree that way).
- Enter in a dialog: the toolkit sends Enter to the dialog's `on_ok` before the focused button
  sees it. Every dialog therefore sets `on_ok` and acts as the focused button (`enter_key`):
  confirm only when the action button has the focus, cancel on Cancel, and return false (stay
  open) anywhere else, which leaves Enter to a focused text field (the signing dialog's second
  password field submits on Enter).
- Nothing may show a notification before the window has the toolkit's root view (that is,
  from `MainView::new`): `push_notification` panics there. `show_toast` returns false instead,
  and the first reads run from `MainView::start`, called by `build_ui` after `open_window`.
- Screen readers: GPUI reports only elements with an id and a role. Use `label(id, text)` for
  text (never a bare string child in a dialog), `accessibility_label` on icon buttons and on
  buttons that repeat per row (they name the kernel), `aria_label` on inputs, and `toast()` for
  messages (it also feeds the `Announcer` live region; the toolkit's notifications are not
  announced). The toolkit's dialog cannot be named in 0.9.1; `named_group` plus the initial focus
  is the stand-in. Selectable lists (releases, built kernels, configuration mode) are `Radio`s
  in a `Role::RadioGroup`.
- Fonts: `ui::init` names installed families for the UI and fixed-width fonts before the toolkit
  starts; without that GPUI warns about ".SystemUIFont" on every Linux system. Nothing may log at
  warn level on a normal start (`KERNEL_POP_LOG=info` shows the renderer picked; `main.rs` keeps
  chatty dependencies at warn whatever the level, and `zbus` (its harmless "Failed to populate
  properties cache") and `gpui::window::a11y` ("set_focus called more than once" with a screen
  reader) at error).
- Theme: `follow_appearance` re-applies the light/dark theme and then sets the `input` colour
  (radio rings, checkbox and field borders) to one with at least 3:1 contrast; the toolkit's is
  nearly the background colour. The first window size is clamped to the display less room for
  panels (`initial_window_size`). On Wayland GPUI's output size is the mode divided by the integer
  `wl_output` scale (150% arrives as 2), so once the window knows its fractional scale the size is
  worked out again (`wayland_logical`, `rescaled_size`) when the window first learns that scale (a
  bounds observer: right after opening it is still 1), and the window resized once if the user has
  not (`refit`: armed only for the first 3 s and while the window keeps its first size, and only
  with a single output). The resize itself runs 300 ms later from a task, and once more 700 ms after
  that only if the first size came back: one made inside the bounds callback is lost, and KWin's
  configure with the old size undoes an immediate one. Never on X11, where GPUI already divides by the true scale. Every tab
  scrolls above its bottom buttons.
- File dialogs (portal) have no file-type filter and no starting folder in GPUI: `inspect` refuses
  anything that is not a kernel tarball after the pick and on drop.
- Closing: `may_close` (from `on_window_should_close`) refuses while a privileged action runs,
  cancels a running build and closes the window once it has cleaned up, and cancels a running
  download and closes once its `.part` file is gone (after 5 s it deletes that download's own
  `.part` files itself, `AppState::download_parts`, never a sweep of the folder). While closing,
  `begin_busy` starts nothing new (`busy_refusal`).
- Testing the UI without touching the machine: a scratch `HOME` whose
  `Downloads/kernel-pop/linux-<krel>/` holds small fake `.deb` files, a `SHA256SUMS` for them and
  a `build-info.txt` reaches the Install tab with a verified build; Browse, Download Source and
  Build reach the build summary dialog, whose Cancel (or Escape) deletes the configured tree. Never
  press Install Kernel, Sign, Boot Next Time, Remove's confirm, Set Up or Install… (build tools)
  on a dev machine: they run the privileged script for real. Run GUI checks on a private display
  (Xvfb + `dbus-run-session`) with a private `XDG_RUNTIME_DIR`. Renderer checks:
  `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json` (lavapipe), and with no Vulkan ICD at
  all plus `__EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json` (GLES on
  llvmpipe); `KERNEL_POP_LOG=info` names the adapter picked.
- Before a release, on GNOME, KDE Plasma, Hyprland and COSMIC: start, every tab, a drop, the
  dialogs above, light/dark switch, polkit prompt and its cancel / no-agent messages; and with no
  GPU: lavapipe, GLES only, no driver (the app's dialog), no loader (AppRun's message). Retake the
  screenshots in `docs/screenshots/` when a tab's look changes.

## Source and build verification

- Source: kernel.org signs the uncompressed tar; the `.tar.sign` is a
  detached signature. A missing or bad signature blocks the build; the only
  way past it is the per-tarball "Build unverified source" switch, which is
  reset whenever the source changes and is recorded in build.log,
  build-info.txt and the Install tab. No key or toolchain ships with the
  app (nothing that can go stale).
- Build: the app writes `SHA256SUMS` for the debs it built. The privileged
  script trusts nothing in the build directory: it enters it once, checks
  owner (`PKEXEC_UID`) and modes on each file it has opened
  (`copy_trusted_file`, perl: O_NOFOLLOW|O_NONBLOCK and fstat), copies the
  manifest and debs into a root-only dir under `/var/tmp`, checks every
  hash on that copy (a deb not in the manifest, or a manifest entry
  without its deb, is an error), and installs only the copy. The trust
  anchor is the user who built the packages and authenticated through
  pkexec.
- `deb_allowed` admits only `linux-image-*` (never `-dbg`) and
  `linux-headers-*`; `linux-libc-dev` would replace Ubuntu's own package and
  is refused in the script, not just left out by the app. Every package
  must belong to the one kernel `kver_from_debs` reads from the image
  package's Package field.

## Privileged script output

Every line starts with `[kernel-pop] `. The app reacts to markers at the
start of a line's text, so each is printed once per problem and never in
a summary: `DKMS WARNING:`, `SIGN WARNING:`, `BOOT PENDING:` (boots once
the key enrollment is confirmed). Summaries use `DKMS SUMMARY:`/`SIGN
SUMMARY:`.

## Secure Boot rule

No kernel that shim would refuse is ever left installed. With Secure Boot
enforcing, `--install` requires the key set, the sbsign tools and a key
that is enrolled or queued (`secure_boot_preflight`, before dpkg; the app
checks the same in `install_blocker` and refuses up front). If signing
then fails, `rollback_and_die` undoes the install and the run fails: new
packages are purged, a leftover `/lib/modules/<kver>` removed and their
boot entry removed (`remove_boot_entry`: no postrm hook removes a
systemd-boot entry); a version that was installed before (its
`/boot/vmlinuz-<kver>` is there; a rebuild, maybe of the running kernel) is
never purged: its kernel, System.map, config and modules are kept before
dpkg (`keep_installed_kernel`, hard links), its initramfs copied when
`/boot` has room (else generated again on restore), and moved back. The
copy is taken after the package lock wait and the interrupted-run repair.
Any other stop once `dpkg -i` has really run (`KEEP_ARMED`, set in
`dpkg_install`) and before the install finished puts it back too
(`kept_kernel_on_exit`, the EXIT trap); a stop before that only drops the
copy, and a stop by a signal (TERM, HUP, INT) never restores: the kept files
stay and their paths are logged. A restore that fails part way leaves the
rest where it was kept and names the paths. Everything the steps after dpkg need (an initramfs tool,
kernel-install or kernelstub) is checked before dpkg. A queued key is fine: the kernel
boots once MOK Manager enrolls it.

## Kernel signing

- Kernel Pop has its own kernel-signing key (`/var/lib/kernel-pop/mok/`,
  `signing.key` 0600, `signing.pem`/`signing.der` 0644, dir 0755). Never
  reuse `/var/lib/shim-signed/mok/MOK.priv`: its module-only EKU
  (1.3.6.1.4.1.2312.16.1.2) makes shim reject kernels signed with it. The
  certificate config sets `extendedKeyUsage = codeSigning`; the script
  tests check the OID is absent.
- The key set is built in a temp dir and published with one rename of
  that dir onto the key dir, so the key dir is never half-populated.
- `sbsign` takes the PEM cert, `mokutil` the DER. sbsign appends a
  signature; an already-signed image keeps its existing ones.
- With no key, `--install` only adds one "signing not set up" log line.
- Enrollment (`queue_key_enrollment`): shim keeps ONE pending request
  (MokNew) with ONE password (MokAuth); `mokutil --import` appends to it but
  replaces the password, and skips an already-queued key without updating
  it. So the script rebuilds the whole request every time: the certificates
  already waiting (parsed out of the MokNew EFI variable), Kernel Pop's key
  and the DKMS key (`/var/lib/shim-signed/mok/MOK.der`), minus anything
  already enrolled, under the password just chosen (`--revoke-import`, then
  one `--import`). A request holding non-certificate entries, an entry
  openssl can't read, or a signature list whose sizes don't add up (perl
  parser exit 2) is left alone, before anything is revoked; Kernel Pop's
  own key or the DKMS key unreadable is a hard error. A failed `--import`
  is retried once with the same hash file; if it fails again, whatever is
  left in MokNew is revoked, the shared list removed, and every key logged
  with CN and SHA1 as no longer waiting.
- `ensure_efivarfs` runs before anything is decided about MokNew (signing
  setup and `secure_boot_preflight`): with the EFI firmware dir present and
  efivars on the same filesystem as it (not mounted), it mounts efivarfs
  (`nosuid,nodev,noexec`) or dies.
- MokNew is root-only, so the app cannot see it with `mokutil --list-new`.
  The list of waiting keys is shared with other apps that rebuild the same
  request: `/var/lib/mok-pending/list` (dir 0755, file 0644, written tmp +
  mv). First line `moknew-size <bytes>` (`stat -c %s` of MokNew right after
  the import), then one lowercase SHA1 per line, every key in the request.
  Rewritten after every successful import, removed when nothing is queued.
  The app (`parse_pending_list`) trusts it only while MokNew exists with
  that size; `combine_kernel_signing`/`combine_key_enrollment` let
  `--test-key`'s Enrolled win over the list, and "EFI variables are not
  supported" on an EFI machine is `EfiVarsNotMounted`. Keep the format
  identical in every app that writes it.
- The script's test overrides (`KERNEL_POP_*`, listed at its top) exist
  only in the script; pkexec clears the environment, so they never reach a
  real run. The script tests sign a copied EFI binary with a throwaway key
  and stub mokutil, dpkg, apt-get and the boot loader tools; never run the
  real modes on a dev machine. CI sets
  `KERNEL_POP_REQUIRE_ALL_TESTS=1`, which turns a skipped test group into
  a failure.
- Set Up Signing order on systemd-boot: `install_shim_packages`
  (shim-signed, placeholder, fb/mm links), `setup_dkms_key`, sign kernels,
  `queue_key_enrollment`, and only then `setup_shim_chain` (ESP and
  firmware entries) and the hooks: a failed enrollment changes nothing on
  the ESP. Every ESP copy goes through `esp_install` (temp name + rename;
  failure stops before any firmware entry change). `sign_boot_loader`
  leaves a `.signed` systemd-boot alone when a package owns it and it
  carries a signature that is not ours (distribution-signed: shim trusts it
  without enrollment), and only keeps it on the ESP. Set Up Signing first
  runs `restore_distribution_sdboot` (from `setup_shim_chain`, so after the
  keys are queued): a `.signed` copy signed with Kernel Pop's key that
  differs from the md5sum its package recorded (signed over by a release
  before 2.2.3) gets its package reinstalled, which puts the
  distribution's copy back. A failed reinstall only logs. Never from the
  apt hook, which runs inside dpkg.
- systemd-boot entries are looked for on the ESP and then on the XBOOTLDR
  partition (`bootctl --print-boot-path`, `KERNEL_POP_XBOOTLDR` in tests);
  the paths in an entry are relative to the partition it is on
  (`sdboot_entry_root`). After an install the ESP copy is refreshed whether
  or not the kernel was signed (the package hook copied the initramfs from
  before DKMS) and every file the entry names must exist.
- `prune_esp_initrds <esp> <kver>`: after every kernel-install add and at the
  end of a systemd-boot install, initramfs files ("initrd", "initrd.img-*")
  in the entry's ESP directory that the entry does not name are removed
  (the package hook writes "initrd", Kernel Pop passes "initrd.img-<kver>").
- `sync_signed_to_esp` returns non-zero when the ESP copy is not known to
  be signed; `do_install` then calls `esp_copy_unsigned`, which rolls the
  install back with Secure Boot enforcing (a warning with it off), like a
  failed signing.
- shim in front of systemd-boot (`setup_shim_chain`, from
  `do_setup_signing` when `detect_bootloader` says systemd-boot): the
  layout of Ubuntu's systemd-boot postinst `install_shim` (shim/fb/mm in
  `EFI/<os-release ID>/`, shim at `EFI/BOOT/BOOTX64.EFI` only over a
  fallback loader that is ours (`fallback_is_ours`: none, shim, or
  systemd-boot; never Windows' or another distribution's, and then no
  fbx64 either; copied before `EFI/<vendor>`, so a run that fails part way
  still sees it as ours next time), UCS-2LE `BOOTX64.CSV` (written tmp + mv)
  byte-identical to install_shim's, firmware entry
  `<Vendor>` = `EFI/<vendor>/shimx64.efi` with load option
  `\EFI\systemd\systemd-bootx64.efi \0`), done by the script itself so it
  also works while GRUB's packages are installed (install_shim returns
  early then). Ubuntu ships `fbx64.efi`/`mmx64.efi` without the `.signed`
  names install_shim reads; setup links them. `shim-without-grub`
  (Provides grub-efi-amd64-signed, grub2-common = 999) is built with
  dpkg-deb only when GRUB's signed package is not installed. "Linux Boot
  Manager" entries are pruned only while `shim_sdboot_entries` finds a
  shim entry (efibootmgr prints the load option as UTF-16LE hex), so a
  machine is never left without a Linux entry. `sign_boot_loader` re-signs
  `systemd-bootx64.efi.signed` when the sha256 in
  `/var/lib/kernel-pop/systemd-boot-signed-from` no longer matches, then
  `bootctl update` and a direct ESP copy when bootctl skipped it.
- Hooks setup installs: `/etc/apt/apt.conf.d/99kernel-pop-boot-loader`
  (systemd-boot only, runs `--sign-boot-loader`) and
  `/etc/kernel/postinst.d/zzz-kernel-pop-sign` (runs `--sign-new-kernel`
  after `zz-systemd-boot` copied the kernel to the ESP; signs only an image
  with no signature at all, then `sync_signed_to_esp`). Both call the
  installed helper path and never fail the apt/dpkg run.
- `setup_dkms_key`: DKMS modules installed and no
  `/var/lib/shim-signed/mok/MOK.der` → `update-secureboot-policy
  --new-key` (installing shim-signed also makes it). Then every installed
  package whose modules (its dkms.conf `BUILT_MODULE_NAME`s under
  `updates/dkms/`) are not signed with that key gets `dkms install
  --force`, preceded by `dkms build --force` (Ubuntu's DKMS signs with it
  at build time; `install --force` alone puts the stored, unsigned build
  back). The check compares
  modinfo's `sig_key` with the cert's serial and subject key identifier,
  never the signer name: `update-secureboot-policy` reuses the same CN for
  every key it makes. Every kernel it rebuilt modules for then gets its
  initramfs generated again and copied to the ESP (`refresh_esp_copy`):
  dracut `force_drivers` and the like keep the old unsigned module there.
- The System tab's Boot path row (`system::parse_boot_path`) classifies the
  efibootmgr entry BootCurrent names: shim, systemd-boot started directly
  (Set Up Signing is offered), another loader without shim, or unknown (the
  `EFI\BOOT\BOOTX64.EFI` fallback path can hold either).
- Every package operation in the script goes through `apt_get` or
  `dpkg_install`, never `apt-get`/`dpkg -i` directly: they wait for a
  software updater holding dpkg's locks (`package_lock_holder` reads
  `/proc/locks`, no fuser needed) and print the wait on fd 3, the script's
  own output, since their callers send stdout to the log file. They also
  run `finish_interrupted_dpkg` first: with files in
  `/var/lib/dpkg/updates` or any `dpkg --audit` output (a run cut off by a
  crash or power cut), `dpkg --configure -a`, `apt-get install -f`, and
  `--reinstall` of packages whose status has the reinst-required flag. Every
apt repair (`install -f`, that `--reinstall`, the `install -f` after a failed
`dpkg -i`) is simulated first (`apt_removals`: `Remv` and `Purg` lines) and
not run when it would remove a package (after a failed `dpkg -i`, only the
new packages themselves may go, and only when they replaced nothing).

## Build process

The AppImage holds one binary, as cargo built it, and no libraries. The binary links glibc, libgcc_s,
libxcb, libxkbcommon and libxkbcommon-x11, and loads by name at run time libwayland-client (Wayland
sessions), libvulkan and/or libEGL (with libwayland-egl) and then the GPU driver's own libraries; it
loads no GTK, GLib, Qt, fontconfig or freetype. AppRun's `libcheck_run` checks this before any
password prompt: `ldd -r` on the binary must show no `not found` line and no unresolved symbol of the
binary itself (a system library's own lazy ones don't count; when ldd fails without naming anything,
it asks the loader directly), and `libcheck_runtime` looks in the loader cache (`ldconfig -p`, 64-bit
entries, plus `LD_LIBRARY_PATH`) for libwayland-client on a Wayland session and for at least one of
libvulkan and libEGL. A cache that does not list glibc (NixOS and the like) says nothing, so nothing
is claimed. The message names the glibc version needed and found, or the missing libraries with the
install command for dnf, zypper, apt or pacman (`libcheck_package` holds the package names: add a
row when the binary gains a library). `tests/libcheck.rs` runs the block in bash with a fake ldd,
ldconfig and os-release, and the `debian_based` block with stub dpkg/apt-get. Nothing sets
`LD_LIBRARY_PATH` or any toolkit variable, so the programs the app starts (pkexec, make, dpkg, tar)
get the user's environment as it is: plain `std::process::Command`.

`build-appimage.sh` builds the AppImage. It runs as an ordinary user and
writes only inside the checkout:
1. Only on a machine with no toolchain (no cargo, or pkg-config can't find xcb, xkbcommon,
   xkbcommon-x11 and fontconfig) does it install build deps via apt (through sudo when not
   root), the one part needing root: cargo, rustc, build-essential, pkg-config, `libxcb1-dev`,
   `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libfontconfig1-dev` (one crate probes it at build
   time; the binary does not load it), `zsync`, `wget`, `file` and `desktop-file-utils`. It then
   checks `wget`, `file` and `desktop-file-validate` exist and fails clearly if one is missing.
2. `cargo build --release --locked`, with the compiler pinned in
   `rust-toolchain.toml`.
3. Assembles the AppDir (binary, privileged script, polkit policy, desktop
   file, icon, appdata, generated `AppRun`) and runs `desktop-file-validate` on the desktop
   file. `scripts/third_party_licenses.py` writes the notices of every crate in the build
   (from `cargo metadata` and the crates' own licence files; for a crate without one, the standard
   text of each licence its SPDX expression names, kept in `scripts/spdx/`: the build fails when
   a text is missing there, so add it) to
   `usr/share/licenses/kernel-pop/THIRD-PARTY-LICENSES.txt`, next to the app's own `LICENSE`; CI
   runs it too. AppRun installs the script and policy to system paths through
   pkexec when they differ (only on a Debian-based system, `debian_based`),
   says so on stderr when that is cancelled or fails (`helper_install`: pkexec runs under
   `setsid -w` with stdin from /dev/null, so it never prompts on an inherited console; a missing
   pkexec is named with its package), and exports
   `KERNEL_POP_BUNDLE` so the app can detect the mismatch and refuse to
   run an old helper.
4. Downloads `appimagetool` (pinned 1.9.1) and the AppImage type2 runtime
   (pinned 20251108, passed with `--runtime-file`; otherwise appimagetool
   downloads the moving `continuous` build), both SHA256-verified and
   cached in `.cache/`, and packs the AppDir into
   `kernel-pop-$VERSION-x86_64.AppImage`, with
   `UPDATE_INFORMATION` set for `gh-releases-zsync` delta updates.
5. Runs `zsyncmake` directly on the built AppImage to produce the
   `.zsync` sidecar.

Rust is pinned in `rust-toolchain.toml`; bump it there and in the toolchain
steps of both workflows together (each workflow checks they agree). The
workflows' actions are pinned to full commit SHAs, and the weekly
maintenance round updates them (see `.github/dependabot.yml`).

**Gotcha (fixed in v1.0.6):** `appimagetool`'s own built-in zsync
generation silently no-ops on the GitHub Actions runner even when
`UPDATE_INFORMATION` is set and `zsync`/`zsyncmake` are installed and
working — it produces no `.zsync` file and prints no warning either way.
Best guess: this appimagetool build probes zsyncmake with a long-option
flag (e.g. `--version`) that the installed short-option-only zsyncmake
build rejects, and appimagetool treats that as "zsyncmake unavailable"
without logging it. Do not rely on appimagetool to generate the
`.zsync` — call `zsyncmake "$OUT"` directly right after packing, as the
script does now. That call is fatal when `CI` is set (a missing or failing
`zsyncmake` exits 1, because the update information points at a `.zsync`
and a release without one cannot update); a local build only warns.

Also note CI's "Set up Rust Toolchain" step means the script's
`command -v cargo` guard evaluates false there, so nothing inside it runs
in CI. The workflow's "Install build dependencies" step installs the UI toolkit's
build headers and the packaging tools (`zsync`, `wget`, `file`,
`desktop-file-utils`) instead, and the script checks the tools exist and
fails clearly if one is missing.

## Rust

- Toolchain: Rust 1.100 or newer. `rust-toolchain.toml` pins the exact release that local, CI and release builds use; `rust-version = "1.100"` in Cargo.toml is the minimum.
- Dependency cooldown: `.cargo/config.toml` sets `[registry] global-min-publish-age = "3 days"`, so resolution only takes crate versions at least 3 days old (Dependabot's cargo `cooldown` matches). To take a newer version early for a security advisory: `CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE=allow cargo update -p <crate> --precise <version>`. Until that version is 3 days old, a plain `cargo update` moves it back down, so run that round's `cargo update` with the same variable set (or update the other crates with `-p`).
- Never compare tool versions as text or floats ("1.100" sorts before "1.99" as text): split into integers, or use `sort -V`.
- Never read Cargo's intermediate layout under `target/` (`build/`, `deps/`, `incremental/`, `.fingerprint/`): it is not a stable interface and changed in Cargo 1.100. Use the final artifact paths, `OUT_DIR` inside build.rs, or `cargo build --message-format=json`.
- Cargo's own lints are on (`[lints.cargo]`, `unused_dependencies = "warn"`) and CI sets `CARGO_BUILD_WARNINGS=deny`, so a lint warning fails CI. A direct dependency kept only to turn on a feature of a crate already in the graph stays in `[dependencies]` with a comment saying why: the lint does not flag it, and moving it to a `cfg(false)` table would turn the feature off. (`[target.'cfg(false)'.dependencies]` is only for a dependency listed just to pin a transitive crate's version.)

## Test builds

- A fix for an issue someone else reported goes to them before it reaches `main`: push it on its own branch,
  carrying the version bump and CHANGELOG section it will be released with, open a PR (CI must pass), then run
  `gh workflow run release.yml --ref BRANCH -f issue=N`. The Build and Release workflow builds that branch
  exactly as a release and publishes it as a pre-release named "Test build for #N (BRANCH)" with the tag
  `test-BRANCH-RUN` (never `v*`, never the latest release, so the AppImage updater does not offer it).
- Link the pre-release in the issue and ask the reporter to try it. Merge and release only once they confirm
  it fixes the problem (or say plainly why you release without that). After the release, delete the test
  pre-releases and their tags (`gh release delete TAG --cleanup-tag --yes`).

## Release process

Dry run (the release build of a branch, nothing published, nobody notified): `gh workflow run release.yml --ref BRANCH -f dry_run=true`; the AppImage and .zsync are the run's `release-files` artifact.

1. Bump `version` in `Cargo.toml` (and let `Cargo.lock` follow).
2. Add a `CHANGELOG.md` entry (see Changelog below).
3. Run `python3 scripts/sync_appdata_releases.py` to regenerate the
   appdata `<releases>` list; CI and the release workflow fail if it is
   out of date.
4. Commit, push to `main` (`.github/workflows/ci.yml` builds, tests and
   shellchecks on push/PR).
5. `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"). Its `build` job has a read-only token: it checks the tag
   matches `Cargo.toml`, runs the same checks as CI (ShellCheck, script
   tests, `cargo test`, clippy), runs `build-appimage.sh` and
   uploads the AppImage, `.zsync` and release notes as a workflow
   artifact. A separate `publish` job, the only one with write access,
   attaches those files to a GitHub Release via
   `softprops/action-gh-release`, with that version's changelog section
   as the release text.

## Changelog

- One `## X.Y.Z — YYYY-MM-DD` heading per released version, newest
  first. No entries for builds that were never released.
- Write each entry for the people using the app: what changed for them
  and anything they need to do. Leave out implementation detail (file
  paths, flags, internal names, CI and packaging changes) unless a user
  needs it to act.
- The release page is the version's section written out in full
  (`scripts/release_notes.py`), never a link to the changelog. The
  release fails if the section is missing.
- The appdata `<releases>` list is generated from the headings
  (`scripts/sync_appdata_releases.py`). Don't edit it by hand.

## Conventions

- Every commit must be rustfmt-clean: run `cargo fmt --all` (settings in `rustfmt.toml`) before committing; CI runs
  `cargo fmt --all --check`. The formatting-only commit is listed in `.git-blame-ignore-revs`.
- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/kernel-pop` (GitHub repo: `labj1987/kernel-pop`), owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file
  ownership correct.

## Shared memory

The owner's Claude sessions (local, cloud and chat) share notes in the Google Drive folder "Claude Memory", one doc per topic; this repo's doc is "app: kernel-pop". If you have the Google Drive connector:

- Before starting work, read that doc and the "prefs:" docs for decisions, things already tried or ruled out, and the release state.
- After meaningful work (a release, a decision, a failed attempt worth not repeating, an open issue), update the doc. The connector can't edit a doc's text: create a replacement doc with the same title in that folder, then trash the old one. The folder's "README - how this folder works" doc has the conventions.
