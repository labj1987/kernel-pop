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
- `ui.rs` — the GTK4/libadwaita UI: browse/install/log/system tabs.
- `versions.rs` — talks to kernel.ubuntu.com/mainline: lists versions,
  resolves the generic-flavour amd64 `.deb` set for a version, fetches
  checksums.
- `download.rs` — downloads the `.deb` set with progress, cancel, retries,
  and SHA256 verification against the published CHECKSUMS file.
- `system.rs` — inventories installed kernels and their boot health (the
  initrd/modules safety check described above); disk space on `/boot` and
  `/`; Secure Boot detection; kernel-signing state (`KernelSigning`, from
  the key files in `/var/lib/kernel-pop/mok/` plus `mokutil --test-key` and
  `--list-new`) and per-kernel signed state (from the size/mtime markers in
  `/var/lib/kernel-pop/signed/`, since `/boot/vmlinuz-*` is 0600); install
  preflight warnings; which old kernels can be pruned.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec`
  (dpkg install, kernel signing, DKMS build, initramfs generation, boot
  loader update, bulk removal, one-time boot, signing setup, signing one
  kernel) and returns the script's output to the UI. The signing setup's
  MOK password goes to the script on stdin, never in argv.

## Kernel signing

- Kernel Pop has its own kernel-signing key (`/var/lib/kernel-pop/mok/`,
  `signing.key` 0600, `signing.pem`/`signing.der` 0644, dir 0755). Never
  reuse `/var/lib/shim-signed/mok/MOK.priv`: its module-only EKU
  (1.3.6.1.4.1.2312.16.1.2) makes shim reject kernels signed with it. Key
  generation asserts that OID is absent.
- `sbsign` takes the PEM cert, `mokutil` the DER. sbsign appends a
  signature; an already-signed image keeps its existing ones.
- With no key, `--install` only adds one "signing not set up" log line.
- Test overrides: `KERNEL_POP_MOK_DIR`, `KERNEL_POP_SIGNED_DIR`,
  `KERNEL_POP_BOOT`. The script tests sign a copied EFI binary with a
  throwaway key and stub mokutil; never run the real modes on a dev machine.

## Build process

`build-appimage.sh` builds the AppImage. It runs as an ordinary user and
writes only inside the checkout:
1. Only on a machine with no toolchain (cargo or the gtk4 headers missing)
   does it install build deps via apt, which is the one part needing root:
   cargo, rustc, gtk4/adwaita dev headers, pkg-config, `zsync`, `wget`,
   `file` and `desktop-file-utils`. It then checks `wget`, `file` and
   `desktop-file-validate` exist and fails clearly if one is missing.
2. `cargo build --release --locked`, with the compiler pinned in
   `rust-toolchain.toml`.
3. Assembles the AppDir (binary, privileged script, polkit policy,
   desktop file, icon, appdata) and runs `desktop-file-validate` on the
   desktop file.
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
   matches `Cargo.toml`, runs the tests, runs `build-appimage.sh` and
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
