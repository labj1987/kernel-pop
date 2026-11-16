#!/usr/bin/env bash
# build-appimage.sh — build the Kernel Pop AppImage.
# Run from the repo root on Ubuntu (CI uses ubuntu-24.04) as an ordinary user: it
# writes only inside the checkout. Only its from-scratch dependency install below
# needs root, and it uses sudo for that when not already root.
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
# one part of this script that needs root. In CI the workflow installs the UI
# toolkit's build headers and the packaging tools before this script runs, so the
# guard is false there. A failed index refresh (an unrelated third-party repo) is
# tolerated; only a failed install is fatal.
# The UI toolkit (GPUI) links libxcb, libxkbcommon and libxkbcommon-x11 and one of its crates
# probes fontconfig at build time (the binary does not load it), so their headers are the sign
# of a machine that can build it.
if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists xcb xkbcommon xkbcommon-x11 fontconfig 2>/dev/null; then
    echo "==> Installing build dependencies"
    AS_ROOT=()
    if [[ $EUID -ne 0 ]]; then AS_ROOT=(sudo); fi
    "${AS_ROOT[@]}" apt-get update -qq || true
    "${AS_ROOT[@]}" apt-get install -y -qq cargo rustc build-essential pkg-config \
        libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev \
        zsync wget file desktop-file-utils
fi
# On a machine that already has the toolchain (CI: the workflow installs the packaging
# tools) nothing above runs, so a missing tool is reported here instead of mid-build.
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

# Cargo builds into CARGO_TARGET_DIR when it is set (e.g. a target dir shared by several
# checkouts), else into target/.
# The binary goes in as cargo built it. It brings no libraries: the UI toolkit draws everything
# itself and takes only the windowing and graphics libraries every desktop has from the system
# (AppRun's libcheck names them).
cp "${CARGO_TARGET_DIR:-target}/release/$APP" "$APPDIR/usr/bin/"
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

# The app's own licence, and the notices of everything built into the binary: every Rust crate
# of the release build with its licence text (the UI toolkit's assets crate carries the Lucide
# icons' ISC licence).
mkdir -p "$APPDIR/usr/share/licenses/$APP"
cp LICENSE "$APPDIR/usr/share/licenses/$APP/"
python3 scripts/third_party_licenses.py "$APPDIR/usr/share/licenses/$APP/THIRD-PARTY-LICENSES.txt"

# ── AppRun ────────────────────────────────────────────────────────────
# On first launch the privileged script and the polkit policy
# must exist at fixed system paths (polkit refuses relative/user paths), so
# AppRun installs them via pkexec when missing or outdated, then execs the
# app. When that install is cancelled or fails, the app is still started, but
# it sees the mismatch itself (KERNEL_POP_BUNDLE points it at the bundled
# copies), shows why, and refuses to run a helper from another version.
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
APP="kernel-pop"

# >>> libcheck: before any setup, make sure the app can start. Kernel Pop brings no libraries
# of its own. It is linked against glibc, libgcc_s, libxcb, libxkbcommon and libxkbcommon-x11:
# `ldd -r` on the binary must report no missing library, no missing symbol version (a glibc older
# than the build's 2.39, for one) and no undefined symbol of the binary. And it loads by name at
# run time, where ldd cannot see them, libwayland-client on a Wayland session and a graphics
# loader, Vulkan's or EGL's (libcheck_runtime). A system with a loader but no working driver is
# the app's own business: it says so in a dialog of its own.
# LIBCHECK_LDD, LIBCHECK_LDCONFIG, LIBCHECK_OS_RELEASE, LIBCHECK_GLIBC_HAVE and LIBCHECK_DIALOG=none
# exist for the tests.

