# Kernel Pop (formerly MKI / Mainline Kernel Installer)

GTK4 + libadwaita GUI, written in Rust, for browsing and installing Ubuntu
mainline kernels from kernel.ubuntu.com. Distributed as a single AppImage.
The core safety feature: every kernel in `/boot` is checked for a matching
`initrd.img` and `/lib/modules` directory, so a kernel that would fail to
boot is flagged *before* the reboot, not after.

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
- `ui.rs` — the GTK4/libadwaita UI: browse/install/log/system tabs. One
  global busy state (`AppState::busy`, `begin_busy`/`end_busy`,
  `refresh_controls`) serialises downloads and every privileged action:
  while one runs, nothing that starts another is sensitive, including the
  per-kernel buttons (they live in `kernels_list`). Toasts for runs that
  can report problems come from `report_run`/`outcome_message`.
- `versions.rs` — talks to kernel.ubuntu.com/mainline: lists versions,
  resolves the generic-flavour amd64 `.deb` set, and fetches the
  `CHECKSUMS`/`CHECKSUMS.gpg` from the directory the debs are in (see
  Download verification).
- `verify.rs` — checks a CHECKSUMS signature with `gpgv` against the
  Ubuntu kernel PPA key built into the binary (`data/kernel-ppa.gpg`).
- `download.rs` — downloads the set with progress, cancel (the typed
  `Cancelled` error) and retries, verifies every SHA256, and writes the
  CHECKSUMS pair next to the debs with explicit 0755/0644 modes.
- `system.rs` — inventories installed kernels and their boot health (the
  initrd/modules/boot-entry check described above; for kernelstub, whether
  the kernel is the `vmlinuz` or `vmlinuz.old` one and its ESP copy
  exists); disk space; Secure Boot detection; kernel-signing state
  (`KernelSigning`, from the key files in `/var/lib/kernel-pop/mok/` plus
  `mokutil --test-key`, whose "already in the enrollment request" means
  pending, and `--list-new`); per-kernel `KernelSignature` (Kernel Pop's
  signature from the size/mtime markers in `/var/lib/kernel-pop/signed/`,
  since `/boot/vmlinuz-*` is 0600, or Ubuntu's from the dpkg source
  package); install preflight warnings; which old kernels can be pruned.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec` and
  returns its output; refuses to run when the installed helper differs from
  the bundled one (`helper_problem`); tells a cancelled authentication from
  a refused one and from a failed script; parses the output protocol into
  a `ScriptReport`. The signing setup's MOK password goes to the script on
  stdin, never in argv.

## Download verification

- Mainline publishes the debs' hashes in `<version>/amd64/CHECKSUMS` (the
  version page's own CHECKSUMS covers a build bundle, not the debs),
  signed in `CHECKSUMS.gpg` by "Kernel PPA <kernel-ppa@canonical.com>",
  primary key `60AA7B6F30434AE68E569963E50C6A0917C622B0`. Always fetch the
  pair from the debs' own directory. A deb missing from CHECKSUMS, or
  CHECKSUMS without a valid signature, is an error: the version is not
  installable.
- The privileged script trusts nothing in the download directory: it
  checks owner (`PKEXEC_UID`) and modes, copies the set into a root-only
  dir under `/var/tmp`, verifies the signature (with the keyring AppRun
  installs as `/usr/lib/kernel-pop/kernel-ppa.gpg`) and every hash on that
  copy, and installs only the copy.
- `src/testdata/` holds a real signed CHECKSUMS pair for the tests. To
  refresh `data/kernel-ppa.gpg`, export that fingerprint as a binary
  (not armored) keyring.

## Privileged script output

Every line starts with `[kernel-pop] `. The app reacts to markers at the
start of a line's text, so each is printed once per problem and never in
a summary: `DKMS WARNING:`, `SIGN WARNING:`, `BOOT BLOCKED:` (will not
boot while Secure Boot is enforcing), `BOOT PENDING:` (boots once the key
enrollment is confirmed). Summaries use `DKMS SUMMARY:`/`SIGN SUMMARY:`.
A blocked kernel is never made the GRUB saved default, and the running
kernel is selected for the next boot instead.

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
- The script's test overrides (`KERNEL_POP_*`, listed at its top) exist
  only in the script; pkexec clears the environment, so they never reach a
  real run. The script tests sign a copied EFI binary with a throwaway key,
  sign CHECKSUMS with a throwaway gpg key, and stub mokutil, dpkg and the
  boot loader tools; never run the real modes on a dev machine. CI sets
  `KERNEL_POP_REQUIRE_ALL_TESTS=1`, which turns a skipped test group into
  a failure.

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
3. Assembles the AppDir (binary, privileged script, kernel PPA keyring,
   polkit policy, desktop file, icon, appdata) and runs
   `desktop-file-validate` on the desktop file. AppRun installs the script,
   keyring and policy to system paths through pkexec when they differ,
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
workflows' actions are pinned to full commit SHAs, and Dependabot
(`.github/dependabot.yml`) proposes the updates.

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

- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/kernel-pop` (GitHub repo: `labj1987/kernel-pop`), owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file
  ownership correct.
