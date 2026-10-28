# Changelog

One `## <version> — <date>` heading per released version, newest first. A release's page
carries its section as written, and the AppStream `<releases>` list is generated from these
headings.

## 2.2.6 — 2026-10-28

**Fixed: a DKMS module loaded from the initramfs still refused with Secure Boot on.** When Set
Up Signing rebuilt DKMS modules signed with the DKMS key, the initramfs kept the old, unsigned
copy, so a module that loads from there early in boot (a GPU helper module, for example) was
refused by kernel lockdown. Setup now generates the initramfs again for every kernel whose
modules it rebuilt, and copies it to the ESP.

## 2.2.5 — 2026-10-28

**Fixed: installing the build tools (or a kernel) failing after an interrupted update.** When
the machine was switched off or crashed while packages were being updated, apt refused to
install anything until the interrupted run was finished ("dpkg was interrupted"). Kernel Pop now
finishes it first: it completes the half-done package setup, repairs dependencies and reinstalls
any package the interruption left broken, says so in the log, and then goes on.

## 2.2.4 — 2026-10-28

**Fixed: a systemd-boot signed over by an earlier version stayed that way.** Before 2.2.3, Set
Up Signing replaced the systemd-boot the distribution ships already signed with a copy signed
only with Kernel Pop's key, so with Secure Boot on the machine booted only once that key was
enrolled. Set Up Signing now reinstalls the package that ships it, which puts the distribution's
signed copy back. If you set up signing on systemd-boot with an earlier version, run Set Up
Signing once more. If the package cannot be reinstalled (no network, say), setup says so and
goes on as before.

## 2.2.3 — 2026-10-28

**Fixed: DKMS modules still not signed after Set Up Signing.** The rebuild put DKMS's stored
build back instead of building again, and DKMS signs a module only when it builds it. Setup now
builds each module again before installing it, checks the result, and says which module is
still unsigned if a rebuild does not take.

**Fixed: a full ESP could leave a machine without a working boot entry.** When a copy of shim,
MOK Manager or systemd-boot to the ESP failed, Set Up Signing still created the firmware entry
and removed the old one. Every copy is now written under a temporary name and renamed, and setup
stops before changing any firmware entry when one fails.

**Fixed: Set Up Signing changed the boot path before the keys were queued.** If queuing the
keys for enrollment failed, systemd-boot was already signed with a key that would never be
enrolled. Setup now queues the keys first and only then changes the ESP and the firmware entries.

**Fixed: a systemd-boot signed by the distribution was signed over.** When the distribution
ships systemd-boot already signed, shim trusts it without any enrollment; setup now leaves it
as it is instead of replacing it with a copy that needs Kernel Pop's key.

**Fixed: a kernel whose ESP copy is unsigned is no longer kept with Secure Boot on.** If the
kernel is signed in /boot but the copy systemd-boot or kernelstub starts from the ESP is not,
the install is undone, as when signing fails, because that kernel could not boot.

## 2.2.2 — 2026-10-28

**Fixed: DKMS modules left unsigned by Set Up Signing.** Installing shim creates the DKMS
signing key itself, so setup found a key already there and rebuilt nothing, leaving the NVIDIA
driver and other DKMS modules unsigned or signed with an older key of the same name. Setup now
checks every DKMS module's signature against the DKMS key and rebuilds each package that is not
signed with it.

## 2.2.1 — 2026-10-28

**Fixed: the firmware entry for shim that Set Up Signing creates had a relative path**
(`EFI\ubuntu\shimx64.efi`, without the leading backslash), which not every firmware boots,
and setup then reported it could not create the entry. It now uses the full path
`\EFI\ubuntu\shimx64.efi`, and an entry made with either form is recognized.

## 2.2.0 — 2026-10-28

