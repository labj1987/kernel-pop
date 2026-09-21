# KernelPop (formerly MKI / Mainline Kernel Installer)

GTK4 + libadwaita GUI, written in Rust, for browsing and installing Ubuntu
mainline kernels from kernel.ubuntu.com. Distributed as a single AppImage.
The core safety feature: every kernel in `/boot` is checked for a matching
`initrd.img` and `/lib/modules` directory, so a kernel that would fail to
boot is flagged *before* the reboot, not after.

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
  `/`.
- `install.rs` — invokes `scripts/privileged-install.sh` via `pkexec`
  (dpkg install, initramfs generation, GRUB update).

## Build process

`build-appimage.sh` builds the AppImage:
1. Installs build deps via apt (cargo, rustc, gtk4/adwaita dev headers,
   `zsync` — see gotcha below).
2. `cargo build --release`.
3. Assembles the AppDir (binary, privileged script, polkit policy,
   desktop file, icon, appdata).
4. Downloads `appimagetool` (continuous build) and packs the AppDir into
   `kernelpop-$VERSION-x86_64.AppImage`, with
   `UPDATE_INFORMATION` set for `gh-releases-zsync` delta updates.
5. Runs `zsyncmake` directly on the built AppImage to produce the
   `.zsync` sidecar.

**Gotcha (fixed in v1.0.6):** `appimagetool`'s own built-in zsync
generation silently no-ops on the GitHub Actions runner even when
`UPDATE_INFORMATION` is set and `zsync`/`zsyncmake` are installed and
working — it produces no `.zsync` file and prints no warning either way.
Best guess: this appimagetool build probes zsyncmake with a long-option
flag (e.g. `--version`) that the installed short-option-only zsyncmake
build rejects, and appimagetool treats that as "zsyncmake unavailable"
without logging it. Do not rely on appimagetool to generate the
`.zsync` — call `zsyncmake "$OUT"` directly right after packing, as the
script does now. Keep that call non-fatal (the AppImage is valid without
the sidecar).

Also note the apt-get install for `zsync` is deliberately unconditional
(not inside the `command -v cargo` guard) — CI's "Set up Rust Toolchain"
step means that guard evaluates false, so anything gated behind it gets
silently skipped in CI even though it runs fine locally on a clean
machine.

## Release process

1. Bump `version` in `Cargo.toml` (and let `Cargo.lock` follow).
2. Add a `CHANGELOG.md` entry.
3. Add a `<release version="X.Y.Z" date="..."/>` entry to
   `data/io.github.labj1987.KernelPop.appdata.xml`. The release workflow
   fails if it is missing, and `build-appimage.sh` injects one into the
   packaged copy as a safety net, but the source file should be right.
4. Commit, push to `main` (`.github/workflows/ci.yml` builds, tests and
   shellchecks on push/PR).
5. `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"), which checks the tag matches `Cargo.toml`, runs the tests,
   runs `build-appimage.sh` and uploads the AppImage (+ `.zsync`) to a
   GitHub Release via `softprops/action-gh-release`.

## Conventions

- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/KernelPop`, owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file
  ownership correct.
