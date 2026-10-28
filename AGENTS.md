# Kernel Pop (formerly MKI / Mainline Kernel Installer)

GTK4 + libadwaita GUI, written in Rust, that builds kernel.org kernels for
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

- `main.rs` — entry point, sets up the shared Tokio runtime and wires up
  the GTK application.
- `ui.rs` — the GTK4/libadwaita UI: system/browse/build/install/log tabs,
  plus a window-wide drop target for tarballs. One global busy state
  (`AppState::busy`, `begin_busy`/`end_busy`, `refresh_controls`)
  serialises downloads, signature checks, builds and every privileged
  action: while one runs, nothing that starts another is sensitive,
  including the per-kernel buttons (they live in `kernels_list`). Worker
  threads report through the `UiMsg` channel, drained in batches so a
  build's output cannot flood GTK; the Log tab keeps the last
  `LOG_MAX_LINES` lines (the whole output is in build.log). Toasts for runs
  that can report problems come from `report_run`/`outcome_message`.
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
  configure, `ConfigSummary`) and `build` (`make bindeb-pkg`, publish to
  `~/Downloads/kernel-pop/linux-<release>/` with `SHA256SUMS`, the final
  config, build-info.txt and build.log, then delete the tree). Each command
  runs in its own process group so `BuildControl::cancel` kills all of it.
  Refuses to run as root.
- `download.rs` — downloads files with progress, cancel (the typed
  `Cancelled` error) and retries, into `.part` files renamed when complete,
  with explicit 0755/0644 modes; `sha256_file`.
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
  returns its output; refuses to run when the installed helper differs from
  the bundled one (`helper_problem`); tells a cancelled authentication from
  a refused one and from a failed script; parses the output protocol into
  a `ScriptReport`. `--install-build-deps` takes only names from the
  script's `BUILD_DEPS_ALLOWED`. The signing setup's MOK password goes to the script on
  stdin, never in argv.

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
then fails, the just-installed packages are purged again
(`rollback_install`) and the run fails. A queued key is fine: the kernel
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
- `sync_signed_to_esp` returns non-zero when the ESP copy is not known to
  be signed; `do_install` then calls `esp_copy_unsigned`, which rolls the
  install back with Secure Boot enforcing (a warning with it off), like a
  failed signing.
- shim in front of systemd-boot (`setup_shim_chain`, from
  `do_setup_signing` when `detect_bootloader` says systemd-boot): the
  layout of Ubuntu's systemd-boot postinst `install_shim` (shim/fb/mm in
  `EFI/<os-release ID>/`, shim at `EFI/BOOT/BOOTX64.EFI`, UCS-2LE
  `BOOTX64.CSV` byte-identical to install_shim's, firmware entry
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
  `--reinstall` of packages whose status has the reinst-required flag.

## Build process

`build-appimage.sh` builds the AppImage. It runs as an ordinary user and
writes only inside the checkout:
1. Only on a machine with no toolchain (cargo or the gtk4 headers missing)
   does it install build deps via apt (through sudo when not root), the
   one part needing root:
   cargo, rustc, gtk4/adwaita dev headers, pkg-config, `zsync`, `wget`,
   `file` and `desktop-file-utils`. It then checks `wget`, `file` and
   `desktop-file-validate` exist and fails clearly if one is missing.
2. `cargo build --release --locked`, with the compiler pinned in
   `rust-toolchain.toml`.
3. Assembles the AppDir (binary, privileged script, polkit policy, desktop
   file, icon, appdata) and runs `desktop-file-validate` on the desktop
   file. AppRun installs the script and policy to system paths through
   pkexec when they differ,
   says so on stderr when that is cancelled or fails, and exports
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
in CI. The workflow's "Install build dependencies" step installs the GTK
headers and the packaging tools (`zsync`, `wget`, `file`,
`desktop-file-utils`) instead, and the script checks the tools exist and
fails clearly if one is missing.

## Release process

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