# libcheck_scan BINARY: sorts what `ldd -r BINARY` reports into LIBCHECK_LIBS (libraries not
# found), LIBCHECK_GLIBC (the newest GLIBC_x.y version that is needed but missing),
# LIBCHECK_OTHER (other "not found" lines), LIBCHECK_SYMBOLS (undefined symbols) and LIBCHECK_RC
# (ldd's exit status). Without ldd, or with "loader" as the second argument, the dynamic loader
# is asked directly, as ldd -r does (LIBCHECK_LOADER replaces it in the tests).
libcheck_scan() {
    local out line rc=0 re='version .GLIBC_([0-9]+(\.[0-9]+)*). not found'
    LIBCHECK_LIBS=(); LIBCHECK_GLIBC=""; LIBCHECK_OTHER=(); LIBCHECK_SYMBOLS=()
    if [[ "${2:-}" == loader ]]; then
        if [[ -n "${LIBCHECK_LOADER:-}" ]]; then
            out="$("$LIBCHECK_LOADER" "$1" 2>&1)" || rc=$?
        else
            out="$(LD_TRACE_LOADED_OBJECTS=1 LD_WARN=yes LD_BIND_NOW=yes "$1" 2>&1)" || rc=$?
        fi
    elif [[ -n "${LIBCHECK_LDD:-}" ]]; then
        out="$("$LIBCHECK_LDD" -r "$1" 2>&1)" || rc=$?
    elif command -v ldd >/dev/null 2>&1; then
        out="$(ldd -r "$1" 2>&1)" || rc=$?
    else
        out="$(LD_TRACE_LOADED_OBJECTS=1 LD_WARN=yes LD_BIND_NOW=yes "$1" 2>&1)" || rc=$?
    fi
    while IFS= read -r line; do
        line="${line#"${line%%[![:space:]]*}"}"
        if [[ "$line" == *"=> not found"* ]]; then
            LIBCHECK_LIBS+=("${line%% *}")
        elif [[ "$line" =~ $re ]]; then
            if [[ -z "$LIBCHECK_GLIBC" ]] || [[ "$(printf '%s\n' "$LIBCHECK_GLIBC" "${BASH_REMATCH[1]}" | sort -V | tail -n1)" != "$LIBCHECK_GLIBC" ]]; then
                LIBCHECK_GLIBC="${BASH_REMATCH[1]}"
            fi
        elif [[ "$line" == *"not found"* ]]; then
            LIBCHECK_OTHER+=("$line")
        elif [[ "$line" == "undefined symbol: "* ]] && libcheck_ours "$line" "$1"; then
            line="${line#undefined symbol: }"
            LIBCHECK_SYMBOLS+=("${line%%[[:space:]]*}")
        elif [[ "$line" == "symbol "*" version "*" not defined in file "* ]] && libcheck_ours "$line" "$1"; then
            line="${line#symbol }"
            LIBCHECK_SYMBOLS+=("${line%% *}")
        fi
    done <<<"$out"
    LIBCHECK_RC=$rc
}

# libcheck_ours LINE BINARY: whether the object an unresolved-symbol line ends with, "(OBJECT)", is
# BINARY. A system library's own unresolved (lazy) symbol is its business.
libcheck_ours() {
    local obj="${1##*(}"
    [[ "${obj%)}" == "$2" ]]
}

# libcheck_cache: puts the dynamic loader's cache listing (`ldconfig -p`) in LIBCHECK_CACHE, or
# nothing when there is no usable one: no ldconfig, or a cache that does not even list glibc (a
# system that keeps its libraries elsewhere, like NixOS). Without it nothing is said about the
# run-time libraries.
libcheck_cache() {
    local tool out
    LIBCHECK_CACHE=""
    for tool in "${LIBCHECK_LDCONFIG:-}" ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
        [[ -n "$tool" ]] || continue
        command -v "$tool" >/dev/null 2>&1 || continue
        out="$("$tool" -p 2>/dev/null)" || continue
        [[ "$out" == *$'\t'"libc.so.6 (libc6,x86-64"* ]] || return 0
        LIBCHECK_CACHE="$out"
        return 0
    done
}

# libcheck_has LIBRARY: whether the loader would find the 64-bit LIBRARY: it is in the cache, or
# in a directory of LD_LIBRARY_PATH.
libcheck_has() {
    local dir
    [[ "$LIBCHECK_CACHE" == *$'\t'"$1 (libc6,x86-64"* ]] && return 0
    local IFS=:
    for dir in ${LD_LIBRARY_PATH:-}; do
        [[ -n "$dir" && -e "$dir/$1" ]] && return 0
    done
    return 1
}

