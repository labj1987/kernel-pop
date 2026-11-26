# Building kernels from kernel.org source

How Kernel Pop gets, checks, configures, builds and installs a kernel.org
kernel, and why each step is the way it is. The code is in `src/kernelorg.rs`,
`src/source.rs`, `src/verify.rs`, `src/build.rs` and
`scripts/privileged-install.sh`.

## The flow

1. **Source.** The Browse tab lists `https://www.kernel.org/releases.json`
   (mainline, stable, longterm; linux-next has no tarball and is left out).
   Download Source fetches the release's `source` (`.tar.xz`) and `pgp`
   (`.tar.sign`) URLs from cdn.kernel.org into `~/Downloads/kernel-pop/`.
   A tarball can also be opened or dropped onto the window (`.tar.xz` or
   `.tar.gz`). The compression is read from the file's magic bytes and the
   version from the top-level Makefile (`VERSION`, `PATCHLEVEL`, `SUBLEVEL`,
   `EXTRAVERSION`), never from the file name.
2. **Copy and verify.** The tarball is copied into
   `~/.cache/kernel-pop/source/` (0700, the file 0600; emptied on the next
   selection), and the signature is checked on that copy (below). The build
   extracts the same copy, so a tarball in a place someone else can write
   (a shared folder, removable media, `/tmp`) cannot be swapped between the
   check and the build. A bad or missing signature blocks the build unless
   "Build unverified source" is turned on for that one tarball.
3. **Prepare** (`build::prepare`, as the user): refuse to run as root, check
   the build tools, empty `~/.cache/kernel-pop/build/` (a failed build's tree
   is kept until here, so its space counts as free), check disk space, list
   the whole tarball and refuse it if any member is outside its one
   top-level directory, extract into `~/.cache/kernel-pop/build/linux-X.Y.Z/`
   (`tar --strip-components=1`), configure (below), run `make prepare` and a
   dry run (`make -n vmlinux modules`, a few seconds) that counts the objects
   the build compiles, and show the summary.
4. **Build** (`build::build`, as the user, after the summary is accepted):
   `make -j$(nproc) bindeb-pkg KDEB_PKGVERSION=<version>-1` with
   `CC="ccache gcc"` and `CCACHE_BASEDIR`, and
   `DEB_BUILD_PROFILES=pkg.linux-upstream.nokerneldbg`. The progress bar
   counts kbuild's `CC`/`AS` lines for objects against the dry run's count
   (extracting 0%, configuring 2%, compiling 5-95%, packaging 95%); without
   a count it pulses. Output goes to the
   Log tab and to `build.log` in the tree.
5. **Publish.** The `linux-image-<release>` and `linux-headers-<release>`
   packages (picked by their Package field) are moved into
   `~/Downloads/kernel-pop/.linux-<release>.new/` with the final config
   (`config-<release>`), `build-info.txt` (source, signature verdict, mode),
   a copy of `build.log` and a `SHA256SUMS` manifest. An earlier build of the
   same release is moved aside, the new directory renamed to
   `linux-<release>/`, and only then is the earlier one deleted (if the
   rename fails it is put back; on any error the temp directory goes).
   Everything else bindeb-pkg wrote (`linux-libc-dev`, `.buildinfo`,
   `.changes`) and the build tree are deleted.
6. **Install** through `pkexec` and `privileged-install.sh --install`, which
   is the only privileged step.

Every command runs in its own process group. Cancel sends SIGTERM to the
group (SIGKILL five seconds later), then removes the tree and every package
written so far, so nothing half-built can reach the install step. After a
failure the tree and its `build.log` stay until the next build starts.

## Verifying the source

kernel.org's procedure (https://www.kernel.org/signature.html, checked
2026-10-24): the `.tar.sign` is a detached signature over the
**uncompressed** tar, so one signature covers the `.tar.xz` and the
`.tar.gz`; the developers' keys are published over WKD
(`gpg2 --locate-keys torvalds@kernel.org gregkh@kernel.org`); verify with
`xz -cd linux-X.tar.xz | gpg2 --verify linux-X.tar.sign -`.

