# Kernel Pop

GTK4 + libadwaita desktop app for installing Ubuntu mainline kernels from
kernel.ubuntu.com, written in Rust. Distributed as an AppImage. (Formerly
known as MKI / Mainline Kernel Installer.)

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
- Remove old kernels (running kernel is never removable)
- Disk space checks for /boot and /
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

Developed and tested on Ubuntu 26.04, GNOME on Wayland. Mainline kernels
are unsigned: with Secure Boot enabled they will not boot without a
signing setup. Release candidates and daily builds are intentionally not
listed.

## Building from source

    apt install -y cargo rustc libgtk-4-dev libadwaita-1-dev pkg-config libssl-dev
    cargo build --release

Or build the AppImage the same way CI does:

    bash build-appimage.sh

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).

Code at or before commit `8cb172eb68a8ed7c417ae0f04c4b2adaae858c34` remains available
under the MIT License per its original release. From this commit forward,
AGPL-3.0-or-later.
