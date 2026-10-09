#!/usr/bin/env bash
# build-appimage.sh — build the Kernel Pop AppImage.
# Run from the repo root on Ubuntu (CI uses ubuntu-24.04) as an ordinary user: it
# writes only inside the checkout. Only its from-scratch dependency install below
# needs root.
set -euo pipefail

APP="kernel-pop"
# Single source of truth: the version in Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="x86_64"
BUILD_DIR="build-appimage"
APPDIR="$BUILD_DIR/AppDir"

echo "==> Building $APP $VERSION AppImage"

# ── Build dependencies ────────────────────────────────────────────────
# Only a machine with no toolchain gets packages installed here, and that is the
# one part of this script that needs root. In CI the workflow installs the GTK
# headers and the packaging tools before this script runs, so the guard is false
# there. A failed index refresh (an unrelated third-party repo) is tolerated;
# only a failed install is fatal.
if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists gtk4 2>/dev/null; then
    echo "==> Installing build dependencies"
    apt-get update -qq || true
    apt-get install -y -qq cargo rustc libgtk-4-dev libadwaita-1-dev \
        pkg-config zsync wget file desktop-file-utils
fi

for tool in wget file desktop-file-validate; do
    command -v "$tool" >/dev/null 2>&1 || { echo "==> ERROR: $tool is not installed" >&2; exit 1; }
done

# ── Release build ─────────────────────────────────────────────────────
echo "==> cargo build --release --locked"
cargo build --release --locked

# ── AppDir layout ─────────────────────────────────────────────────────
rm -rf "$BUILD_DIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/lib/$APP" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/polkit-1/actions" \
         "$APPDIR/usr/share/metainfo"

cp "target/release/$APP"                           "$APPDIR/usr/bin/"
cp scripts/privileged-install.sh                   "$APPDIR/usr/lib/$APP/"
chmod 755 "$APPDIR/usr/lib/$APP/privileged-install.sh"
cp data/$APP.desktop                                "$APPDIR/usr/share/applications/"
cp data/$APP-256.png                                "$APPDIR/usr/share/icons/hicolor/256x256/apps/$APP.png"
cp data/io.github.labj1987.KernelPop.policy         "$APPDIR/usr/share/polkit-1/actions/"
# The <releases> list is generated from CHANGELOG.md's version headings, and fails the
# build if the newest one is not this Cargo.toml version.
python3 scripts/sync_appdata_releases.py
cp data/io.github.labj1987.KernelPop.appdata.xml    "$APPDIR/usr/share/metainfo/"

# Top-level AppImage requirements
cp data/$APP.desktop "$APPDIR/"
cp data/$APP-256.png "$APPDIR/$APP.png"

desktop-file-validate "$APPDIR/$APP.desktop"

# ── AppRun ────────────────────────────────────────────────────────────
# On first launch the privileged script and polkit policy must exist at
# fixed system paths (polkit refuses relative/user paths), so AppRun
# installs them via pkexec when missing or outdated, then execs the app.
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
APP="kernel-pop"

SRC_SCRIPT="$HERE/usr/lib/$APP/privileged-install.sh"
SRC_POLICY="$HERE/usr/share/polkit-1/actions/io.github.labj1987.KernelPop.policy"
DST_SCRIPT="/usr/lib/$APP/privileged-install.sh"
DST_POLICY="/usr/share/polkit-1/actions/io.github.labj1987.KernelPop.policy"

needs_install=0
if [[ ! -f "$DST_SCRIPT" ]] || ! cmp -s "$SRC_SCRIPT" "$DST_SCRIPT"; then
    needs_install=1
fi
if [[ ! -f "$DST_POLICY" ]] || ! cmp -s "$SRC_POLICY" "$DST_POLICY"; then
    needs_install=1
fi

