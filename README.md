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
- SHA256 verification against the published CHECKSUMS file
- Per-file download progress with retries and cancellation
- Remove old kernels (running kernel is never removable), singly or
  "keep the newest N" in one step
- Boot a chosen installed kernel on the next restart only
- Secure Boot warning before installing a kernel that will not be signed
  with an enrolled key
- Optional kernel signing: a one-time Set Up Signing step creates Kernel
  Pop's own signing key, signs the installed kernels and queues the key for
  MOK enrollment; from then on every kernel it installs is signed
  automatically, with a Signed/Unsigned badge per kernel
- DKMS modules (such as NVIDIA) are built for the new kernel and reported
  per module; a failure is a warning, not an install error
- Disk space checks for / and for /boot (sized from the kernels already there)
- Install log in-app plus /var/log/kernel-pop.log

## Install

Download the AppImage from the Releases page:

    chmod +x kernel-pop-*-x86_64.AppImage
    ./kernel-pop-*-x86_64.AppImage

First launch asks for authentication once to install the privileged
helper script and polkit policy to system paths.

## Using it

1. The System tab shows the running kernel and the health of every
   installed kernel.
2. Pick a version on the Browse tab and hit Download. Packages land in
   ~/Downloads/mainline-kernel-vX.Y.Z/ and are checksum-verified.
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

Mainline kernels are unsigned: with Secure Boot enforcing, shim refuses to
boot them. Kernel signing fixes that:

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
Ubuntu keeps its original one too. Kernel Pop uses its own key rather than
Ubuntu's module-signing MOK, which shim does not accept for kernels. Without
the setup, kernels are installed unsigned exactly as before.

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
