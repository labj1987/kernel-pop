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
   (`tar --strip-components=1`), configure (below), and show the summary.
4. **Build** (`build::build`, as the user, after the summary is accepted):
   `make -j$(nproc) bindeb-pkg KDEB_PKGVERSION=<version>-1` with
   `CC="ccache gcc"` and `CCACHE_BASEDIR` when ccache is installed, and
   `DEB_BUILD_PROFILES=pkg.linux-upstream.nokerneldbg`. Output goes to the
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

ccache is optional and used when installed. Missing tools are installed
with one `pkexec … --install-build-deps <names>`, which refuses any name not
in its allowlist.

## Configuration

Base config:

- **Match this system** (default): `/boot/config-$(uname -r)`.
- **Trim to this machine**: the same, then
  `make LSMOD=<lsmod output> localmodconfig` with the modules loaded right
  now. Hardware that is not plugged in or in use (USB devices, docks,
  external drives) may have no driver.
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

After configuring, the build refuses to start unless `LOCALVERSION` and the
two key settings came out as set. The summary also warns when an option the
kernel needs to trust MOK-signed modules is off (below), and when
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
| Trim to this machine (201 modules loaded) | 226 | 6 min 41 s | 3.2 GB | 28.5 MB | 10.3 MB |

The Match build took longer here than the 18 min 48 s in the container
because of the cold ccache's overhead and the running desktop. A warm ccache
(a rebuild, or the next point release) is much faster. ccache held 1.1 GB
after the Match build, well inside its default 5 GB limit.

## Disk space

The preflight refuses to start a build with less than the measured need
plus a margin free in `~/.cache` (`build::BUILD_NEED_BYTES`,
`TRIM_BUILD_NEED_BYTES`), or less than one more kernel needs in `/boot`
(the largest kernel image there plus twice the largest initramfs).