if [[ $needs_install -eq 1 ]]; then
    # The AppImage's FUSE mount is normally not readable by root, so the
    # files are staged in a private user dir. To close the stage-then-install
    # TOCTOU window, hashes are taken from the read-only mounted originals and
    # the root side copies the staged files into a root-owned dir and refuses
    # to install anything that doesn't match those hashes.
    STAGE="$(mktemp -d)"
    cp "$SRC_SCRIPT" "$STAGE/privileged-install.sh"
    cp "$SRC_POLICY" "$STAGE/policy"
    H_SCRIPT="$(sha256sum "$SRC_SCRIPT" | cut -d' ' -f1)"
    H_POLICY="$(sha256sum "$SRC_POLICY" | cut -d' ' -f1)"
    pkexec bash -c '
        set -euo pipefail
        stage="$1"; hs="$2"; hp="$3"; dst_s="$4"; dst_p="$5"
        safe="$(mktemp -d)"; trap "rm -rf \"$safe\"" EXIT
        cp "$stage/privileged-install.sh" "$safe/s"
        cp "$stage/policy" "$safe/p"
        [[ "$(sha256sum "$safe/s" | cut -d" " -f1)" == "$hs" ]] || { echo "script hash mismatch" >&2; exit 1; }
        [[ "$(sha256sum "$safe/p" | cut -d" " -f1)" == "$hp" ]] || { echo "policy hash mismatch" >&2; exit 1; }
        install -D -m 755 "$safe/s" "$dst_s"
        install -D -m 644 "$safe/p" "$dst_p"
    ' _ "$STAGE" "$H_SCRIPT" "$H_POLICY" "$DST_SCRIPT" "$DST_POLICY"
    rm -rf "$STAGE"
fi

export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/$APP" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# ── appimagetool ──────────────────────────────────────────────────────
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
TOOL_DIR=".cache"
TOOL="$TOOL_DIR/appimagetool-$APPIMAGETOOL_VERSION"
if [[ ! -f "$TOOL" ]]; then
    mkdir -p "$TOOL_DIR"
    wget -q --no-hsts -O "$TOOL.part" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
    mv "$TOOL.part" "$TOOL"
fi
if ! echo "$APPIMAGETOOL_SHA256  $TOOL" | sha256sum -c --status -; then
    echo "==> ERROR: appimagetool checksum mismatch" >&2
    rm -f "$TOOL"
    exit 1
fi
chmod +x "$TOOL"

# The runtime appimagetool puts in front of the squashfs. Without --runtime-file it downloads
# the moving `continuous` build at pack time, so it is pinned and checked the same way.
# To bump: pick a release at https://github.com/AppImage/type2-runtime/releases and take the
# sha256 of its runtime-x86_64 asset (download it and run sha256sum).
RUNTIME_VERSION="20251108"
RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"
RUNTIME="$TOOL_DIR/runtime-x86_64-$RUNTIME_VERSION"
if [[ ! -f "$RUNTIME" ]]; then
    mkdir -p "$TOOL_DIR"
    wget -q --no-hsts -O "$RUNTIME.part" \
        "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-x86_64"
    mv "$RUNTIME.part" "$RUNTIME"
fi
if ! echo "$RUNTIME_SHA256  $RUNTIME" | sha256sum -c --status -; then
    echo "==> ERROR: type2-runtime checksum mismatch" >&2
    rm -f "$RUNTIME"
    exit 1
fi

echo "==> Packing AppImage"
OUT="$APP-$VERSION-$ARCH.AppImage"

UPDATE_INFORMATION="gh-releases-zsync|labj1987|kernel-pop|latest|kernel-pop-*-x86_64.AppImage.zsync"
VERSION="$VERSION" ARCH="$ARCH" "$TOOL" --appimage-extract-and-run \
    --runtime-file "$RUNTIME" -u "$UPDATE_INFORMATION" "$APPDIR" "$OUT"

echo "==> Done: $OUT"
ls -lh "$OUT"

# appimagetool's built-in zsync generation silently no-ops on this runner,
# so build the .zsync sidecar directly. Fatal in CI (CI is set): the AppImage's
# update information points at a .zsync, so a release without one cannot
# update. A local build only warns.
echo "==> Generating .zsync sidecar"
if ! command -v zsyncmake >/dev/null 2>&1; then
    if [[ -n "${CI:-}" ]]; then
        echo "==> ERROR: zsyncmake is not installed (install the zsync package)" >&2
        exit 1
    fi
    echo "==> WARNING: zsyncmake not found — continuing without .zsync"
elif zsyncmake "$OUT"; then
    echo "==> .zsync generated: $OUT.zsync"
elif [[ -n "${CI:-}" ]]; then
    echo "==> ERROR: zsyncmake failed" >&2
    exit 1
else
    echo "==> WARNING: zsyncmake failed — continuing without .zsync"
fi