# libcheck_runtime: adds to LIBCHECK_LIBS the missing libraries among those the app loads by name:
# libwayland-client.so.0 on a Wayland session, and libvulkan.so.1 with libEGL.so.1 when both are
# missing (the window is drawn with either). LIBCHECK_NOTE gets the sentence that says so.
libcheck_runtime() {
    LIBCHECK_NOTE=""
    libcheck_cache
    [[ -n "$LIBCHECK_CACHE" ]] || return 0
    if [[ -n "${WAYLAND_DISPLAY:-}" ]] && ! libcheck_has libwayland-client.so.0; then
        LIBCHECK_LIBS+=(libwayland-client.so.0)
    fi
    if ! libcheck_has libvulkan.so.1 && ! libcheck_has libEGL.so.1; then
        LIBCHECK_LIBS+=(libvulkan.so.1 libEGL.so.1)
        LIBCHECK_NOTE=" The window is drawn with Vulkan or OpenGL ES, so one of libvulkan.so.1 and libEGL.so.1 is enough."
    fi
}

# libcheck_glibc_version: the system's glibc version, or nothing when it can't be told.
libcheck_glibc_version() {
    local v
    if [[ -n "${LIBCHECK_GLIBC_HAVE:-}" ]]; then
        echo "$LIBCHECK_GLIBC_HAVE"
        return 0
    fi
    v="$(getconf GNU_LIBC_VERSION 2>/dev/null)"
    v="${v#glibc }"
    if [[ -z "$v" ]] && command -v ldd >/dev/null 2>&1; then
        v="$(ldd --version 2>/dev/null | head -n1)"
        v="${v##* }"
    fi
    [[ "$v" =~ ^[0-9]+(\.[0-9]+)+$ ]] && echo "$v"
    return 0
}

# libcheck_package MANAGER LIBRARY: the apt or pacman package that provides LIBRARY, for the
# libraries the AppImage takes from the system; nothing for any other library.
libcheck_package() {
    local so apt pacman
    while read -r so apt pacman; do
        [[ "$so" == "$2" ]] || continue
        if [[ "$1" == apt ]]; then echo "$apt"; else echo "$pacman"; fi
        return 0
    done <<'TABLE'
libxcb.so.1 libxcb1 libxcb
libxkbcommon.so.0 libxkbcommon0 libxkbcommon
libxkbcommon-x11.so.0 libxkbcommon-x11-0 libxkbcommon-x11
libwayland-client.so.0 libwayland-client0 wayland
libvulkan.so.1 libvulkan1 vulkan-icd-loader
libEGL.so.1 libegl1 libglvnd
libgcc_s.so.1 libgcc-s1 gcc-libs
TABLE
}