Kernel Pop does exactly that, without any bundled key:

1. A throwaway `GNUPGHOME` (0700, removed afterwards, its gpg-agent and
   dirmngr stopped).
2. `gpg --auto-key-locate clear,nodefault,wkd --locate-keys --
   torvalds@kernel.org gregkh@kernel.org` (given up after 90 seconds), so
   keys come from kernel.org's WKD only, never from a keyserver or the
   user's keyring. If the signature carries a "signer's user ID" subpacket
   naming another `@kernel.org` address, that key is fetched the same way.
   (Current releases carry only the issuer fingerprint; on 2026-10-24 every
   listed release, mainline 7.3 included, was signed by Greg Kroah-Hartman's
   key `647F 2865 4894 E3BD 4571 99BE 38DB BDC8 6092 693E`.)
3. The fetched keys are exported to a keyring of their own, and
   `xz -dc tarball | gpgv --keyring <that keyring> linux-X.tar.sign -` runs
   with an empty home. Only GOODSIG together with VALIDSIG counts; BADSIG,
   expired or revoked keys and a key not in the keyring are failures, and so
   is a tarball that does not decompress (unless gpgv stopped reading because
   the signature file itself is unusable; then gpgv's reason is shown).
4. The good signature must come from a key whose user ID is
   torvalds@kernel.org or gregkh@kernel.org, the release signers
   signature.html names. Any other kernel.org developer's signature (the
   signer picks the user ID subpacket, and hundreds of developers have WKD
   keys) is reported but not "verified": it needs the override.

A dropped tarball without a `.tar.sign` next to it gets kernel.org's,
fetched from `cdn.kernel.org/pub/linux/kernel/vN.x/linux-<name>.tar.sign` by
the Makefile version (`<name>` is `7.3` for 7.3.0, `7.2.12`, `7.4-rc1`).
Release candidates have no signature on cdn.kernel.org, so they need the
unverified override.

The override is per tarball (it resets whenever the source changes) and is
recorded in `build.log` ("Source signature: UNVERIFIED …" and a WARNING
line), in `build-info.txt`, on the Install tab, and in the install
confirmation.

## Build tools