**Fixed: machines that boot systemd-boot directly not booting with Secure Boot on.** The
firmware starts only the Microsoft-signed shim with Secure Boot on, and shim is also what shows
MOK Manager, so on such a machine Set Up Signing queued a key that could never be enrolled and
the machine stopped booting once Secure Boot was turned on. Set Up Signing on systemd-boot now
puts shim in front of it, the same way Ubuntu's systemd-boot package does when GRUB is absent:

- It installs shim if needed (with a small placeholder package where GRUB is not installed,
  so GRUB stays off), signs systemd-boot with Kernel Pop's key, copies shim and MOK Manager to
  the ESP and puts shim at the fallback path.
- It adds a firmware entry that starts shim, which starts systemd-boot, and removes the
  "Linux Boot Manager" entries that skip shim.
- systemd-boot is signed again after every update that replaces it.

**New: the System tab shows the boot path**, whether shim starts the boot loader or the
firmware starts it without shim, and offers Set Up Signing when systemd-boot is started
directly.

**New: kernels installed later are signed automatically.** After Set Up Signing, a kernel
installed any other way without a signature (a mainline kernel installed with apt or dpkg, for
example) is signed with Kernel Pop's key as it is installed. Kernels signed by the distribution
are left alone.

**New: a DKMS key for out-of-tree modules.** On a machine with DKMS modules (such as the
NVIDIA driver) and no DKMS signing key, Set Up Signing creates one, rebuilds the modules signed
with it and queues it for enrollment with Kernel Pop's key, so they load with Secure Boot on.

## 2.1.4 — 2026-10-28

**Fixed: installing the build tools failing when a software update was running.** When
GNOME Software, unattended upgrades or another updater is using the package manager,
installing the build tools, installing or removing a kernel and Set Up Signing now wait for
it to finish (up to 30 minutes) instead of failing at once. The log names the program they
are waiting for and says when they go on.

## 2.1.3 — 2026-10-27

**Fixed: keys enrolled at boot still showing as waiting**, and this app and other apps that
queue keys in the same enrollment request disagreeing about what is waiting.

- The System tab now reads one list of waiting keys shared with those apps, and only while
  the waiting request is unchanged since it was written. Once the keys are enrolled at the
  blue MOK Manager screen, the list is ignored, and an enrolled key always shows as enrolled
  (for the DKMS key too). A key queued with an earlier version shows as not enrolled until
  you run Set Up Signing again; that keeps everything already waiting in the request.
- Set Up Signing no longer drops a waiting key it can't read, or a request whose contents
  don't add up: it leaves the whole request alone and asks you to finish it at the next
  reboot first. If Kernel Pop's own key or the DKMS key can't be read, setup stops with an
  error instead of leaving it out.
- If writing the new request fails, setup tries once more. If that fails too, the log names
  every key that is no longer waiting, with its fingerprint, and tells you to run Set Up
  Signing again; the System tab then no longer shows them as waiting.
- When the EFI variables are not mounted (some chroot tools unmount them on exit), Set Up
  Signing and installs mount them before they look at the keys, instead of seeing nothing
  waiting, and the System tab says they are not mounted instead of an unknown state.
- **Fixed: Check Again on the Build tab always failing.** It now checks the tarball you
  chose again, as it should.

## 2.1.2 — 2026-10-25

- The Build tab's configurations are now Match this system, Tuned for this machine and Use
  my own .config. Trim to this machine is gone: Tuned covers the same hardware plus
  everything else in the computer and the devices people plug in, for about a minute and a
  half more build time.

## 2.1.1 — 2026-10-25