# libcheck_install_hint LIBRARY...: how to install the packages that provide LIBRARY..., as one
# or two sentences, for the system's package manager; nothing for an unknown distribution.
# dnf and zypper install by library name (rpm's provides); apt and pacman get the package names
# of the usual host libraries, and the command that finds the package for any other one.
libcheck_install_hint() {
    local f="${LIBCHECK_OS_RELEASE:-/etc/os-release}" ids id mgr="" cmd lib pkg out=""
    local pkgs=() unknown=()
    [[ -r "$f" ]] || return 0
    # shellcheck disable=SC1090
    ids="$(. "$f" 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")"
    for id in $ids; do
        case "$id" in
            fedora|rhel|centos|rocky|almalinux) mgr=dnf ;;
            opensuse*|suse|sles) mgr=zypper ;;
            debian|ubuntu) mgr=apt ;;
            arch) mgr=pacman ;;
            *) continue ;;
        esac
        break
    done
    case "$mgr" in
        dnf|zypper)
            cmd="sudo $mgr install"
            for lib in "$@"; do cmd+=" '$lib()(64bit)'"; done
            echo "Install them with: $cmd."
            ;;
        apt|pacman)
            for lib in "$@"; do
                pkg="$(libcheck_package "$mgr" "$lib")"
                if [[ -z "$pkg" ]]; then
                    unknown+=("$lib")
                elif [[ " ${pkgs[*]} " != *" $pkg "* ]]; then
                    pkgs+=("$pkg")
                fi
            done
            if ((${#pkgs[@]})); then
                if [[ "$mgr" == apt ]]; then cmd="sudo apt install"; else cmd="sudo pacman -S"; fi
                out="Install them with: $cmd ${pkgs[*]}."
            fi
            if ((${#unknown[@]})); then
                if [[ "$mgr" == apt ]]; then cmd="apt-file search"; else cmd="pacman -F"; fi
                if ((${#pkgs[@]})); then lib="each of the others"; else lib="each of them"; fi
                out+="${out:+ }Find the package that provides $lib with:"
                for lib in "${unknown[@]}"; do out+=" $cmd $lib;"; done
                out="${out%;}."
            fi
            echo "$out"
            ;;
    esac
}

# libcheck_list ITEM...: "a, b, c".
libcheck_list() {
    local IFS=,
    local s="$*"
    echo "${s//,/, }"
}

# libcheck_plain TEXT: TEXT with nothing kdialog (Qt rich text) or a notification server (body
# markup) would take as markup: no &, < or >.
libcheck_plain() {
    local t="${1//&/and}"
    t="${t//</‹}"
    echo "${t//>/›}"
}

# libcheck_gvariant TEXT: TEXT as a GVariant text-format string, the way gdbus reads its arguments.
libcheck_gvariant() {
    local bs='\' q='"' nl=$'\n' t
    t="${1//"$bs"/"$bs$bs"}"
    t="${t//"$q"/"$bs$q"}"
    printf '"%s"' "${t//"$nl"/"${bs}n"}"
}

# libcheck_show TITLE TEXT: TEXT on stderr and, on a desktop, through the first of these that
# shows it: zenity, kdialog, notify-send, the desktop's notification service over D-Bus (gdbus:
# Pop!_OS COSMIC has none of the others) and xmessage. A dialog ends with 0 or 1 once it was
# seen; the others only with 0 (xmessage ends with 1 when it can't open the display).
libcheck_show() {
    local plain
    printf '%s: %s\n' "$1" "$2" >&2
    [[ "${LIBCHECK_DIALOG:-}" == none ]] && return 0
    [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] || return 0
    plain="$(libcheck_plain "$2")"
    if command -v zenity >/dev/null 2>&1; then
        zenity --error --no-markup --title="$1" --text="$2" --width=460 >/dev/null 2>&1
        (($? <= 1)) && return 0
    fi
    if command -v kdialog >/dev/null 2>&1; then
        kdialog --error "$plain" --title "$1" >/dev/null 2>&1
        (($? <= 1)) && return 0
    fi
    if command -v notify-send >/dev/null 2>&1; then
        notify-send -u critical "$1" "$plain" >/dev/null 2>&1 && return 0
    fi
    if command -v gdbus >/dev/null 2>&1; then
        gdbus call --session --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
            --method org.freedesktop.Notifications.Notify "$(libcheck_gvariant "$1")" 0 '"dialog-error"' \
            "$(libcheck_gvariant "$1")" "$(libcheck_gvariant "$plain")" '[]' "{'urgency': <byte 2>}" 0 >/dev/null 2>&1 && return 0
    fi
    if command -v xmessage >/dev/null 2>&1; then
        xmessage -center "$1: $2" >/dev/null 2>&1
    fi
    return 0
}

# libcheck_run NAME BINARY: returns 1 (after showing why) when BINARY can't start here.
libcheck_run() {
    local msg have hint more=""
    libcheck_scan "$2"
    if ((LIBCHECK_RC != 0)) && [[ -z "$LIBCHECK_GLIBC" ]] && ((${#LIBCHECK_LIBS[@]} + ${#LIBCHECK_OTHER[@]} + ${#LIBCHECK_SYMBOLS[@]} == 0)); then
        # ldd failed without saying why: ask the loader itself, and stop only if it names a problem.
        libcheck_scan "$2" loader
    fi
    LIBCHECK_NOTE=""
    [[ -n "$LIBCHECK_GLIBC" ]] || libcheck_runtime
    if [[ -n "$LIBCHECK_GLIBC" ]]; then
        have="$(libcheck_glibc_version)"
        msg="$1 can't start: it needs glibc $LIBCHECK_GLIBC or newer; this system has ${have:-an older one}."
        msg+=" This system is too old for this version of $1."
    elif ((${#LIBCHECK_LIBS[@]})); then
        msg="$1 can't start because these system libraries are missing: $(libcheck_list "${LIBCHECK_LIBS[@]}")."
        msg+=" $1 takes them from the system (windowing and graphics libraries every desktop normally has).$LIBCHECK_NOTE"
        hint="$(libcheck_install_hint "${LIBCHECK_LIBS[@]}")"
        if [[ -n "$hint" ]]; then
            msg+=" $hint Then start $1 again."
        else
            msg+=" Install the packages that provide them with your package manager, then start $1 again."
        fi
    elif ((${#LIBCHECK_SYMBOLS[@]} + ${#LIBCHECK_OTHER[@]})); then
        if ((${#LIBCHECK_SYMBOLS[@]} > 3)); then more=" and $((${#LIBCHECK_SYMBOLS[@]} - 3)) more"; fi
        if ((${#LIBCHECK_SYMBOLS[@]})); then
            msg="$1 can't start because the system's libraries are too old: they lack $(libcheck_list "${LIBCHECK_SYMBOLS[@]:0:3}")$more."
        else
            msg="$1 can't start because the system's libraries are too old: ${LIBCHECK_OTHER[0]}."
        fi
        msg+=" Update the system, then start $1 again."
    else
        return 0
    fi
    libcheck_show "$1" "$msg"
    return 1
}
# <<< libcheck

# >>> debian_based: Kernel Pop builds .deb packages and installs them with dpkg, so it only works on
# Debian, Ubuntu and their derivatives. The same rule as system.rs (unsupported_system): dpkg and
# apt-get on PATH, and an os-release whose ID or ID_LIKE names debian or ubuntu (with no
# os-release at all: /var/lib/dpkg/status exists). dpkg and apt-get alone are not enough: Fedora
# and Arch package them too.
# DEBIAN_BASED_DPKG_STATUS exists for the tests; LIBCHECK_OS_RELEASE as in libcheck.
debian_based() {
    local f="${LIBCHECK_OS_RELEASE:-/etc/os-release}" ids id
    command -v dpkg >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1 || return 1
    [[ -r "$f" || -n "${LIBCHECK_OS_RELEASE:-}" ]] || f=/usr/lib/os-release
    if [[ -r "$f" ]]; then
        # shellcheck disable=SC1090
        ids="$(. "$f" 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")"
        for id in $ids; do
            [[ "$id" == debian || "$id" == ubuntu ]] && return 0
        done
        return 1
    fi
    [[ -e "${DEBIAN_BASED_DPKG_STATUS:-/var/lib/dpkg/status}" ]]
}
# <<< debian_based

# >>> instance_running: whether a copy of Kernel Pop already runs in this login. The running
# copy holds flock on $XDG_RUNTIME_DIR/kernel-pop.sock.alive for its whole life (instance.rs);
# flock -n gives up on it with 75 then. Anything else (no runtime dir, no flock(1), no file) means
# no.
instance_running() {
    local alive="${XDG_RUNTIME_DIR:-}/kernel-pop.sock.alive" rc=0
    [[ -n "${XDG_RUNTIME_DIR:-}" && -e "$alive" ]] || return 1
    command -v flock >/dev/null 2>&1 || return 1
    flock -n -E 75 "$alive" true 2>/dev/null || rc=$?
    ((rc == 75))
}
# <<< instance_running

# A copy is already running: the binary hands this launch over to it straight away (its window
# is raised with this launch's activation token). Library checks or a password prompt first
# would only delay that, and could let the token go stale.
if instance_running; then
    export KERNEL_POP_BUNDLE="$HERE"
    export PATH="$HERE/usr/bin:$PATH"
    exec "$HERE/usr/bin/$APP" "$@"
fi

libcheck_run "Kernel Pop" "$HERE/usr/bin/$APP" || exit 1

# >>> helper_install: installing the privileged script and the polkit policy with pkexec.
# HELPER_PKEXEC exists for the tests.

# helper_pkexec ARG...: pkexec ARG... in a session of its own, with no controlling terminal and
# stdin from /dev/null. A desktop session started from a text console (Hyprland or sway run from
# tty1) passes that console on to every app as its controlling terminal, and pkexec would put its
# own text password prompt there, on a console nobody looks at, and wait for ever. Without one it
# uses the desktop's polkit agent, or fails at once when there is none.
helper_pkexec() {
    local pkexec="${HELPER_PKEXEC:-pkexec}"
    if command -v setsid >/dev/null 2>&1; then
        setsid -w "$pkexec" "$@" </dev/null
    else
        "$pkexec" "$@" </dev/null
    fi
}

# helper_failure RC: why the install through pkexec ended with RC. pkexec ends with 126 when the
# password dialog was dismissed and 127 when authorization failed or no agent could ask (KDE's
# agent's Cancel also gives 127).
helper_failure() {
    case $1 in
        126|127) echo "authorization was cancelled or refused. If no password dialog appeared, no polkit authentication agent is running: start one (hyprpolkitagent on Hyprland, polkit-gnome or lxqt-policykit on sway or i3; policykit-1-gnome on Debian/Ubuntu) and try again" ;;
        *)       echo "the install failed (exit code $1)" ;;
    esac
}

# helper_install SRC_SCRIPT SRC_POLICY DST_SCRIPT DST_POLICY: installs both, or says on stderr why
# not (and returns 1).
helper_install() {
    local stage h_script h_policy rc=0
    if ! command -v "${HELPER_PKEXEC:-pkexec}" >/dev/null 2>&1; then
        echo "kernel-pop: the privileged helper was not installed: pkexec (polkit) is missing." \
             "Install it with: sudo apt install pkexec (policykit-1 on older releases), then restart Kernel Pop." \
             "Until then installing, removing and signing kernels stay off." >&2
        return 1
    fi
    # The AppImage's FUSE mount is normally not readable by root, so the
    # files are staged in a private user dir. To close the stage-then-install
    # TOCTOU window, hashes are taken from the read-only mounted originals and
    # the root side copies the staged files into a root-owned dir and refuses
    # to install anything that doesn't match those hashes.
    stage="$(mktemp -d)"
    cp "$1" "$stage/privileged-install.sh"
    cp "$2" "$stage/policy"
    h_script="$(sha256sum "$1" | cut -d' ' -f1)"
    h_policy="$(sha256sum "$2" | cut -d' ' -f1)"
    helper_pkexec bash -c '
        set -euo pipefail
        stage="$1"; hs="$2"; hp="$3"; dst_s="$4"; dst_p="$5"
        safe="$(mktemp -d)"; trap "rm -rf \"$safe\"" EXIT
        cp "$stage/privileged-install.sh" "$safe/s"
        cp "$stage/policy" "$safe/p"
        [[ "$(sha256sum "$safe/s" | cut -d" " -f1)" == "$hs" ]] || { echo "script hash mismatch" >&2; exit 1; }
        [[ "$(sha256sum "$safe/p" | cut -d" " -f1)" == "$hp" ]] || { echo "policy hash mismatch" >&2; exit 1; }
        install -D -m 755 "$safe/s" "$dst_s"
        install -D -m 644 "$safe/p" "$dst_p"
    ' _ "$stage" "$h_script" "$h_policy" "$3" "$4" || rc=$?
    rm -rf "$stage"
    if ((rc != 0)); then
        echo "kernel-pop: the privileged helper was not installed or updated: $(helper_failure "$rc")." \
             "Installing, removing and signing kernels stay off; restart Kernel Pop to try again." >&2
        return 1
    fi
}
# <<< helper_install

SRC_SCRIPT="$HERE/usr/lib/$APP/privileged-install.sh"
SRC_POLICY="$HERE/usr/share/polkit-1/actions/io.github.labj1987.KernelPop.policy"
DST_SCRIPT="/usr/lib/$APP/privileged-install.sh"
DST_POLICY="/usr/share/polkit-1/actions/io.github.labj1987.KernelPop.policy"

needs_install=0
for pair in "$SRC_SCRIPT:$DST_SCRIPT" "$SRC_POLICY:$DST_POLICY"; do
    if [[ ! -f "${pair#*:}" ]] || ! cmp -s "${pair%%:*}" "${pair#*:}"; then
        needs_install=1
    fi
done

# Not on a system Kernel Pop can't install kernels on: no password prompt for a helper nothing uses.
if [[ $needs_install -eq 1 ]] && debian_based; then
    helper_install "$SRC_SCRIPT" "$SRC_POLICY" "$DST_SCRIPT" "$DST_POLICY"
fi

export KERNEL_POP_BUNDLE="$HERE"
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