Confirmed with a real `make bindeb-pkg` in a clean `ubuntu:26.04` container,
run as an unprivileged user with exactly this list (`build::BUILD_DEPS`,
which must equal the script's `BUILD_DEPS_ALLOWED`):

| Package | Needed for |
| --- | --- |
| build-essential, dpkg-dev | gcc, make, dpkg-buildpackage |
| bc, bison, flex | kconfig and the build itself |
| debhelper | bindeb-pkg's `debhelper-compat (= 12)` build dependency |
| kmod, libdw-dev, libelf-dev, libssl-dev, python3, rsync | bindeb-pkg's Build-Depends (dpkg-checkbuilddeps refuses to start without them) |
| xz-utils | reading and extracting `.tar.xz` |
| pahole | BTF (`CONFIG_DEBUG_INFO_BTF`). In Ubuntu 26.04 `dwarves` is only a transitional package for it |
| zstd | `CONFIG_MODULE_COMPRESS_ZSTD` |
| cpio | `CONFIG_IKHEADERS` |
| openssl | generating the module signing key |
| gawk | `modules.builtin.ranges` (`CONFIG_BUILTIN_MODULE_RANGES`); the first container build failed at the link step without it |
| ccache | every build compiles through it, so a rebuild or the next point release reuses most objects (required since 2.2.9) |
| gnupg | the kernel.org signature check (`gpg` and `dirmngr` fetch the developers' keys over WKD, `gpgconf` stops the throwaway agent) |

Missing tools, ccache and gnupg included, are installed
with one `pkexec … --install-build-deps <names>`, which refuses any name not
in its allowlist.

## Configuration

Base config:

- **Match this system** (default): `/boot/config-$(uname -r)`.
- **Tuned for this machine**: the same, then `make LSMOD=<list>
  localmodconfig` with three sources joined: the modules loaded right now
  (from `/proc/modules`; modules rarely unload, so this covers what was used
  since the last boot), the driver for every device the kernel sees in use
  or not (each `/sys/devices/**/modalias` resolved with `modprobe -R`), and
  a fixed list of drivers for hardware that is often plugged in later or
  used now and then (`build::TUNED_EXTRA_MODULES`: USB storage, card
  readers, common file systems, input devices and game controllers, USB
  audio and webcams, Bluetooth, USB network adapters and phone tethering,
  docks and Thunderbolt, printers and serial adapters, the AMD, Intel and
  nouveau graphics drivers and the HD-audio HDMI codecs, VPN, container
  and VM networking, VFIO, dm-crypt,
  loop, zram, ntsync). Hardware that is neither
  present nor one of those kinds (an unusual USB gadget, a different Wi-Fi
  card) may have no driver. Kernel messages in the journal were also
  tried as a history of earlier boots; on the test machine they named no
  module that was not loaded already, so they are not used.
- **Use my own .config**: the chosen file.

Fixes applied in every mode with `scripts/config`, then `make olddefconfig`:

- `LOCALVERSION="-kernelpop"`, so the release is `X.Y.Z-kernelpop` and never
  collides with Ubuntu's kernels.
- `SYSTEM_TRUSTED_KEYS=""` and `SYSTEM_REVOCATION_KEYS=""`: Ubuntu's config
  names `debian/canonical-certs.pem` and `debian/canonical-revoked-certs.pem`,
  which only exist in Ubuntu's kernel tree, and the build fails on them.
- Rust: when the base config has `CONFIG_RUST=y`, `make rustavailable`
  decides. If the installed rustc/bindgen do not satisfy it (Ubuntu 26.04
  ships rustc but not bindgen by default), `CONFIG_RUST` is turned off and
  the log and the summary say why. `olddefconfig` would drop it silently
  otherwise.
- Debug info: see the measurement below.
- Required options (`build::missing_required`; Match and Tuned only, Use my own
  .config gets a warning in the summary instead), turned on as modules (yes/no
  options switched on) when the base config has them off, and in Tuned again
  after localmodconfig, which drops every module that is not loaded. Options
  the tree's Kconfig does not define (an older longterm kernel, a renamed
  option) are skipped:
  - input: `USB_HID`, `HID_GENERIC`, `UHID`, `INPUT_UINPUT`, `INPUT_JOYDEV`,
    and `BT_HIDP` with classic Bluetooth. Without `UHID` (the 7.3.4 Tuned
    kernel) Bluetooth controllers, keyboards and mice connect but never become
    input devices.
  - networking, whenever `NETFILTER` is on: nftables and iptables with NAT,
    masquerade, mangle, raw and the REJECT targets, conntrack helpers, the
    common x_tables matches (with `hl` and the IPv6 `rt` match that ufw's
    rules use, and the 6.17+ `NETFILTER_XTABLES_LEGACY` switch the iptables
    tables depend on), ipset and IPVS, bridge netfilter and VLAN
    filtering, veth/macvlan/ipvlan/vxlan, overlayfs, vhost-net and vsock, TUN,
    WireGuard and IPsec (`build::REQUIRED_NETWORK_SYMBOLS` and
    `REQUIRED_NETWORK_FLAGS`; the list follows Docker's `check-config.sh`,
    libvirt's NAT network, ufw/firewalld and the common VPNs). These modules
    load only when a firewall rule, container or VPN uses them, so the 7.3.5
    Tuned kernel had lost them all: libvirt's default network could not start
    (no `REJECT`), and Docker ran only on what happened to be loaded.
  - graphics, whenever `DRM` and `PCI` are on: `DRM_AMDGPU` with
    `DRM_AMD_DC` (the only display path for Vega and newer) and
    `DRM_AMDGPU_SI`/`DRM_AMDGPU_CIK` (amdgpu drives GCN 1/2 cards by default
    when built with them; otherwise radeon takes them, without Vulkan),
    `DRM_RADEON` (pre-GCN AMD/ATI cards), `DRM_I915` and `DRM_XE` (Intel
    integrated and Arc), `DRM_NOUVEAU` (NVIDIA without the proprietary
    driver) (`build::REQUIRED_GPU_SYMBOLS` and `REQUIRED_GPU_FLAGS`; the
    firmware loader and DRM helpers come with them through Kconfig selects).
    localmodconfig keeps only the present card's driver, so a Tuned 7.3.5
    kernel saw an AMD card added next to an NVIDIA one but bound no driver
    to it. A graphics driver this config rules out (`DRM_I915` on
    `PREEMPT_RT` or a non-x86 kernel, `DRM_XE` without 4 KiB pages) is a
    note in the summary, not a refused build; any other one left off
    refuses the build like the rest.
  - HDMI/DisplayPort audio, whenever `SND`, `DRM` and `PCI` are on:
    `SND_HDA_INTEL` (the HD-audio PCI controller a graphics card's audio
    function uses) and the HDMI codecs `SND_HDA_CODEC_HDMI` with
    `SND_HDA_CODEC_HDMI_GENERIC`, `SND_HDA_CODEC_HDMI_ATI`,
    `SND_HDA_CODEC_HDMI_NVIDIA` and `SND_HDA_CODEC_HDMI_INTEL` (separate
    options since Linux 6.17; one `SND_HDA_CODEC_HDMI` before)
    (`build::REQUIRED_HDMI_AUDIO_SYMBOLS`). The 7.3.6 Tuned kernel on a
    machine with an NVIDIA card had no ATI codec, so an RX 580 added later
    fell back to the generic codec: no ELD, no sound on the TV over HDMI.
  - GPU passthrough, whenever `KVM` and `PCI` are on: `VFIO`, `VFIO_PCI` and
    `VFIO_IOMMU_TYPE1`, with `VFIO_GROUP` and `VFIO_CONTAINER` switched on
    (the type 1 backend has no prompt; the container selects it; a config that
    already uses `IOMMUFD_VFIO_CONTAINER` is left on that container)
    (`build::REQUIRED_PASSTHROUGH_SYMBOLS` and `REQUIRED_PASSTHROUGH_FLAGS`).
    Nothing loads them until a device is handed to a VM, so the 7.3.5 and
    7.3.6 Tuned kernels had none and could not pass a graphics card to a VM.

After configuring, the build refuses to start unless `LOCALVERSION`, the
two key settings and (Match and Tuned) every required option, except a
graphics driver the config rules out, came out as set. The summary also
warns when an option the kernel needs to trust MOK-signed modules is off (below), and when
`make kernelrelease` differs from `<version>-kernelpop` (a tree with its own
localversion file).

### Module signatures and Secure Boot

The in-tree modules are signed with a key the build generates
(`certs/signing_key.pem`); that is expected. DKMS modules (NVIDIA) are
signed with Ubuntu's MOK key (`/var/lib/shim-signed/mok/MOK.der`). An
upstream kernel trusts it without any Ubuntu patch:
`mod_verify_sig()` checks against the secondary trusted keyring, and the
`.machine` keyring, which holds the MOK keys when shim exports
`MokListTrustedRT` (shim's default), is linked into it. That needs
`SYSTEM_TRUSTED_KEYRING`, `SECONDARY_TRUSTED_KEYRING`,
`INTEGRITY_PLATFORM_KEYRING`, `LOAD_UEFI_KEYS`, `INTEGRITY_MACHINE_KEYRING`
and `MODULE_SIG` (Ubuntu's config has all of them and olddefconfig keeps
them), and `INTEGRITY_CA_MACHINE_KEYRING` off (with it on, only CA keys
reach `.machine`). Upstream has no lockdown-on-Secure-Boot switch, but with
`IMA_ARCH_POLICY=y` (Ubuntu's config) module signatures are enforced when
Secure Boot is on.

The kernel image itself is unsigned; with signing set up, the install
signs it with Kernel Pop's key as before.

## Debug info: measured

Measured on 2026-10-24 on LordNikon (Intel Core i5-12600K, 16 threads,
32 GB RAM, NVMe), building Linux 7.2.12 from the running kernel's Ubuntu
config (`7.2.6-070206-generic`, 6703 modules) with the fixes above, `make
-j16 bindeb-pkg`, cold (no ccache), in a clean `ubuntu:26.04` container with
only the build tools listed above:

| Debug settings | Build time | Build tree | Packages |
| --- | --- | --- | --- |
| Ubuntu's (DWARF 5 + BTF), as bindeb-pkg does it by default | 40 min 14 s (about 10 min of it packing the -dbg package) | 44.5 GB | image 132 MB, headers 11 MB, **-dbg 1.6 GB**, libc-dev 1.6 MB |
| Off (`DEBUG_INFO_NONE`, so no BTF) | 18 min 48 s | 6.7 GB | image 117 MB, headers 10.5 MB, libc-dev 1.6 MB |

Even with the -dbg package skipped (`pkg.linux-upstream.nokerneldbg`),
keeping Ubuntu's settings costs about 30 minutes and 44.5 GB per build;
the debug info itself is stripped from the installed modules anyway
(bindeb-pkg installs them with `INSTALL_MOD_STRIP=1`). What it buys is BTF
(`/sys/kernel/btf/vmlinux`), which only BPF CO-RE tools use. On the test
machine nothing did: the only BPF programs loaded were systemd's and
snapd's cgroup device and firewall programs (none with a BTF id), systemd's
BPF-LSM features were already off (`bpf` is not in the LSM list), and no
bpftrace, BCC or sched_ext scheduler was installed.

**Decision: debug info off in every mode** (`--disable DEBUG_INFO_DWARF5
--disable DEBUG_INFO_DWARF4 --disable DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT
--enable DEBUG_INFO_NONE`), which also turns off BTF; the build refuses to
start if `CONFIG_DEBUG_INFO` is still set. The `nokerneldbg` build profile
stays on as well, so no `linux-image-*-dbg` package is ever built, and the
privileged script refuses to install one regardless. If you need BTF for
BPF tooling, this is the setting to revisit.

## Build time and size per mode

Linux 7.2.12 built by Kernel Pop itself (`build::prepare` and `build::build`,
the code the Build tab runs) on the same machine, on the desktop with ccache
installed but empty (cold), debug info off:

| Mode | Modules | Build time | Build tree at its largest | Image deb | Headers deb |
| --- | --- | --- | --- | --- | --- |
| Match this system | 6703 | 26 min 5 s | 6.7 GB | 117 MB | 10.6 MB |
| Tuned for this machine (196 loaded, 23 more for present devices, 110 extras) | 369 | 8 min 2 s | 3.5 GB | 34 MB | 10.3 MB |

Since the graphics drivers are always kept (amdgpu, radeon, i915, xe and
nouveau, above), Tuned builds them all on every machine. A Tuned Linux 7.3.5
build measured the same way on 2026-11-22, on a faster 32-thread machine:
419 modules, 5 min 30 s, 3.9 GB build tree at its largest, image deb 41 MB,
headers deb 10.4 MB. The five graphics drivers were 2465 of its 7888
compiled files, so where none of them was loaded before (an NVIDIA card on
the proprietary driver), a Tuned build compiles about 45% more files than
it did. Match this system was not measured on 7.3.5: that machine runs a
Tuned kernel (431 modules), so Match there starts from a config of about
the same size and says nothing about an Ubuntu config's 6703 modules.

Version 2.1.0 also had "Trim to this machine" (only the modules loaded
right now): 226 modules, 6 min 41 s, 3.2 GB. It was removed in 2.1.2: Tuned
covers the same hardware plus everything present and the common plug-in
kinds for about a minute and a half more.

The Match build took longer here than the 18 min 48 s in the container
because of the cold ccache's overhead and the running desktop. A warm ccache
(a rebuild, or the next point release) is much faster. ccache held 1.1 GB
after the Match build, well inside its default 5 GB limit.

## Disk space

The preflight refuses to start a build with less than the measured need
plus a margin free in `~/.cache` (`build::BUILD_NEED_BYTES`,
`TUNED_BUILD_NEED_BYTES`), or less than one more kernel needs in `/boot`
(the largest kernel image there plus twice the largest initramfs).
