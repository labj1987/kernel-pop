# Kernel Pop

GTK4 + libadwaita desktop app for installing Ubuntu mainline kernels from
kernel.ubuntu.com, written in Rust. Distributed as an AppImage.

## Why this exists

Mainline kernel tools have a habit of installing a kernel without
generating its initramfs, because they parse the kernel version out of a
filename and the mainline naming convention (7.1.3-070103-generic vs
7.1.3-generic) breaks the parse. The result is a kernel that VFS-panics
on boot. This app is built around not letting that happen:

- The kernel version is read from the .deb package metadata, never from
  filenames, and cross-checked against the /lib/modules directory that
  actually appears after install.
- The initramfs is generated and then VERIFIED to exist in /boot before
  the install reports success. A missing initrd fails the install loudly.
- The System tab health-checks every installed kernel for a missing
  initrd, modules directory, or boot menu entry, so a broken kernel is
  visible before a reboot instead of after.
- Works with both GRUB and systemd-boot, detecting whichever one is
  actually active on the machine.

## Features

- Browse stable mainline versions with newer/same/older badges relative
  to the running kernel
- A banner on the System tab when a newer mainline kernel is available
- Optional release-candidate visibility on the Browse tab (off by default)
- Every package is verified before it is installed: the CHECKSUMS file
  must carry a valid signature from Ubuntu's kernel team (the "Kernel PPA"
  key, built into the app), and each package must match its SHA256 there.
  A version without signed checksums is not offered for install. The
  install step repeats the whole check as root on a private copy, so the
  files in your Downloads folder are never what gets installed
- Per-file download progress with retries and cancellation
- Remove old kernels (running kernel is never removable), singly or
  "keep the newest N" in one step
- Boot a chosen installed kernel on the next restart only
- With Secure Boot on, a kernel that could not boot is never installed:
  the install is refused until signing is set up, and a kernel whose
  signing fails is removed again
- Optional kernel signing: a one-time Set Up Signing step creates Kernel
  Pop's own signing key, signs the installed kernels and queues the key for
  MOK enrollment; from then on every kernel it installs is signed
  automatically, with a Signed / Signed by Ubuntu / Unsigned badge per
  kernel
- DKMS modules (such as NVIDIA) are built for the new kernel and reported
  per module; a failure is a warning, not an install error
- Disk space checks for / and for /boot (sized from the kernels already there)
- Install log in-app plus /var/log/kernel-pop.log

## Install

Download the AppImage from the Releases page:

    chmod +x kernel-pop-*-x86_64.AppImage
    ./kernel-pop-*-x86_64.AppImage

First launch asks for authentication once to install the privileged
helper script, the Ubuntu kernel signing key it checks downloads with, and
the polkit policy to system paths. A new version asks once more to update
them. If that authentication is cancelled, the app still opens but shows a
banner, and installing, removing and signing kernels stay off until you
restart it and authenticate.

## Using it

1. The System tab shows the running kernel and the health of every
   installed kernel.
2. Pick a version on the Browse tab and hit Download. Packages land in
   ~/Downloads/mainline-kernel-vX.Y.Z/, next to the signed CHECKSUMS they
   were verified against.
3. Review the staged packages on the Install tab and hit Install Kernel.
4. Reboot when ready. The new kernel only reports success after its
   initramfs is verified on disk.

## Screenshots

**System** — every installed kernel with initrd/modules health checks, a running badge, disk space, and a banner when a newer mainline kernel is available (hidden here since the running kernel is already current)

![System tab](docs/screenshots/system-tab.png)

**Browse** — mainline versions badged against the running kernel, with an off-by-default toggle to show release candidates

![Browse tab](docs/screenshots/browse-tab.png)

**Install** — staged packages, checksum status, and the install action

![Install tab](docs/screenshots/install-tab.png)

**Log** — full run log, from version fetch through initramfs verification

![Log tab](docs/screenshots/log-tab.png)

## Notes

Developed and tested on Ubuntu 26.04, GNOME on Wayland. Daily builds are
intentionally not listed; release candidates are hidden unless "Show
release candidates" is turned on in the Browse tab.

### Secure Boot and kernel signing

Mainline kernels are unsigned. With Secure Boot enforcing, shim refuses to
boot an unsigned kernel, so a mainline kernel needs Kernel Pop's signing
setup (or Secure Boot turned off) before it can boot:

1. On the System tab, Kernel signing → **Set Up Signing**. Choose a
   one-time password (8-16 characters). Kernel Pop installs `sbsigntool`,
   `openssl` and `mokutil` if they are missing, creates a key in
   `/var/lib/kernel-pop/mok/`, signs every installed kernel and queues the
   key for enrollment with `mokutil --import`.
2. Reboot. A blue MOK Manager screen appears: choose Enroll MOK, Continue,
   Yes, and type the password once.

From then on every kernel Kernel Pop installs is signed (`sbsign`, then
`sbverify` before the image in `/boot` is replaced), whether Secure Boot is
on or off. A signing failure is a `SIGN WARNING` in the log, never an
install failure. Signing adds a signature, so a kernel already signed by
Ubuntu keeps its original one too; Ubuntu's own kernels show "Signed by
Ubuntu" and boot under Secure Boot without the setup. Kernel Pop uses its
own key rather than Ubuntu's module-signing MOK, which shim does not accept
for kernels. Without the setup, kernels are installed unsigned exactly as
before.

With Secure Boot enforcing, Kernel Pop only installs a kernel that will be
signed with a key that is enrolled or waiting for enrollment:

- Without the signing setup, or with a key that is neither enrolled nor
  queued (or whose state mokutil cannot report), the install is refused
  before anything changes, and the app points you to Set Up Signing.
- If signing the new kernel fails, its packages are removed again, so no
  kernel that shim would refuse is left in the boot menu.
- If the key is still waiting for enrollment, the kernel is installed and
  signed, and the install reminds you to confirm the enrollment at the MOK
  Manager screen on the next reboot; skip that and it will not boot.

With Secure Boot off, every kernel installs; one that could not be signed
is reported with a `SIGN WARNING`, and boots until Secure Boot is turned
on.

## Building from source

    apt install -y cargo rustc libgtk-4-dev libadwaita-1-dev pkg-config
    cargo build --release

Or build the AppImage. An ordinary user is enough once the build
dependencies are installed:

    apt install -y cargo rustc libgtk-4-dev libadwaita-1-dev pkg-config zsync wget file desktop-file-utils
    bash build-appimage.sh

## Acknowledgements

Development assistance: Claude Code (Anthropic) and Codex (OpenAI).

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).

Code at or before commit `8cb172eb68a8ed7c417ae0f04c4b2adaae858c34` remains available
under the MIT License per its original release. From this commit forward,
AGPL-3.0-or-later.

Linnard Alex Brown Jr.