**Fixed: Secure Boot keys not enrolling.** When another app had also queued a key for
enrollment (for example the key the NVIDIA driver's modules are signed with), the two
requests ended up needing different passwords, the blue MOK Manager screen accepted
neither key, and with Secure Boot on the kernel refused to start ("you need to load the
kernel first").

- Set Up Signing now puts every key that needs enrolling into one request with one
  password: Kernel Pop's key, the key DKMS signs the NVIDIA driver and other out-of-tree
  modules with, and any key another app already queued. At the next reboot, type the
  password you chose last; it enrolls all of them.
- Setting up again replaces the password instead of quietly keeping the old one.
- The System tab now shows a key that is waiting for enrollment as waiting (it showed "not
  enrolled" before, which invited setting up again with a new password), and shows whether
  the DKMS key is enrolled.
- With Secure Boot on, installing warns when the DKMS key is not enrolled, since the
  NVIDIA driver would not load.

Turn Secure Boot on only after the System tab shows the keys as enrolled.

## 2.1.0 — 2026-10-24

**New configuration: Tuned for this machine.** It builds only the drivers this computer
needs: everything loaded now, the driver for every device it has (in use or not), and the
kinds of hardware people plug in later or use now and then: USB drives and card readers,
the common file systems, keyboards, mice and game controllers, USB headsets and webcams,
Bluetooth, USB network adapters and phone tethering, docks and Thunderbolt, printers, and
the networking VPNs, containers and virtual machines need. On a typical desktop that is a
few hundred drivers instead of almost seven thousand, so a kernel builds in about a third
of the time (8 minutes instead of 26 on the test machine). Match this system stays the
default and still includes everything.

## 2.0.0 — 2026-10-24

**Kernel Pop now builds kernels from kernel.org source.** Ubuntu's mainline kernel archive
has stopped getting new builds, so Kernel Pop no longer uses it. Instead it takes the
source straight from kernel.org, builds it for your machine and installs it.

- The Browse tab lists kernel.org's current mainline, stable and longterm releases.
  Download Source fetches the tarball and its signature into `~/Downloads/kernel-pop/`.
  You can also open a tarball you already have, or drop a `linux-*.tar.xz` or `.tar.gz`
  onto the window. The version is read from the source itself, never from the file name.
- Every tarball's signature is checked against the keys of the kernel.org developers who
  sign releases, fetched fresh from kernel.org each time. If a dropped tarball has no
  `.tar.sign` next to it, kernel.org's is downloaded. A missing or bad signature blocks
  the build. For a tree you patched yourself, turn on "Build unverified source" for that
  tarball; the build log and the Install tab then say the source was unverified.
- The new Build tab has three configurations: **Match this system** (the default, built
  from the configuration of the kernel you are running), **Trim to this machine** (only
  drivers for what is loaded right now, much faster to build; hardware that is not
  plugged in may have no driver) and **Use my own .config**. The settings an Ubuntu
  configuration needs for a kernel.org tree are fixed automatically, and you see a
  summary before the build starts. New kernels are named like `7.2.12-kernelpop`.
- Builds run as your own user, never as root. Missing build tools are listed and
  installed with one authentication. ccache is used when it is installed, so the next
  point release builds much faster.
- While building you see the stage and the elapsed time, and Cancel stops the whole
  build and removes what it wrote. The finished packages, the final configuration and
  the build log go to `~/Downloads/kernel-pop/linux-<version>-kernelpop/`, and the build
  folder is deleted.
- The Install tab lists your finished builds. Only the kernel and its headers are
  installed, never the C library headers package a kernel build also makes, and the
  install checks every package again against the list written when it was built.
- Kernels you installed earlier, from Ubuntu's mainline archive or anywhere else, still
  show up on the System tab and can be removed, signed and booted as before.

You can delete `/usr/lib/kernel-pop/kernel-ppa.gpg`; Kernel Pop no longer uses it.

## 1.6.0 — 2026-10-11

**Downloads are now really verified.** Earlier versions checked the packages against the
wrong checksum file, so in practice no package was ever verified. Now:

- Every package must match the checksum file published next to it, and that file must
  carry a valid signature from Ubuntu's kernel team. A version without signed checksums
  is refused instead of being installed unverified.
- The install step checks everything again, with administrator rights, on a private copy
  of the packages, so nothing in your Downloads folder can be swapped in after the check.

**Secure Boot**

- With Secure Boot on, Kernel Pop no longer installs a kernel that could not boot. If
  signing is not set up, or the signing key is neither enrolled nor waiting for
  enrollment, the install is refused before anything changes and points you to Set Up
  Signing. If signing the new kernel fails, it is removed again, so nothing unbootable is
  left in the boot menu.
- If the signing key is still waiting for enrollment, the kernel is installed and signed,
  and the install reminds you to confirm the enrollment at the blue MOK Manager screen on
  the next reboot.
- A key waiting for enrollment is no longer sometimes shown as enrolled.
- Ubuntu's own kernels show "Signed by Ubuntu" instead of a misleading "Unsigned".
- On Pop!_OS, signing a kernel checks the boot loader copy of that kernel, not whichever
  kernel happens to be the default. Kernels beyond the current and previous one show
  "not in the boot menu" as information (that is how Pop!_OS works) instead of being
  marked broken, and the warning banner names the actual problem.
- On systemd-boot, re-signing a kernel no longer drops its initramfs from the boot entry.

**Boot menu**

- With the GRUB submenu turned off, new kernels are made the default again and Boot Next
  Time works. On systemd-boot, Boot Next Time also works with boot counting.
- Kernel versions are matched exactly, so removing or selecting one kernel can no longer
  pick up another whose version merely contains it.

**Other fixes**

- Only one download or administrator action runs at a time; the buttons for the others
  wait until it has finished.
- A cancelled download says "Cancelled" instead of "Download failed".
- Cancelling the password prompt, being refused, and a real error are now told apart, and
  the error details are shown.
- DKMS and signing problems are no longer counted twice.
- If updating Kernel Pop's system helper is cancelled when the app starts, the app says so
  and keeps installing, removing and signing kernels off until you restart it and
  authenticate, instead of running the old helper.

## 1.5.0 — 2026-10-08

**Kernel signing for Secure Boot.** Kernel Pop can sign the mainline kernels it installs,
so they can boot with Secure Boot on.

- **Set Up Signing** on the System tab does the one-time setup. It creates Kernel Pop's own
  signing key, signs every installed kernel and queues the key for enrollment. You choose
  a one-time password and type it once at the blue MOK Manager screen on the next reboot.
- After that, every kernel you install is signed. If signing fails, the install still
  finishes and the log says so with a `SIGN WARNING` line.
- Each kernel on the System tab shows Signed or Unsigned, with a Sign button for unsigned
  ones. The Kernel signing row shows whether the key is enrolled or waiting for the reboot.
- If you never set up signing, installs work as before.

## 1.4.1 — 2026-10-02

- The About dialog and README credit Codex (OpenAI) again, alongside Claude Code (Anthropic).

## 1.4.0 — 2026-10-02

- Secure Boot preflight: the System tab shows the Secure Boot state (EFI
  variable, cross-checked with `mokutil --sb-state` when installed), and
  installing while it is on asks for explicit confirmation, since unsigned
  mainline kernels are refused at boot.
- DKMS: the install builds registered DKMS modules (NVIDIA and others) for
  the new kernel before generating the initramfs and reports installed or
  failed per module in the Log tab, with the tail of `make.log` on failure.
  A DKMS failure is a loud warning, never an install error.
- `/boot` space check: the System tab and the install confirmation compare
  free space with the largest kernel image plus twice the largest initramfs
  already in `/boot` (a margin, since update-initramfs can briefly need more
  than the final size), instead of a fixed number.
- Remove Old Kernels: keep the newest N kernels and remove the rest under a
  single authentication. The running kernel and the newest installed kernel
  are never removed, and kernels whose removal would drag out the kernel
  metapackages are skipped.
- Boot Next Time: boot a chosen installed kernel on the next restart only
  (`grub-reboot` or `bootctl set-oneshot`), leaving the default untouched.
- On `GRUB_DEFAULT=saved` systems the previous default is recorded and put
  back if the new default does not verify.
- Initramfs generation works with `update-initramfs` or `dracut`, whichever
  is installed; the image is verified on disk either way.
- The privileged script's output now appears in the app's Log tab.

## 1.3.5 — 2026-09-24

- Releases no longer publish legacy `kernelpop-*` AppImage/.zsync copies, and
  the old `KERNELPOP_LOG` log override is gone; use `KERNEL_POP_LOG`.

## 1.3.4 — 2026-09-21

- Naming convention: display name "Kernel Pop"; crate, binary, repo
  (`labj1987/kernel-pop`), AppImage, desktop/icon files, helper dir
  (`/usr/lib/kernel-pop/`) and log (`/var/log/kernel-pop.log`) are now
  hyphenated lowercase. App ID and polkit action ids are unchanged.
- Releases keep publishing legacy `kernelpop-*` AppImage/.zsync copies so
  older installs can still self-update. The log override env var is now
  `KERNEL_POP_LOG` (`KERNELPOP_LOG` still honored).

## 1.3.3 — 2026-09-21

- **Security:** the removal path now rejects any kernel version string that
  isn't a plain token (`*` or `../..` could previously purge every
  `linux-*` package or walk `rm -rf` outside `/lib/modules`) and requires
  the kernel to actually be installed. The install path only accepts
  `linux-image`/`linux-modules`/`linux-headers` packages and refuses
  world-writable staging directories. The polkit policy no longer caches
  authorization (`auth_admin` instead of `auth_admin_keep`), and the
  AppImage's first-run helper install is hash-verified on the root side.
- Fixes a crash (RefCell double borrow) when typing in the search box or
  flipping the release-candidate switch with a kernel selected.
- Pop!_OS: kernelstub is detected and verified via the ESP kernel image
  instead of a per-version entry; boot-menu health no longer flags every
  kernel on Pop!_OS. GRUB boot-entry health now actually checks `grub.cfg`.
- Installer script runs under `set -euo pipefail`, checks that
  `/boot/vmlinuz-<version>` exists before reporting success, and refuses
  to purge a kernel if that would also remove the `linux-generic`
  metapackages.
- Downloads that end short of the advertised length are retried/failed.
- Build: `apt-get update` before installing zsync, appimagetool cache kept.
- CI: build/test/clippy/shellcheck on push and PR; releases verify the tag
  matches `Cargo.toml` and run the tests first. Added unit tests.
- The display name is now "Kernel Pop"; credits use the full author name.

## 1.3.2 — 2026-09-17

- Fixes the install script updating GRUB but not the actual boot default
  on `GRUB_DEFAULT=saved` systems (curtin/autoinstall images set this up
  by default). `update-grub` regenerates `grub.cfg` with a correct entry
  for the new kernel, but the saved default stays pinned to whatever was
  last selected — so the new kernel installs and verifies cleanly, and the
  machine still reboots into the old one, silently, especially with
  `GRUB_TIMEOUT_STYLE=hidden` where there's no visible menu to notice the
  stale selection from. Confirmed against a real install. The script now
  detects `GRUB_DEFAULT=saved`, reads the new kernel's actual menu entry
  ID out of the generated `grub.cfg` (never guessed or constructed — grub's
  entry IDs are random per-machine UUIDs unrelated to `/etc/machine-id`),
  points `grub-set-default` at it, and verifies the saved default actually
  changed before reporting success.

## 1.3.1 — 2026-09-09

- Credits Claude Code (Anthropic) in the About dialog's acknowledgements.

## 1.3.0 — 2026-09-06

- Adds systemd-boot support alongside GRUB. The privileged install/remove
  script now detects which boot loader is actually active (an ESP
  `loader.conf`, not just which binaries happen to be installed — GRUB
  packages can be left over on a systemd-boot machine) and handles each
  correctly:
  - Install verifies the boot menu entry systemd-boot's `kernel-install`
    hook creates automatically, failing loudly (same as the existing
    initramfs check) if it doesn't appear — instead of blindly calling
    `update-grub`, which does nothing on systemd-boot and would previously
    either report false success or `die` on a machine with no GRUB
    binary at all.
  - Remove now cleans up the systemd-boot boot menu entry via
    `kernel-install remove`. Previously, removing a kernel on a
    systemd-boot system left its menu entry permanently orphaned, since
    dpkg has no postrm hook for it — confirmed and fixed against a real
    installed kernel on a systemd-boot machine.
- The System tab's per-kernel health check gains a `boot menu` status
  alongside `initrd`/`modules`, so a kernel missing its boot loader entry
  shows as unhealthy before a reboot, not after.

## 1.2.1 — 2026-08-12

- Fixes the 1.2.0 release build, which failed in CI: its gtk4/libadwaita
  feature flags (`v4_22`/`v1_9`) required a newer system GTK4/libadwaita
  than the GitHub Actions runner's `libgtk-4-dev` provides (4.14.5).
  Dialed the feature flags back to `v4_12`/`v1_4,v1_5` — nothing in this
  release actually needs the newer APIs.

## 1.2.0 — 2026-08-12

- The System tab now shows a banner when a newer mainline kernel is
  available than the one currently running, checked in the background
  against the same version list the Browse tab uses. No banner when
  already current; a failed check fails silently.
- Browse tab gets an off-by-default toggle to show release-candidate
  (`-rcN`) kernel versions, badged distinctly from stable releases. The
  existing download/verify/install path — including the safety checks
  that never touch the running kernel — works unmodified for RCs.
- Dependency bump: gtk4 0.11, libadwaita 0.9, glib/gio 0.22, reqwest 0.13,
  scraper 0.27.
- AppStream metadata: added `developer`, `url`, and `content_rating` tags.

## 1.1.1 — 2026-08-05

- Fixes the AppImage's self-update pointer, which still referenced the
  pre-rename `MKI` repo — it now points at `KernelPop`, matching where
  the 1.1.0 release actually lives.

## 1.1.0 — 2026-08-05

**Rebrand to KernelPop, new icon set.** MKI is now KernelPop. Full rename — crate/package name, application ID
(`io.github.labj1987.KernelPop`), prgname, window title, About dialog,
desktop file, appdata, polkit policy, install-script/log paths
(`/usr/lib/kernelpop/`, `/var/log/kernelpop.log`), and the HTTP user
agent. Pure rebrand, no behavior change.

- Replaces the single 256px icon with the approved three-popcorn-kernel
  design, rendered natively at 16/32/48/64/128/256/512px (not just
  downscaled from one size) — bringing the icon set up to the usual
  multi-size convention. `build-appimage.sh`'s icon-copy lines now source from
  `data/$APP-256.png` instead of `data/$APP.png` accordingly.
- Adds a `.gitignore` — this repo never had one, which meant `target/`
  came within one `git add -A` of being committed wholesale while
  staging this change. `/target/`, `/build-appimage/`, and
  `*.AppImage(.zsync)` are now excluded.

The GitHub repo itself (`labj1987/MKI`) is intentionally left unrenamed
for now — `build-appimage.sh`'s `UPDATE_INFORMATION` still points at MKI.

## 1.0.12 — 2026-08-04

**Fix phantom taskbar entry: app_id/StartupWMClass mismatch.**

- Root cause confirmed with `WAYLAND_DEBUG=1 <appimage> 2>&1 | grep
  set_app_id`: on Wayland, GTK4 announces the GApplication ID
  (`io.github.labj1987.MKI`) as the toplevel's `app_id`, not `prgname`.
  The `.desktop` file's `StartupWMClass` was set to the old prgname
  (`mainline-kernel-installer`), so GNOME Shell couldn't match the
  running window to the desktop launcher — one process, two dock
  entries: the correctly-branded launcher entry and an unmatched generic
  one. This does not show under XWayland/X11, where WM_CLASS comes from
  prgname, which did match. Fixed by setting both `prgname` (in
  `main.rs`) and `StartupWMClass` (in the `.desktop` file) to the
  application ID, so the match works on either backend. The
  MessageDialog/AboutDialog/Dialog conversions and `StartupNotify=true`
  from 1.0.7–1.0.11 were reasonable but orthogonal — this is the actual
  fix.

## 1.0.11 — 2026-08-04

- The 1.0.7/1.0.9/1.0.10 fixes addressed real GTK-window-subclass dialogs
  but the phantom dock icon persisted immediately at launch, before any
  dialog could fire. Root cause: `mainline-kernel-installer.desktop` was
  missing `StartupNotify=true`. Without it, GNOME Shell has no way to
  associate the launch sequence with the eventual mapped window, so it
  leaves an orphaned placeholder entry in the dock (generic icon, tooltip
  showing only the raw `io.github.labj1987.MKI` app ID) alongside the
  real, correctly-iconed window. Added the line.

## 1.0.10 — 2026-08-04

**Fix phantom taskbar window from the About dialog.**

- The About dialog used `gtk4::AboutDialog`, which — like the
  `MessageDialog` fixed in 1.0.7 — is a `Gtk.Window` subclass and creates
  a real separate top-level Wayland surface, showing as a second, unnamed
  window in the dock. Switched to `libadwaita::AboutDialog` (`Adw.Dialog`
  subclass, requires the `v1_5` feature, already enabled), which renders
  as a sheet inside the main window's own surface.

## 1.0.9 — 2026-08-04

**Fix UPDATE_INFORMATION not being embedded at all.**

- 1.0.8 corrected the `UPDATE_INFORMATION` string but the fix never took
  effect: `build-appimage.sh` passed it to `appimagetool` as an
  environment variable, and this appimagetool build (continuous, git
  8c8c91f) silently ignores that env var — it only reads update info via
  the `-u`/`--updateinformation` CLI flag. Confirmed by inspecting the
  1.0.8 release AppImage's `.upd_info` ELF section: empty. This also
  explains the zsyncmake "silent no-op" workaround from 1.0.6 — without
  `-u`, appimagetool never attempts its own zsync generation either.
  Switched to passing `-u "$UPDATE_INFORMATION"` as an argument.

## 1.0.8 — 2026-08-04

**Fix UPDATE_INFORMATION to reference .zsync sidecar.**

- Per the AppImage update spec, the GitHub Releases zsync transport string
  must end in the `.zsync` sidecar filename, not the AppImage filename.
  `UPDATE_INFORMATION` in `build-appimage.sh` ended in
  `-x86_64.AppImage` instead of `-x86_64.AppImage.zsync`, which broke
  update detection in tools like Gear Lever even though the `.zsync`
  sidecar itself was already being generated and published correctly.
  Packaging-only fix, no application behavior changes.

## 1.0.7 — 2026-08-04

**Fix phantom taskbar window on kernel removal.**

- The "Remove kernel X?" confirmation used `libadwaita::MessageDialog`,
  which is deprecated (libadwaita 1.2+, replaced by `AlertDialog`) and is
  itself a `Gtk.Window` subclass — a real separate top-level Wayland
  surface. On GNOME/Wayland that surface doesn't inherit the app's
  icon/app_id, so the compositor showed it in the dock as a second,
  unnamed, generic-icon window alongside MKI's own. Switched to
  `libadwaita::AlertDialog`, which is an `Adw.Dialog` and renders as a
  floating sheet inside the parent window's own surface, so no second
  top-level window is created. Requires the `v1_5` libadwaita feature.

## 1.0.6 — 2026-07-17

**Generate .zsync directly with zsyncmake.**

- The 1.0.5 diagnostics showed zsync is installed and zsyncmake is on
  PATH and works — but appimagetool's own built-in zsync generation was
  still silently producing no .zsync file, with no warning either way.
  This appimagetool build (continuous, git 8c8c91f) most likely probes
  zsyncmake with a long-option flag (e.g. --version) to check it's
  usable before generating the sidecar; the installed zsyncmake build
  only understands old-style short options, so that probe fails and
  appimagetool quietly skips zsync generation instead of erroring.
  Rather than depend on appimagetool's internal detection, the .zsync
  is now built directly with a standalone `zsyncmake "$OUT"` call right
  after the AppImage is packed. This step is non-fatal: if it fails for
  any reason, the build still succeeds since the AppImage itself is
  already valid without it.

## 1.0.5 — 2026-07-17

**Diagnostic-only release.**

- 1.0.4 made zsync's install unconditional, but the release still isn't
  producing a .zsync file. Added explicit diagnostic output right before
  the appimagetool invocation — dpkg status for the zsync package,
  `which zsyncmake`, and `zsyncmake --version` — to pin down definitively
  whether zsync is installed, whether zsyncmake is on PATH, and whether
  it actually runs on the CI runner. No functional change otherwise.

## 1.0.4 — 2026-07-17

**Fix zsync still not installing in CI.**

- 1.0.3 added zsync to the build-dependencies apt-get install line, but
  that whole block is gated behind a check for whether cargo is already
  on PATH. In CI, a prior "Set up Rust Toolchain" step already installs
  cargo, so the guard evaluated false and the entire block — zsync
  included — was silently skipped, same as before 1.0.3. zsync is now
  installed unconditionally, ahead of the guarded block.

## 1.0.3 — 2026-07-17

**Fix missing .zsync file.**

- The build runner never had zsync installed, so appimagetool silently
  skipped generating the .zsync file even though UPDATE_INFORMATION was
  already set in 1.0.2 — update-aware tools had nothing to delta-update
  against. zsync is now installed alongside the other build dependencies.

## 1.0.2 — 2026-07-17

**Enable update checking.**

- Embedded UPDATE_INFORMATION in the AppImage so update-aware tools
  (Gear Lever, AppImageUpdate) can check GitHub Releases for newer
  versions and delta-update via zsync. CI now also uploads the .zsync
  file alongside the AppImage.

## 1.0.1 — 2026-07-17

**Fix AppImage version metadata.**

- build-appimage.sh computed VERSION from Cargo.toml but never exported
  it to appimagetool, which fell back to a git commit hash. Package
  managers like Gear Lever showed "Mainline Kernel Installer (05706c)"
  instead of a real version. VERSION is now passed into appimagetool's
  environment alongside ARCH.

## 1.0.0 — 2026-07-17

**Initial release.**

- Browse stable Ubuntu mainline kernel versions from kernel.ubuntu.com,
  with badges comparing each version against the running kernel and a
  filter field. Both the flat and amd64/ subdirectory page layouts are
  supported.
- Download the four generic amd64 packages (image-unsigned, modules,
  headers-generic, headers-all) with per-file progress, retries,
  cancellation, and SHA256 verification against the published CHECKSUMS
  file. A file that fails verification is deleted and the download
  aborts.
- Privileged install via polkit: dpkg install, then initramfs generation
  keyed on the kernel version read from .deb package metadata (never
  parsed from filenames), cross-checked against /lib/modules, then GRUB
  update. The install fails loudly if initrd.img does not appear in
  /boot, so a silently unbootable kernel cannot be produced.
- System tab lists every kernel in /boot with initrd and modules health
  checks; a window-wide warning banner appears whenever any installed
  kernel is missing its initramfs. Disk space on /boot and / is shown
  with low-space warnings.
- Old kernels can be removed from the System tab (packages purged, GRUB
  updated); the running kernel is never removable.
- AppImage-only distribution. First launch installs the privileged
  script and polkit policy to system paths via pkexec.
