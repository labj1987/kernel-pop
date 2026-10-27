#!/usr/bin/env bash
# privileged-install.sh — runs as root via pkexec.
#
# THE WHOLE POINT OF THIS SCRIPT
# ------------------------------
# ukuu and hand-rolled scripts have repeatedly installed kernels without
# generating the initramfs, because they parsed the kernel version out of a
# FILENAME with a regex and an unexpected naming convention broke the parse.
# One reboot later: VFS panic.
#
# This script never parses filenames. The kernel version is read from the
# .deb package metadata (dpkg-deb -f Package), cross-checked against the
# /lib/modules directory that actually appears after install, and the
# initramfs is VERIFIED to exist on disk before the script reports success.
# If the initramfs is missing, this script fails loudly instead of leaving
# an unbootable kernel behind.
#
# Usage:
#   privileged-install.sh --install <build-dir>      (the .debs Kernel Pop built, plus
#                                                     the SHA256SUMS manifest it wrote)
#   privileged-install.sh --remove  <kernel-version-string>
#   privileged-install.sh --remove-many <kernel-version-string>...
#   privileged-install.sh --boot-once <kernel-version-string>
#   privileged-install.sh --setup-signing        (enrollment password on stdin)
#   privileged-install.sh --sign <kernel-version-string>
#   privileged-install.sh --install-build-deps <package>...  (only names from
#                                                     BUILD_DEPS_ALLOWED below)
#
# Output protocol: every line starts with "[kernel-pop] ". The app counts and
# reacts to these markers at the start of a line's text, so each one is used
# once per problem and never in a summary line:
#   DKMS WARNING:  one DKMS problem        SIGN WARNING:  one signing problem
#   BOOT PENDING:  it boots once the signing key's enrollment is confirmed
# Summaries use "DKMS SUMMARY:" / "SIGN SUMMARY:", which are not counted.

set -euo pipefail

LOGFILE="${KERNEL_POP_LOG:-/var/log/kernel-pop.log}"
log() {
    local msg="[kernel-pop] $*"
    echo "$msg"
    { echo "$(date '+%Y-%m-%d %H:%M:%S') $msg" >> "$LOGFILE"; } 2>/dev/null || true
}
die() { log "ERROR: $*"; exit 1; }

MODE="${1:-}"
ARG="${2:-}"

# Test overrides (pkexec clears the environment, so they never reach a real
# run): KERNEL_POP_BOOT, KERNEL_POP_MODULES, KERNEL_POP_ROOTFS,
# KERNEL_POP_MOK_DIR, KERNEL_POP_SIGNED_DIR, KERNEL_POP_STAGING,
# KERNEL_POP_EFI, KERNEL_POP_EFIVARS, KERNEL_POP_MOK_PENDING, KERNEL_POP_DKMS_MOK,
# KERNEL_POP_GRUB_CFG, KERNEL_POP_GRUB_DEFAULTS, KERNEL_POP_DKMS_TREE.
boot_dir()    { echo "${KERNEL_POP_BOOT:-/boot}"; }
modules_dir() { echo "${KERNEL_POP_MODULES:-/lib/modules}"; }

# ── Boot loader detection ───────────────────────────────────────────────
# GRUB and systemd-boot binaries/config can both be present on a machine
# (leftover packages, distro upgrades) — the signal that matters is which
# one an ESP loader.conf says is actually active, not merely which
# binaries exist. Prints one of: "kernelstub <esp-path>" | "systemd-boot
# <esp-path>" | "grub" | "unknown".
# Pop!_OS uses kernelstub on top of systemd-boot: it writes a single
# Pop_OS-current.conf entry (not per-kernel-version) and copies the kernel
# to EFI/Pop_OS-*/vmlinuz.efi, so it is checked for before systemd-boot.
detect_bootloader() {
    if command -v kernelstub >/dev/null 2>&1 || [[ -f /etc/kernelstub/configuration ]]; then
        local kesp=""
        if command -v bootctl >/dev/null 2>&1; then
            kesp="$(bootctl --print-esp-path 2>/dev/null)" || kesp=""
        fi
        [[ -n "$kesp" ]] || kesp="/boot/efi"
        echo "kernelstub $kesp"
        return
    fi
    if [[ -d /sys/firmware/efi ]] && command -v bootctl >/dev/null 2>&1; then
        local esp
        esp="$(bootctl --print-esp-path 2>/dev/null)" || esp=""
        if [[ -n "$esp" && -f "$esp/loader/loader.conf" ]]; then
            echo "systemd-boot $esp"
            return
        fi
    fi
    if command -v update-grub >/dev/null 2>&1; then
        echo "grub"
        return
    fi
    echo "unknown"
}

# ── Find a kernel's GRUB menu entry path from the generated grub.cfg ────
# Entry IDs (menuentry_id_option) are per-machine random UUIDs baked in by
# grub-mkconfig — never /etc/machine-id, and never guessable — so they must
# be read out of the actual config, not constructed. Prints the path for
# grub-set-default / grub-reboot: "<submenu-id>><entry-id>" for an entry in
# the "Advanced options" submenu, or just "<entry-id>" for a top-level entry
# (GRUB_DISABLE_SUBMENU=y puts every kernel at the top level). Returns 1 when
# there is no entry for <kver>.
#
# An entry matches when its title ends in exactly "Linux <kver>", compared as
# a plain string: recovery entries ("... (recovery mode)") and other systems'
# entries from os-prober ("... (on /dev/sda2)") never match, and neither does
# a kernel whose version merely contains <kver>. grub-mkconfig writes
# top-level menuentry/submenu lines and their closing braces at column 0 and
# everything inside a submenu indented, which is what tells the two apart.
grub_entry_path_for_kver() {
    local kver="$1" cfg="${KERNEL_POP_GRUB_CFG:-/boot/grub/grub.cfg}"
    [[ -f "$cfg" ]] || return 1

    local line sub="" in_top_entry=0 title id
    local re_sub="^submenu '[^']*'.*[\$]menuentry_id_option '([^']+)'"
    local re_entry="^[[:space:]]*menuentry '([^']*)'.*[\$]menuentry_id_option '([^']+)'"
    while IFS= read -r line; do
        if [[ "$line" =~ $re_sub ]]; then
            sub="${BASH_REMATCH[1]}"
        elif [[ "$line" =~ $re_entry ]]; then
            title="${BASH_REMATCH[1]}"
            id="${BASH_REMATCH[2]}"
            [[ "$line" == menuentry* ]] && in_top_entry=1
            if [[ "$title" == *"Linux $kver" ]]; then
                if [[ "$line" == menuentry* || -z "$sub" ]]; then
                    echo "$id"
                else
                    echo "${sub}>${id}"
                fi
                return 0
            fi
        elif [[ "$line" == "}"* ]]; then
            if (( in_top_entry )); then in_top_entry=0; else sub=""; fi
        fi
    done < "$cfg"
    return 1
}

# The systemd-boot loader entry file for <kver> under <esp>, or nothing.
# kernel-install writes "version <kver>" into each entry; the file name
# (<entry-token>-<kver>.conf, "+N-M" when boot counting is on) is the fallback
# for entries without that line.
sdboot_entry_for_kver() {
    local esp="$1" kver="$2" f line
    for f in "$esp"/loader/entries/*.conf; do
        [[ -f "$f" ]] || continue
        while IFS= read -r line; do
            line="${line%$'\r'}"
            if [[ "$line" =~ ^version[[:space:]]+(.*[^[:space:]])[[:space:]]*$ && "${BASH_REMATCH[1]}" == "$kver" ]]; then
                echo "$f"
                return 0
            fi
        done < "$f"
    done
    for f in "$esp"/loader/entries/*.conf; do
        [[ -f "$f" ]] || continue
        if [[ "$(sdboot_entry_id "$f")" == *"-$kver.conf" ]]; then
            echo "$f"
            return 0
        fi
    done
    return 0
}

# The entry ID systemd-boot uses for an entry file (what bootctl
# set-oneshot takes): the file name with any boot-counting suffix
# ("+3.conf", "+2-1.conf") removed.
sdboot_entry_id() {
    local name="${1##*/}"
    name="${name%.conf}"
    if [[ "$name" =~ ^(.+)\+[0-9]+(-[0-9]+)?$ ]]; then
        name="${BASH_REMATCH[1]}"
    fi
    echo "$name.conf"
}

# kernelstub (Pop!_OS) copies exactly two kernels to the ESP: the one the
# vmlinuz symlink points at as vmlinuz.efi, and the one vmlinuz.old points at
# as vmlinuz-previous.efi. Prints which of those two files is <kver>'s, or
# nothing when kernelstub keeps no copy of it. The symlinks live in / or in
# /boot depending on /etc/kernel-img.conf; / wins when it has them.
kernelstub_slot_for_kver() {
    local kver="$1" base
    base="${KERNEL_POP_ROOTFS:-}"
    [[ -L "$base/vmlinuz" ]] || base="$(boot_dir)"
    if [[ -L "$base/vmlinuz" && "$(basename "$(readlink "$base/vmlinuz")")" == "vmlinuz-$kver" ]]; then
        echo "vmlinuz.efi"
    elif [[ -L "$base/vmlinuz.old" && "$(basename "$(readlink "$base/vmlinuz.old")")" == "vmlinuz-$kver" ]]; then
        echo "vmlinuz-previous.efi"
    fi
    return 0
}

# The kernel image file the boot loader loads for <kver> from the ESP, or
# nothing when it has no ESP copy of it (GRUB reads /boot itself).
# Usage: esp_image_for_kver <bootloader> <esp> <kver>
esp_image_for_kver() {
    local bl="$1" esp="$2" kver="$3" entry rel slot
    case "$bl" in
        systemd-boot)
            entry="$(sdboot_entry_for_kver "$esp" "$kver")"
            if [[ -n "$entry" ]]; then
                rel="$(sed -n 's/^linux[[:space:]]\{1,\}//p' "$entry" | head -1)"
                if [[ -n "$rel" ]]; then echo "$esp/${rel#/}"; fi
            fi
            ;;
        kernelstub)
            slot="$(kernelstub_slot_for_kver "$kver")"
            if [[ -n "$slot" ]]; then
                compgen -G "$esp/EFI/Pop_OS-*/$slot" | head -1 || true
            fi
            ;;
    esac
    return 0
}

# Boot <kver> on the next boot only, leaving the saved default alone.
# Returns non-zero (with a log line) when that is not possible.
set_boot_once() {
    local bl="$1" esp="$2" kver="$3" entry_path entry id
    case "$bl" in
        grub)
            entry_path="$(grub_entry_path_for_kver "$kver")" \
                || { log "Could not find a GRUB menu entry for $kver in grub.cfg"; return 1; }
            grub-reboot "$entry_path" >>"$LOGFILE" 2>&1 \
                || { log "grub-reboot failed for $entry_path"; return 1; }
            log "GRUB will boot $kver on the next boot only ($entry_path)"
            ;;
        systemd-boot)
            entry="$(sdboot_entry_for_kver "$esp" "$kver")"
            [[ -n "$entry" ]] || { log "No systemd-boot entry found for $kver in $esp/loader/entries"; return 1; }
            id="$(sdboot_entry_id "$entry")"
            bootctl set-oneshot "$id" >>"$LOGFILE" 2>&1 \
                || { log "bootctl set-oneshot failed for $id"; return 1; }
            log "systemd-boot will boot $kver on the next boot only ($id)"
            ;;
        kernelstub)
            log "One-time boot selection is not supported with kernelstub (Pop!_OS); use the boot menu"
            return 1
            ;;
        *)
            log "Could not detect a supported boot loader — use your boot menu to pick $kver"
            return 1
            ;;
    esac
}

# Put the previously saved GRUB default back after a default change that
# did not verify. Best effort and conservative: does nothing when there was
# no previous default, and never fails the install.
restore_grub_default() {
    local prev="$1" now
    [[ -n "$prev" ]] || return 0
    if grub-set-default "$prev" >>"$LOGFILE" 2>&1; then
        now="$(grub-editenv list 2>/dev/null | sed -n 's/^saved_entry=//p')" || now=""
        if [[ "$now" == "$prev" ]]; then
            log "Restored previous GRUB default: $prev"
        else
            log "WARNING: tried to restore the previous GRUB default '$prev' but saved_entry is '$now' — check it before rebooting"
        fi
    else
        log "WARNING: could not restore the previous GRUB default '$prev' — check it before rebooting"
    fi
    return 0
}

# ── Derive kernel version strings from .deb metadata ──────────────────
# The image package's NAME (its Package field, never the file name) embeds
# the full version string, as make bindeb-pkg names it:
#   linux-image-7.2.12-kernelpop  ->  7.2.12-kernelpop
# Exactly one image package is expected.
kver_from_debs() {
    local dir="$1"
    local deb pkg found=""
    for deb in "$dir"/*.deb; do
        [[ -f "$deb" ]] || continue
        pkg="$(dpkg-deb -f "$deb" Package 2>/dev/null)" || continue
        case "$pkg" in
            linux-image-*-dbg) ;;
            linux-image-*)
                [[ -z "$found" ]] || return 1
                found="${pkg#linux-image-}" ;;
        esac
    done
    [[ -n "$found" ]] || return 1
    echo "$found"
}

# A kernel version string is spliced into globs and rm -rf paths, so it must
# be a plain token: no '*', '/', '..' or whitespace. Returns 0 if safe.
valid_kver() {
    [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] && [[ "$1" != *..* ]]
}

# ── Initramfs tool detection ────────────────────────────────────────────
# Prints "update-initramfs" | "dracut" | "none". Ubuntu ships
# initramfs-tools; dracut is only used when initramfs-tools is absent.
initramfs_tool() {
    if command -v update-initramfs >/dev/null 2>&1; then
        echo "update-initramfs"
    elif command -v dracut >/dev/null 2>&1; then
        echo "dracut"
    else
        echo "none"
    fi
}

# Generate /boot/initrd.img-<kver> with whichever tool is installed. The
# caller verifies the file exists afterwards, whatever the tool was.
generate_initramfs() {
    local kver="$1" tool boot
    tool="$(initramfs_tool)"
    boot="$(boot_dir)"
    case "$tool" in
        update-initramfs)
            if [[ -f "$boot/initrd.img-$kver" ]]; then
                update-initramfs -u -k "$kver" >>"$LOGFILE" 2>&1
            else
                update-initramfs -c -k "$kver" >>"$LOGFILE" 2>&1
            fi
            ;;
        dracut)
            # Write to the same initrd.img-<kver> name the rest of this
            # script and the app's health check look for.
            dracut --force "$boot/initrd.img-$kver" "$kver" >>"$LOGFILE" 2>&1
            ;;
        *)
            log "No initramfs tool found (update-initramfs or dracut)"
            return 1
            ;;
    esac
}

# ── DKMS ────────────────────────────────────────────────────────────────
# Out-of-tree modules (nvidia above all) are built per kernel. A failed
# build does not stop the kernel from booting, but the machine may come up
# without its GPU driver, so failures are reported loudly as warnings and
# never turn a successful kernel install into a failed one.
DKMS_FAILED=0

# "nvidia/550.54, 6.8.0-45-generic, x86_64: installed" (dkms 3) or
# "nvidia, 550.54, 6.8.0-45-generic, x86_64: installed" (dkms 2) -> the
# state word ("installed", "built", "added", ...).
dkms_line_state() {
    local rest="${1#*: }"
    echo "${rest%% *}"
}

# Same lines -> "nvidia/550.54" (module/version).
dkms_line_module() {
    local head="${1%%:*}" f
    IFS=', ' read -ra f <<< "$head"
    if [[ "${f[0]}" == */* ]]; then
        echo "${f[0]}"
    else
        echo "${f[0]}/${f[1]:-?}"
    fi
}

# Path of the make.log for a "module/version" and kernel, or nothing.
dkms_make_log() {
    compgen -G "${KERNEL_POP_DKMS_TREE:-/var/lib/dkms}/$1/$2/*/log/make.log" | head -1 || true
}

dkms_postcheck() {
    local kver="$1"
    if ! command -v dkms >/dev/null 2>&1; then
        log "DKMS not installed — no out-of-tree modules to build"
        return 0
    fi
    local registered
    registered="$(dkms status 2>/dev/null || true)"
    if [[ -z "$registered" ]]; then
        log "No DKMS modules registered — nothing to build for $kver"
        return 0
    fi
    if [[ ! -e "$(modules_dir)/$kver/build" ]]; then
        DKMS_FAILED=$((DKMS_FAILED + 1))
        log "DKMS WARNING: DKMS modules are registered but /lib/modules/$kver/build is missing (headers not installed?) — they cannot be built for $kver"
        return 0
    fi

    log "DKMS modules registered — building for $kver (this can take a few minutes)…"
    # Not a counted warning by itself: the per-module states below are what
    # decides whether anything is missing.
    dkms autoinstall -k "$kver" >>"$LOGFILE" 2>&1 \
        || log "dkms autoinstall reported errors for $kver — per-module results follow"

    local line tl state mod total=0 logf
    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        total=$((total + 1))
        state="$(dkms_line_state "$line")"
        mod="$(dkms_line_module "$line")"
        if [[ "$state" == "installed" ]]; then
            log "DKMS OK: $mod installed for $kver"
        else
            DKMS_FAILED=$((DKMS_FAILED + 1))
            log "DKMS WARNING: $mod is '$state' for $kver, not installed — the module will be missing after reboot"
            logf="$(dkms_make_log "$mod" "$kver")"
            if [[ -n "$logf" && -r "$logf" ]]; then
                log "DKMS make.log tail ($logf):"
                while IFS= read -r tl; do log "  | $tl"; done < <(tail -n 20 "$logf")
            fi
        fi
    done < <(dkms status -k "$kver" 2>/dev/null || true)

    if (( total == 0 )); then
        DKMS_FAILED=$((DKMS_FAILED + 1))
        log "DKMS WARNING: DKMS modules are registered but none is installed for $kver"
    fi
    return 0
}

# ── Kernel signing (Secure Boot) ────────────────────────────────────────
# Kernels built from source are unsigned. Once --setup-signing has created a key and
# queued it for MOK enrollment, every kernel this script installs is signed
# with it, whether or not Secure Boot is on right now. Kernel Pop has its own
# kernel-signing key: Ubuntu's module-signing MOK (/var/lib/shim-signed/mok)
# carries a module-only EKU that shim rejects for kernels. sbsign appends a
# signature, so a kernel that is already signed (by Canonical, say) keeps its
# original signature as well.
# With no key, nothing here runs except one "not set up" log line per install.
SIGN_FAILED=0
SIGN_KEY_CN="Kernel Pop kernel signing key"

mok_dir()    { echo "${KERNEL_POP_MOK_DIR:-/var/lib/kernel-pop/mok}"; }
signed_dir() { echo "${KERNEL_POP_SIGNED_DIR:-/var/lib/kernel-pop/signed}"; }

# All three key files are present.
signing_key_present() {
    local d
    d="$(mok_dir)"
    [[ -f "$d/signing.key" && -f "$d/signing.pem" && -f "$d/signing.der" ]]
}

# Create the signing key and certificate. Idempotent: an existing key is
# never overwritten. Fails (non-zero) without touching anything already there.
# The whole set is built in a sibling temp directory and published with one
# rename(2) of that directory onto the key directory, so the key directory
# either does not exist (or is empty) or holds all three files: a failure or
# a crash part way can never leave an incomplete set behind.
generate_signing_key() {
    local d parent tmpd
    d="$(mok_dir)"
    parent="$(dirname "$d")"
    if signing_key_present; then
        log "Signing key already exists in $d — keeping it"
        return 0
    fi
    if [[ -e "$d/signing.key" || -e "$d/signing.pem" || -e "$d/signing.der" ]]; then
        log "SIGN WARNING: $d holds an incomplete key set — refusing to overwrite it. Move it aside and run the setup again."
        return 1
    fi
    mkdir -p "$parent" || return 1
    chmod 0755 "$parent" 2>/dev/null || true
    tmpd="$(mktemp -d "$parent/.mok.new.XXXXXX")" || return 1
    cat > "$tmpd/req.cnf" <<EOF
[ req ]
distinguished_name = dn
prompt = no
x509_extensions = v3
[ dn ]
CN = $SIGN_KEY_CN
[ v3 ]
basicConstraints = critical,CA:FALSE
extendedKeyUsage = codeSigning
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
EOF
    if ! (umask 077 && openssl req -new -x509 -newkey rsa:2048 -nodes -days 36500 \
            -config "$tmpd/req.cnf" -keyout "$tmpd/signing.key" -out "$tmpd/signing.pem") \
            >>"$LOGFILE" 2>&1; then
        rm -rf "$tmpd"
        log "SIGN WARNING: openssl could not generate the signing key"
        return 1
    fi
    if ! openssl x509 -in "$tmpd/signing.pem" -outform DER -out "$tmpd/signing.der" >>"$LOGFILE" 2>&1; then
        rm -rf "$tmpd"
        log "SIGN WARNING: openssl could not convert the certificate to DER"
        return 1
    fi
    rm -f "$tmpd/req.cnf"
    if ! { chmod 0600 "$tmpd/signing.key" && chmod 0644 "$tmpd/signing.pem" "$tmpd/signing.der" \
            && chmod 0755 "$tmpd"; }; then
        rm -rf "$tmpd"
        log "SIGN WARNING: could not set the key file modes"
        return 1
    fi
    # rename(2) replaces the key directory only when it is missing or empty.
    if ! mv -T "$tmpd" "$d" >>"$LOGFILE" 2>&1; then
        rm -rf "$tmpd"
        log "SIGN WARNING: could not move the new key set into $d (is it a non-empty directory?)"
        return 1
    fi
    log "Created signing key in $d (CN=$SIGN_KEY_CN)"
}

# Whether /boot/vmlinuz-<kver> carries a signature from our key. Quiet.
kernel_signed() {
    local img
    img="$(boot_dir)/vmlinuz-$1"
    [[ -f "$img" ]] && sbverify --cert "$(mok_dir)/signing.pem" "$img" >/dev/null 2>&1
}

# Record that vmlinuz-<kver> is signed, as "<size> <mtime>": the unprivileged
# app cannot read a 0600 kernel image, but it can stat it and compare.
write_signed_marker() {
    local kver="$1" img d
    img="$(boot_dir)/vmlinuz-$kver"
    d="$(signed_dir)"
    mkdir -p "$d" && chmod 0755 "$d" || return 1
    stat -c '%s %Y' "$img" > "$d/$kver" && chmod 0644 "$d/$kver"
}

remove_signed_marker() {
    rm -f "$(signed_dir)/$1"
}

# Sign /boot/vmlinuz-<kver> in place. The signed image is written to a temp
# file in the same directory, verified, and only then moved over the
# original (keeping its mode and owner). Any failure removes the temp file,
# leaves the original untouched and returns non-zero. An image that already
# verifies against our key is left alone (returns 0).
sign_kernel() {
    local kver="$1" d img tmp
    d="$(mok_dir)"
    img="$(boot_dir)/vmlinuz-$kver"
    if [[ ! -f "$img" ]]; then
        log "SIGN WARNING: $img not found — nothing to sign"
        return 1
    fi
    if ! signing_key_present; then
        log "SIGN WARNING: no signing key in $d — run the signing setup first"
        return 1
    fi
    if kernel_signed "$kver"; then
        log "Already signed with the Kernel Pop key: $img"
        write_signed_marker "$kver" || log "SIGN WARNING: could not write the signed marker for $kver"
        return 0
    fi
    tmp="$(mktemp "$(dirname "$img")/.vmlinuz-$kver.signing.XXXXXX")" || {
        log "SIGN WARNING: could not create a temp file next to $img"
        return 1
    }
    if ! sbsign --key "$d/signing.key" --cert "$d/signing.pem" --output "$tmp" "$img" >>"$LOGFILE" 2>&1; then
        rm -f "$tmp"
        log "SIGN WARNING: sbsign failed for $img — left unchanged"
        return 1
    fi
    if ! sbverify --cert "$d/signing.pem" "$tmp" >>"$LOGFILE" 2>&1; then
        rm -f "$tmp"
        log "SIGN WARNING: the signed copy of $img did not verify — left unchanged"
        return 1
    fi
    if ! { chmod --reference="$img" "$tmp" && chown --reference="$img" "$tmp" && mv -f "$tmp" "$img"; }; then
        rm -f "$tmp"
        log "SIGN WARNING: could not replace $img with the signed copy — left unchanged"
        return 1
    fi
    write_signed_marker "$kver" || log "SIGN WARNING: could not write the signed marker for $kver"
    log "Signed: $img"
}

# After a kernel in /boot was signed, refresh the copy the boot loader really
# loads. GRUB reads /boot directly. systemd-boot and kernelstub got the
# unsigned image copied to the ESP by the dpkg postinst hook, so re-run the
# sync and check the ESP copy. Best effort: problems are SIGN WARNINGs only.
#
# kernelstub only keeps the current and the previous kernel on the ESP, so a
# kernel that is neither has no ESP copy: nothing to refresh or check. Bare
# `kernelstub` re-copies both of those from their symlinks.
sync_signed_to_esp() {
    local kver="$1" bl esp img="" boot slot
    boot="$(boot_dir)"
    read -r bl esp <<< "$(detect_bootloader)"
    case "$bl" in
        systemd-boot)
            if ! command -v kernel-install >/dev/null 2>&1; then
                log "SIGN WARNING: kernel-install not found — the ESP copy of $kver was not refreshed and is still unsigned"
                return 0
            fi
            # The initrd is passed explicitly: on initramfs-tools systems
            # kernel-install does not find it itself, and the refreshed entry
            # would lose its initrd line.
            local initrd=()
            [[ -f "$boot/initrd.img-$kver" ]] && initrd=("$boot/initrd.img-$kver")
            if ! kernel-install add "$kver" "$boot/vmlinuz-$kver" "${initrd[@]}" >>"$LOGFILE" 2>&1; then
                log "SIGN WARNING: kernel-install add failed for $kver — the ESP copy may still be unsigned"
                return 0
            fi
            ;;
        kernelstub)
            slot="$(kernelstub_slot_for_kver "$kver")"
            if [[ -z "$slot" ]]; then
                log "kernelstub keeps only the current and previous kernel on the ESP; $kver is neither, so it has no ESP copy to refresh"
                return 0
            fi
            if ! kernelstub >>"$LOGFILE" 2>&1; then
                log "SIGN WARNING: kernelstub failed — the ESP copy of $kver ($slot) may still be unsigned"
                return 0
            fi
            ;;
        *)
            return 0
            ;;
    esac
    img="$(esp_image_for_kver "$bl" "$esp" "$kver")"
    if [[ -z "$img" || ! -f "$img" ]]; then
        log "SIGN WARNING: could not find the ESP kernel image for $kver to check its signature"
    elif sbverify --cert "$(mok_dir)/signing.pem" "$img" >/dev/null 2>&1; then
        log "Verified: ESP copy $img is signed"
    else
        log "SIGN WARNING: the ESP copy $img does not carry the Kernel Pop signature"
    fi
    return 0
}

# Secure Boot is on and shim enforces it. mokutil sees shim's validation
# switch; the raw EFI variable is the fallback.
secure_boot_enforcing() {
    local out v
    if command -v mokutil >/dev/null 2>&1; then
        out="$(mokutil --sb-state 2>/dev/null)" || out=""
        [[ "$out" == *"validation is disabled"* ]] && return 1
        [[ "$out" == *"SecureBoot enabled"* ]] && return 0
        [[ "$out" == *"SecureBoot disabled"* ]] && return 1
    fi
    v="$(compgen -G '/sys/firmware/efi/efivars/SecureBoot-*' | head -1 || true)"
    [[ -n "$v" ]] || return 1
    [[ "$(od -An -t u1 -j4 -N1 "$v" 2>/dev/null | tr -d ' ')" == "1" ]]
}

# Enrollment state of Kernel Pop's certificate, from `mokutil --test-key`:
# enrolled | pending (queued, confirmed at MOK Manager on the next boot) |
# not-enrolled | unknown (mokutil missing or said something else).
key_enrollment_state() {
    local out
    out="$(mokutil --test-key "$(mok_dir)/signing.der" 2>/dev/null)" || true
    case "$out" in
        *"is already in the enrollment request"*) echo pending ;;
        *"is already enrolled"*)                  echo enrolled ;;
        *"is not enrolled"*)                      echo not-enrolled ;;
        *)                                        echo unknown ;;
    esac
}

# With Secure Boot enforcing, a kernel can only boot when it is signed with
# a key that is enrolled, or queued for enrollment (it boots once MOK
# Manager has enrolled it at the next start). So before anything is
# installed: with Secure Boot enforcing, the signing setup must be done, its
# tools present and the key enrolled or queued; otherwise the install is
# refused and nothing changes. Sets SB_PENDING=1 when the key is queued.
SB_PENDING=0
secure_boot_preflight() {
    ensure_efivarfs
    secure_boot_enforcing || return 0
    signing_key_present \
        || die "Secure Boot is enforcing and kernel signing is not set up, so a kernel built from source could not boot. Nothing was installed. Use Set Up Signing on the System tab first, or turn Secure Boot off."
    if ! command -v sbsign >/dev/null 2>&1 || ! command -v sbverify >/dev/null 2>&1; then
        die "Secure Boot is enforcing and the signing tools (sbsigntool) are missing, so the kernel could not be signed. Nothing was installed. Run Set Up Signing on the System tab again."
    fi
    case "$(key_enrollment_state)" in
        enrolled) ;;
        pending)  SB_PENDING=1 ;;
        not-enrolled)
            die "Secure Boot is enforcing and Kernel Pop's signing key is neither enrolled nor waiting for enrollment, so a kernel signed with it could not boot. Nothing was installed. Use Set Up Signing on the System tab again and confirm the enrollment at the next reboot." ;;
        *)
            die "Secure Boot is enforcing and Kernel Pop could not confirm that its signing key is enrolled (mokutil gave no answer), so the kernel might not boot. Nothing was installed." ;;
    esac
}

# Undo a just-finished dpkg install: purge exactly the packages of the
# staged .debs. Used when Secure Boot is enforcing and signing failed, so no
# kernel that shim would refuse stays installed (their postrm hooks take the
# kernel out of the boot menu again).
rollback_install() {
    local dir="$1" d pkgs=()
    for d in "$dir"/*.deb; do
        pkgs+=("$(dpkg-deb -f "$d" Package)")
    done
    log "Removing the just-installed packages again: ${pkgs[*]}"
    apt-get purge -y "${pkgs[@]}" >>"$LOGFILE" 2>&1
}

# MOK Manager takes 8-16 characters, typed on a US keyboard layout at boot.
valid_mok_password() {
    local LC_ALL=C
    [[ "$1" =~ ^[!-~]{8,16}$ ]]
}

# The crypt line from `mokutil --generate-hash` output (its prompts are
# printed too).
mok_hash_from_output() {
    grep -m1 '^\$6\$' || true
}

# ── Installed kernels ───────────────────────────────────────────────────
# One kernel version per line, from the vmlinuz-* files in /boot.
installed_kvers() {
    local boot f
    boot="$(boot_dir)"
    for f in "$boot"/vmlinuz-*; do
        [[ -e "$f" ]] || continue
        echo "${f#"$boot"/vmlinuz-}"
    done
}

# Newest version in a newline-separated list on stdin (version sort).
newest_kver_in_list() {
    sort -V | tail -n 1
}

# Only a kernel image and its headers may be installed as root through this
# helper. bindeb-pkg also builds linux-libc-dev, which would replace Ubuntu's
# own package, and can build a linux-image-*-dbg; both are refused here, not
# just left out by the app.
deb_allowed() {
    local pkg
    pkg="$(dpkg-deb -f "$1" Package 2>/dev/null)" || return 1
    case "$pkg" in
        linux-libc-dev*|linux-image-*-dbg) return 1 ;;
        linux-image-?*|linux-headers-?*) return 0 ;;
        *) return 1 ;;
    esac
}

# ── Verifying a build before it is installed ────────────────────────────
# The .debs and the SHA256SUMS manifest come from a directory the user owns:
# Kernel Pop built the packages as that user and wrote the manifest. The
# trust anchor is that user, who authenticated through pkexec for this run.
# Nothing in the directory is trusted beyond that: the set is copied into a
# root-only directory and every package's SHA256 is checked against the
# manifest there, and dpkg installs only those private copies, so nothing
# that can still be changed after the check is what gets installed.

# The uid the app runs as: pkexec says who asked.
caller_uid() { echo "${PKEXEC_UID:-${SUDO_UID:-0}}"; }

# A download path is used only when it is not a symlink, is owned by the
# caller and is not writable by group or others.
path_trusted() {
    local p="$1" uid owner mode
    uid="$(caller_uid)"
    if [[ -L "$p" ]]; then log "Refusing $p: it is a symlink"; return 1; fi
    [[ -e "$p" ]] || { log "Refusing $p: it does not exist"; return 1; }
    owner="$(stat -c '%u' "$p")"
    mode="$(stat -c '%a' "$p")"
    if [[ "$owner" != "$uid" ]]; then
        log "Refusing $p: owned by uid $owner, not by uid $uid"
        return 1
    fi
    if (( (8#$mode & 8#022) != 0 )); then
        log "Refusing $p: writable by group or others (mode $mode)"
        return 1
    fi
}

# Copy file <name> (relative to the current directory) to <dst>. The file is
# opened once, without following a symlink and without blocking on a FIFO,
# and every check is made on that open file: a regular file, owned by the
# caller, not writable by group or others. The copy is read from the same
# open file, so swapping the name for something else after the check has
# no effect. (perl is Essential on Debian/Ubuntu; bash cannot open with
# O_NOFOLLOW|O_NONBLOCK or fstat.)
copy_trusted_file() {
    perl -e '
        use strict; use warnings;
        use Fcntl qw(O_RDONLY O_NOFOLLOW O_NONBLOCK F_GETFL F_SETFL S_ISREG);
        my ($name, $dst, $uid) = @ARGV;
        sysopen(my $in, $name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK) or die "cannot open $name: $!\n";
        my @st = stat($in) or die "cannot stat $name: $!\n";
        S_ISREG($st[2]) or die "$name is not a regular file\n";
        $st[4] == $uid or die "$name is owned by uid $st[4], not by uid $uid\n";
        ($st[2] & 022) == 0 or die sprintf("%s is writable by group or others (mode %o)\n", $name, $st[2] & 07777);
        my $fl = fcntl($in, F_GETFL, 0) or die "fcntl: $!\n";
        fcntl($in, F_SETFL, $fl & ~O_NONBLOCK) or die "fcntl: $!\n";
        open(my $out, ">", $dst) or die "cannot write $dst: $!\n";
        binmode $in; binmode $out;
        while (1) {
            my $n = sysread($in, my $buf, 1 << 20);
            defined $n or die "read error on $name: $!\n";
            last if $n == 0;
            print {$out} $buf or die "write error on $dst: $!\n";
        }
        close $out or die "write error on $dst: $!\n";
    ' "$1" "$2" "$(caller_uid)" 2>&1
}

MANIFEST="SHA256SUMS"

# The SHA256 the manifest lists for package file <name>, or nothing. Lines
# are "<64 hex digits>  <file name>", as sha256sum writes them; the name is
# compared as a plain string.
manifest_hash_for() {
    local file="$1" name="$2" h f
    while read -r h f; do
        if [[ "$h" =~ ^[0-9a-fA-F]{64}$ && "$f" == "$name" ]]; then
            echo "${h,,}"
            return 0
        fi
    done < "$file"
    return 0
}

# Copy <src>'s manifest and .debs into the root-only <dst>, then verify every
# package there. Dies on any problem: a package missing from the manifest,
# or a manifest entry with no package, is an error, never "unverified".
# The directory is entered once and checked as ".", so every file is opened
# relative to that one directory even if <src> is swapped for a symlink.
stage_verified_debs() {
    local src="$1" dst="$2"
    [[ -L "$src" ]] && die "Refusing the package directory $src: it is a symlink"
    (
        cd -P -- "$src" 2>/dev/null || die "Cannot enter the package directory $src"
        path_trusted . || die "Refusing the package directory $src"
        local f name want got out n=0 h
        [[ -e "$MANIFEST" || -L "$MANIFEST" ]] || die "$MANIFEST is missing from $src — Kernel Pop installs only packages it built and listed"
        out="$(copy_trusted_file "$MANIFEST" "$dst/$MANIFEST")" || die "Refusing $src/$MANIFEST: $out"
        for f in ./*.deb; do
            [[ -e "$f" || -L "$f" ]] || continue
            name="${f#./}"
            want="$(manifest_hash_for "$dst/$MANIFEST" "$name")"
            [[ -n "$want" ]] || die "$name is not listed in $MANIFEST — refusing to install it"
            out="$(copy_trusted_file "$name" "$dst/$name")" || die "Refusing $src/$name: $out"
            got="$(sha256sum "$dst/$name" | cut -d' ' -f1)"
            [[ "$got" == "$want" ]] || die "SHA256 mismatch for $name (expected $want, got $got) — refusing to install"
            n=$((n + 1))
        done
        (( n > 0 )) || die "No .deb files found in $src"
        while read -r h f; do
            [[ -n "$h" ]] || continue
            [[ "$f" =~ ^[A-Za-z0-9][A-Za-z0-9._+~-]*\.deb$ ]] || die "$MANIFEST lists '$f', which is not a plain package file name"
            [[ -f "$dst/$f" ]] || die "$MANIFEST lists $f, which is not in $src — the set is incomplete"
        done < "$dst/$MANIFEST"
        log "Verified: $n package(s) match $MANIFEST"
    ) || exit 1
}

STAGE_DIR=""

do_install() {
    local src="$1" boot mods
    boot="$(boot_dir)"
    mods="$(modules_dir)"
    [[ -d "$src" ]] || die "Not a directory: $src"

    log "==== Kernel install started ===="
    log "Package directory: $src"

    # Everything below works on the private, verified copy only.
    STAGE_DIR="$(umask 077 && mktemp -d "${KERNEL_POP_STAGING:-/var/tmp}/kernel-pop-install.XXXXXX")" \
        || die "Could not create a private staging directory"
    trap 'rm -rf "$STAGE_DIR"' EXIT
    stage_verified_debs "$src" "$STAGE_DIR"
    local dir="$STAGE_DIR"

    local all_debs=("$dir"/*.deb) debs=() d
    for d in "${all_debs[@]}"; do
        deb_allowed "$d" || die "Refusing to install ${d##*/}: only a kernel image and its headers are installed (never linux-libc-dev or a -dbg package)"
        debs+=("$d")
    done

    # Version from package metadata, before anything is installed
    local kver
    kver="$(kver_from_debs "$dir")" || die "Could not read one kernel version from the package metadata (exactly one linux-image package is expected)"
    valid_kver "$kver" || die "The package metadata gives an invalid kernel version: '$kver'"
    log "Kernel version (from package metadata): $kver"
    local pkg
    for d in "${debs[@]}"; do
        pkg="$(dpkg-deb -f "$d" Package)"
        [[ "$pkg" == "linux-image-$kver" || "$pkg" == "linux-headers-$kver" ]] \
            || die "Refusing ${d##*/}: package $pkg does not belong to kernel $kver"
    done

    # Refuse up front what could not boot under Secure Boot.
    secure_boot_preflight

    # Snapshot the modules directory so the post-install cross-check is honest
    local before_modules
    before_modules="$(ls -1 "$mods" 2>/dev/null || true)"

    log "Installing ${#debs[@]} packages…"
    if ! dpkg -i "${debs[@]}" >>"$LOGFILE" 2>&1; then
        log "dpkg -i reported errors — attempting to fix dependencies…"
        apt-get install -f -y >>"$LOGFILE" 2>&1 || die "dpkg install failed and apt-get -f could not repair it"
    fi
    log "Packages installed"

    # Cross-check: the modules directory for $kver must now exist
    if [[ ! -d "$mods/$kver" ]]; then
        log "WARNING: $mods/$kver not found after install."
        local new_dir
        new_dir="$(comm -13 <(echo "$before_modules") <(ls -1 "$mods") | head -1 || true)"
        if [[ -n "$new_dir" ]] && valid_kver "$new_dir"; then
            log "Using newly appeared modules directory instead: $new_dir"
            kver="$new_dir"
        else
            die "No modules directory appeared for the installed kernel — aborting before initramfs"
        fi
    fi

    # dpkg can fail while apt-get -f finds nothing to fix and still exit 0;
    # never declare success unless the kernel image itself is on disk.
    [[ -f "$boot/vmlinuz-$kver" ]] || die "$boot/vmlinuz-$kver did NOT appear after install — the kernel package did not install correctly"
    log "Verified: $boot/vmlinuz-$kver exists"

    # ── Signing: only when the signing setup has been done ─────────────
    # With Secure Boot off, a failure is a warning, never an install error
    # (same as DKMS below). With it enforcing, an unsigned kernel could not
    # boot, so the install is undone instead.
    local signed_now=0
    if signing_key_present; then
        if sign_kernel "$kver"; then
            signed_now=1
        elif secure_boot_enforcing; then
            if rollback_install "$dir"; then
                die "$kver could not be signed (see the SIGN WARNING above), and with Secure Boot enforcing it could not boot, so it was removed again. Nothing is left installed. Try again, or check the signing setup on the System tab."
            fi
            die "$kver could not be signed and could not be removed again — it will NOT boot while Secure Boot is enforcing. Remove it on the System tab before rebooting."
        else
            SIGN_FAILED=$((SIGN_FAILED + 1))
            log "$kver was installed but could not be signed (see the SIGN WARNING above)"
        fi
    else
        log "Kernel signing not set up — $kver left unsigned"
    fi

    # ── DKMS: build out-of-tree modules before the initramfs ───────────
    # The headers are installed by now. Running this before the initramfs
    # step means modules that belong in the initramfs are included. A failure
    # here is a warning, never an install error.
    if ! dkms_postcheck "$kver"; then
        DKMS_FAILED=$((DKMS_FAILED + 1))
        log "DKMS WARNING: the DKMS check itself failed"
    fi

    # ── Initramfs: generate AND verify ─────────────────────────────────
    local irtool
    irtool="$(initramfs_tool)"
    log "Generating initramfs for $kver with $irtool…"
    generate_initramfs "$kver" || die "initramfs generation ($irtool) failed for $kver"

    [[ -f "$boot/initrd.img-$kver" ]] || die "initrd.img-$kver did NOT appear in $boot — DO NOT reboot into this kernel"
    log "Verified: $boot/initrd.img-$kver exists"

    # ── Boot loader: verify (systemd-boot) or update (GRUB) ─────────────
    local bl esp
    read -r bl esp <<< "$(detect_bootloader)"
    case "$bl" in
        kernelstub)
            # kernelstub's postinst hook (/etc/kernel/postinst.d/zz-kernelstub)
            # ran during dpkg. It copies only the current and previous kernels
            # to the ESP, so verify the copy that belongs to $kver, if any.
            log "kernelstub (Pop!_OS) detected (ESP: $esp) — verifying ESP kernel image…"
            local slot efi_img
            slot="$(kernelstub_slot_for_kver "$kver")"
            if [[ -z "$slot" ]]; then
                log "WARNING: kernelstub keeps only the current and previous kernel on the ESP, and $kver is neither: it is installed but will not be in the boot menu"
            else
                efi_img="$(esp_image_for_kver kernelstub "$esp" "$kver")"
                [[ -n "$efi_img" ]] || die "No EFI/Pop_OS-*/$slot under $esp after install — kernelstub did not sync. Try 'kernelstub -p' — DO NOT reboot expecting the new kernel"
                log "Verified: kernelstub image $efi_img exists"
                if (( signed_now )); then sync_signed_to_esp "$kver"; fi
            fi
            ;;
        systemd-boot)
            # kernel-install already ran automatically via the dpkg postinst
            # hook (/usr/lib/kernel/install.d/90-loaderentry.install) — verify
            # it actually produced an entry rather than trusting it happened,
            # same philosophy as the initramfs check above.
            log "systemd-boot detected (ESP: $esp) — verifying boot menu entry…"
            local entry
            entry="$(sdboot_entry_for_kver "$esp" "$kver")"
            [[ -n "$entry" ]] || die "No systemd-boot entry appeared for $kver in $esp/loader/entries — DO NOT reboot expecting it in the menu"
            log "Verified: boot menu entry $entry exists"
            if (( signed_now )); then sync_signed_to_esp "$kver"; fi
            ;;
        grub)
            log "Updating GRUB…"
            update-grub >>"$LOGFILE" 2>&1 || die "update-grub failed"

            # On GRUB_DEFAULT=saved systems (curtin/autoinstall images set
            # this up by default) the saved default stays pinned to whatever
            # was last selected — regenerating grub.cfg does NOT move it to
            # the new kernel. Confirmed against a real install: the new
            # kernel installs and verifies cleanly, update-grub lists it
            # correctly, and the machine still reboots into the OLD kernel,
            # silently — doubly so with GRUB_TIMEOUT_STYLE=hidden, where
            # there's no visible menu to notice the stale selection from.
            if grep -qE '^GRUB_DEFAULT=saved' "${KERNEL_POP_GRUB_DEFAULTS:-/etc/default/grub}" 2>/dev/null; then
                local prev_saved
                prev_saved="$(grub-editenv list 2>/dev/null | sed -n 's/^saved_entry=//p')" || prev_saved=""
                log "Previous GRUB default: ${prev_saved:-<none>}"
                # Remember the previous default so it can be put back if
                # the new one does not verify.
                log "GRUB_DEFAULT=saved detected — pointing the saved default at $kver…"
                local entry_path
                entry_path="$(grub_entry_path_for_kver "$kver")" || entry_path=""
                if [[ -n "$entry_path" ]]; then
                    if grub-set-default "$entry_path" >>"$LOGFILE" 2>&1; then
                        local saved
                        saved="$(grub-editenv list 2>/dev/null | sed -n 's/^saved_entry=//p')" || saved=""
                        if [[ "$saved" == "$entry_path" ]]; then
                            log "Verified: saved_entry now points to $kver"
                        else
                            log "WARNING: saved_entry is '$saved' after grub-set-default, expected '$entry_path' — verify manually before rebooting"
                            restore_grub_default "$prev_saved"
                        fi
                    else
                        log "WARNING: grub-set-default failed for $entry_path — $kver is installed but may not be the default boot entry"
                        restore_grub_default "$prev_saved"
                    fi
                else
                    log "WARNING: could not find a GRUB menu entry for $kver in grub.cfg — saved default left unchanged, verify manually before rebooting"
                fi
            fi
            ;;
        *)
            log "WARNING: could not detect a supported boot loader (GRUB or systemd-boot). The kernel and initramfs are installed and verified, but you must confirm $kver appears in your boot menu yourself before rebooting."
            ;;
    esac

    if (( DKMS_FAILED > 0 )); then
        log "DKMS SUMMARY: $DKMS_FAILED DKMS problem(s) for $kver — the kernel is installed, but out-of-tree modules (such as the NVIDIA driver) may be missing after reboot. See the DKMS lines above."
    fi

    if (( SIGN_FAILED > 0 )); then
        log "SIGN SUMMARY: $kver is installed but unsigned. Secure Boot is not enforcing now, so it boots; sign it before turning Secure Boot on."
    fi
    if (( SB_PENDING )); then
        log "BOOT PENDING: $kver is signed, but the signing key is still waiting for enrollment. On the next reboot the blue MOK Manager screen appears: choose Enroll MOK, Continue, Yes, then type your password. Skip it and $kver will not boot while Secure Boot is enforcing."
        log "==== Done. Kernel $kver installed with initramfs verified. Confirm the key enrollment at the next reboot. ===="
    else
        log "==== Done. Kernel $kver installed with initramfs verified. Reboot when ready. ===="
    fi
}

# Boot a different installed kernel on the NEXT boot only; the saved default
# is left alone. Does not reboot.
do_boot_once() {
    local kver="$1" boot
    boot="$(boot_dir)"
    [[ -n "$kver" ]] || die "No kernel version specified"
    valid_kver "$kver" || die "Invalid kernel version string: '$kver'"
    [[ -e "$boot/vmlinuz-$kver" ]] || die "Kernel $kver is not installed (no $boot/vmlinuz-$kver)"

    local bl esp
    read -r bl esp <<< "$(detect_bootloader)"
    set_boot_once "$bl" "$esp" "$kver" || die "Could not select $kver for the next boot"
}

# Installed (or config-files) packages that belong to exactly kernel <kver>,
# one per line: every linux-* package whose name ends in "-<kver>", plus the
# versioned headers base package (linux-headers-<kver without -generic>).
# Compared as plain strings, so neither another kernel whose version merely
# contains <kver> (17.1.3-… for 7.1.3-…) nor a glob in it can match.
packages_for_kver() {
    local kver="$1" base="${1%-generic}" st pkg
    while read -r st pkg; do
        [[ -n "$pkg" && "${st:1:1}" != "n" ]] || continue
        if [[ "$pkg" == linux-*"-$kver" || "$pkg" == "linux-headers-$base" ]]; then
            echo "$pkg"
        fi
    done < <(dpkg-query -W -f '${db:Status-Abbrev} ${Package}\n' 'linux-*' 2>/dev/null || true)
}

do_remove() {
    local kver="$1" boot mods
    boot="$(boot_dir)"
    mods="$(modules_dir)"
    [[ -n "$kver" ]] || die "No kernel version specified"
    valid_kver "$kver" || die "Invalid kernel version string: '$kver'"
    [[ -e "$boot/vmlinuz-$kver" || -d "$mods/$kver" ]] \
        || die "Kernel $kver is not installed (no $boot/vmlinuz-$kver or $mods/$kver)"

    local running
    running="$(uname -r)"
    [[ "$kver" == "$running" ]] && die "Refusing to remove the RUNNING kernel ($running)"

    log "==== Kernel removal started: $kver ===="

    local pkgs
    pkgs="$(packages_for_kver "$kver" | sort -u)"

    if [[ -n "$pkgs" ]]; then
        # Purging a stock kernel can drag out the linux-generic /
        # linux-image-generic metapackages (they depend on that exact
        # version), after which no future kernel update would ever install.
        # Simulate first and refuse rather than silently break updates.
        local sim meta
        # shellcheck disable=SC2086
        sim="$(apt-get -s purge $pkgs 2>/dev/null || true)"
        meta="$(printf '%s\n' "$sim" | grep '^Remv ' | cut -d' ' -f2 \
            | grep -E '^linux-(image-|headers-)?(generic|lowlatency|virtual|oem|kvm)(-[a-z0-9.-]+)?$' || true)"
        if [[ -n "$meta" ]]; then
            die "Removing $kver would also remove kernel metapackage(s): ${meta//$'\n'/ } — that would stop future kernel updates from installing. Install a newer stock kernel first, or remove manually with 'apt-mark hold' on them."
        fi

        log "Purging packages:"
        log "$pkgs"
        # shellcheck disable=SC2086
        apt-get purge -y $pkgs >>"$LOGFILE" 2>&1 || die "apt-get purge failed"
    else
        log "No packages own this kernel — removing files directly"
        rm -f "$boot/vmlinuz-$kver" "$boot/initrd.img-$kver" \
              "$boot/System.map-$kver" "$boot/config-$kver"
        rm -rf "${mods:?}/${kver:?}"
    fi

    # ── Boot loader: clean up (systemd-boot) or update (GRUB) ───────────
    # dpkg has no postrm hook that removes a systemd-boot entry (only
    # zz-update-grub exists by default) — without this, every removed
    # kernel on a systemd-boot machine leaves a permanently dangling,
    # unbootable menu entry behind.
    local bl esp
    read -r bl esp <<< "$(detect_bootloader)"
    case "$bl" in
        kernelstub)
            # The kernelstub postrm hook (/etc/kernel/postrm.d/zz-kernelstub)
            # resyncs the ESP when the packages are purged; kernel-install
            # remove does nothing useful here.
            log "kernelstub (Pop!_OS) detected — ESP entries are resynced by its postrm hook, not per-version"
            ;;
        systemd-boot)
            log "systemd-boot detected (ESP: $esp) — removing boot menu entry for $kver…"
            if command -v kernel-install >/dev/null 2>&1; then
                kernel-install remove "$kver" >>"$LOGFILE" 2>&1 \
                    || log "WARNING: kernel-install remove failed for $kver — its boot menu entry may be orphaned in $esp/loader/entries"
            else
                rm -f "$esp/loader/entries/"*"-${kver}.conf"
                if [[ -r /etc/machine-id ]]; then
                    rm -rf "${esp:?}/$(cat /etc/machine-id)/${kver:?}"
                fi
            fi
            ;;
        grub)
            log "Updating GRUB…"
            update-grub >>"$LOGFILE" 2>&1 || die "update-grub failed"
            ;;
        *)
            log "WARNING: could not detect a supported boot loader (GRUB or systemd-boot). Kernel $kver's files were removed, but its boot menu entry (if any) may still be present — check your boot menu by hand."
            ;;
    esac

    remove_signed_marker "$kver"
    log "==== Done. Kernel $kver removed. ===="
}

# ── Signing setup and on-demand signing ─────────────────────────────────
# Names of the apt packages that provide the signing tools not yet installed.
missing_signing_packages() {
    local pkgs=()
    if ! command -v sbsign >/dev/null 2>&1 || ! command -v sbverify >/dev/null 2>&1; then
        pkgs+=(sbsigntool)
    fi
    command -v openssl >/dev/null 2>&1 || pkgs+=(openssl)
    command -v mokutil >/dev/null 2>&1 || pkgs+=(mokutil)
    echo "${pkgs[*]}"
}

# ── Key enrollment ──────────────────────────────────────────────────────
# shim keeps ONE pending enrollment request (MokNew) with ONE password
# (MokAuth). `mokutil --import` adds to a waiting request but replaces its
# password, and skips a key that is already waiting without touching the
# password. So when another tool (another app, for the DKMS key) has
# queued a key with its own password, and this setup then queues ours, only
# the second password works at MOK Manager, and a mistyped first password
# enrolls nothing. Instead, every setup rebuilds the whole request: the
# certificates already waiting (read from MokNew), Kernel Pop's key and the
# DKMS module-signing key, under the password chosen now, and says so.
MOK_GUID="605dab50-e046-4300-abb6-3dd810dd8b23"
efi_dir()      { echo "${KERNEL_POP_EFI:-/sys/firmware/efi}"; }
efivars_dir()  { echo "${KERNEL_POP_EFIVARS:-/sys/firmware/efi/efivars}"; }
dkms_mok_der() { echo "${KERNEL_POP_DKMS_MOK:-/var/lib/shim-signed/mok/MOK.der}"; }
moknew_var()   { echo "$(efivars_dir)/MokNew-$MOK_GUID"; }
# The list of waiting keys the apps read (MokNew itself is readable by root
# only). It is shared: another app's signing setup rebuilds the same request
# and writes the same list. First line "moknew-size <bytes>", MokNew's size
# right after the import, so an app can tell when the request has changed
# since (enrolled at boot, revoked, replaced by other means) and ignore the
# list; then one SHA1 fingerprint (lowercase hex) per line, every key in the
# request.
mok_pending_list() { echo "${KERNEL_POP_MOK_PENDING:-/var/lib/mok-pending/list}"; }

# Write the shared list for the keys just imported (<sha1>...), atomically.
write_mok_pending_list() {
    local list dir size tmp
    list="$(mok_pending_list)"
    dir="$(dirname "$list")"
    if ! size="$(stat -c %s "$(moknew_var)" 2>/dev/null)"; then
        rm -f "$list"
        log "SIGN WARNING: mokutil reported success, but no enrollment request can be seen, so the keys may not be waiting. Run Set Up Signing again."
        return 1
    fi
    mkdir -p "$dir" && chmod 0755 "$dir" || return 1
    tmp="$(mktemp "$dir/.list.XXXXXX")" || return 1
    if { printf 'moknew-size %s\n' "$size"; printf '%s\n' "$@"; } > "$tmp" \
            && chmod 0644 "$tmp" && mv -f "$tmp" "$list"; then
        return 0
    fi
    rm -f "$tmp"
    return 1
}

# Before anything is decided about MokNew: on an EFI machine the EFI
# variables must be mounted, or MokNew looks absent and mokutil sees
# nothing (a chroot teardown can unmount them, for example). Mounted means
# another filesystem than the EFI firmware directory's (a mount that has been
# covered by another one is still listed in mountinfo, so that is not asked).
efivars_mounted() {
    local d e
    d="$(stat -c %d "$(efivars_dir)" 2>/dev/null)" || return 1
    e="$(stat -c %d "$(efi_dir)" 2>/dev/null)" || return 1
    [[ "$d" != "$e" ]]
}
ensure_efivarfs() {
    [[ -d "$(efi_dir)" ]] || return 0
    efivars_mounted && return 0
    log "The EFI variables are not mounted; mounting them on $(efivars_dir)"
    mount -t efivarfs -o nosuid,nodev,noexec efivarfs "$(efivars_dir)" >>"$LOGFILE" 2>&1 \
        || die "The EFI variables are not mounted and could not be mounted on $(efivars_dir), so Kernel Pop cannot see the waiting enrollment request or check the keys. Nothing was changed. Mount them (mount -t efivarfs efivarfs $(efivars_dir)) or reboot, then try again."
}

# Write the X.509 certificates of the waiting request (MokNew) to
# <dir>/pending-N.der. Returns 0 (nothing waiting, or all written), 3 when
# the request holds entries that are not certificates (hashes), which this
# cannot carry over, 2 when the variable cannot be read or is malformed.
pending_mok_certs() {
    local dir="$1" var
    var="$(moknew_var)"
    [[ -e "$var" ]] || return 0
    perl -e '
        use strict; use warnings;
        my ($file, $dir) = @ARGV;
        open(my $fh, "<:raw", $file) or exit 2;
        local $/; my $raw = <$fh>; close $fh;
        my $data = substr($raw, 4);   # efivarfs: 4 attribute bytes first
        my $x509 = pack("H*", "a159c0a5e494a74a87b5ab155c2bf072");
        my ($off, $n, $other) = (0, 0, 0);
        while ($off < length $data) {
            exit 2 if length($data) - $off < 28;
            my $type = substr($data, $off, 16);
            my ($lsize, $hsize, $ssize) = unpack("V3", substr($data, $off + 16, 12));
            exit 2 if $lsize < 28 || $off + $lsize > length $data || $ssize < 16;
            # The header and whole entries must fill the list exactly.
            exit 2 if 28 + $hsize > $lsize || ($lsize - 28 - $hsize) % $ssize != 0;
            my $e = $off + 28 + $hsize;
            while ($e + $ssize <= $off + $lsize) {
                if ($type eq $x509) {
                    $n++;
                    open(my $out, ">:raw", "$dir/pending-$n.der") or exit 2;
                    print {$out} substr($data, $e + 16, $ssize - 16);
                    close $out or exit 2;
                } else { $other++ }
                $e += $ssize;
            }
            $off += $lsize;
        }
        exit($other ? 3 : 0);
    ' "$var" "$dir"
}

# SHA1 fingerprint of a DER certificate, lowercase hex without colons.
cert_sha1() {
    openssl x509 -inform DER -in "$1" -noout -fingerprint -sha1 2>/dev/null \
        | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f'
}

cert_cn() {
    openssl x509 -inform DER -in "$1" -noout -subject -nameopt multiline 2>/dev/null \
        | sed -n 's/^ *commonName *= *//p' | head -1
}

# Already trusted without enrollment: in the MOK list, db, or built in.
cert_trusted() {
    mokutil --test-key "$1" 2>/dev/null | grep -qiE 'already enrolled|already in db|built-in'
}

# Queue every key that still needs enrolling as one request under <password>.
queue_key_enrollment() {
    local pw="$1" work hash f fp cn i n=0 rc=0
    local -a ders=() names=() fps=()
    ensure_efivarfs
    work="$(umask 077 && mktemp -d)" || die "Could not create a private temp directory"
    MOK_WORK_DIR="$work"
    trap 'rm -rf "$MOK_WORK_DIR"' EXIT
    mkdir "$work/pending"
    pending_mok_certs "$work/pending" || rc=$?
    case $rc in
        0) ;;
        3) die "Another enrollment request is waiting that holds more than certificates, which Kernel Pop cannot carry over. Finish it at the next reboot (or cancel it with 'mokutil --revoke-import'), then set up signing again." ;;
        *) die "The waiting enrollment request could not be read, so Kernel Pop will not replace it. Reboot to finish it first, then set up signing again." ;;
    esac
    add_cert() {   # <der> <why> <waiting|own>
        local der="$1" why="$2" kind="$3" fp c
        fp="$(cert_sha1 "$der")" || fp=""
        if [[ -z "$fp" ]]; then
            # Nothing has been changed yet: a waiting entry that cannot be
            # read would be lost by rebuilding the request, so leave it alone.
            [[ "$kind" == waiting ]] \
                && die "The waiting enrollment request holds an entry that is not a readable certificate, so Kernel Pop will not replace it. Reboot to finish it first (or cancel it with 'mokutil --revoke-import'), then set up signing again."
            die "Could not read the certificate $der ($why). Nothing was queued."
        fi
        for c in "${fps[@]}"; do [[ "$c" == "$fp" ]] && return 0; done
        cn="$(cert_cn "$der")"
        if cert_trusted "$der"; then
            log "Already enrolled: ${cn:-$der}"
            return 0
        fi
        n=$((n + 1))
        cp "$der" "$work/key-$n.der"
        ders+=("$work/key-$n.der"); fps+=("$fp"); names+=("${cn:-$der}")
        log "To enroll: ${cn:-$der} ($why)"
    }
    for f in "$work"/pending/*.der; do
        [[ -e "$f" ]] || continue
        add_cert "$f" "already waiting for enrollment" waiting
    done
    add_cert "$(mok_dir)/signing.der" "Kernel Pop's kernel-signing key" own
    if [[ -f "$(dkms_mok_der)" ]]; then
        add_cert "$(dkms_mok_der)" "the DKMS module-signing key, so modules such as the NVIDIA driver load with Secure Boot on" own
    fi

    if (( n == 0 )); then
        if [[ -e "$(moknew_var)" ]]; then
            mokutil --revoke-import >>"$LOGFILE" 2>&1 || true
        fi
        rm -f "$(mok_pending_list)"
        log "Every key is already enrolled — nothing to queue"
        return 0
    fi

    hash="$(printf '%s\n%s\n' "$pw" "$pw" | mokutil --generate-hash 2>/dev/null | mok_hash_from_output)"
    [[ -n "$hash" ]] || die "mokutil --generate-hash did not produce a password hash"
    (umask 077 && printf '%s\n' "$hash" > "$work/hash")
    hash=""
    # Replace the waiting request (its certificates are in the list above).
    if [[ -e "$(moknew_var)" ]]; then
        mokutil --revoke-import >>"$LOGFILE" 2>&1 \
            || die "Could not replace the waiting enrollment request (mokutil --revoke-import failed)"
    fi
    if ! mokutil --import "${ders[@]}" --hash-file "$work/hash" >>"$LOGFILE" 2>&1; then
        log "mokutil --import failed; trying once more"
        if ! mokutil --import "${ders[@]}" --hash-file "$work/hash" >>"$LOGFILE" 2>&1; then
            # The earlier request is gone. Drop whatever part of the new
            # one mokutil may have written, so that nothing at all waits and
            # no list claims otherwise.
            if [[ -e "$(moknew_var)" ]]; then mokutil --revoke-import >>"$LOGFILE" 2>&1 || true; fi
            rm -f "$(mok_pending_list)"
            log "SIGN WARNING: mokutil --import failed twice. None of these keys is waiting for enrollment now:"
            for i in "${!ders[@]}"; do
                log "  ${names[$i]} (SHA1 ${fps[$i]})"
            done
            die "The keys could not be queued for enrollment. Run Set Up Signing again to queue them all."
        fi
    fi
    write_mok_pending_list "${fps[@]}" || log "SIGN WARNING: could not record the queued keys for the apps"
    log "Enrollment queued for ${#ders[@]} key(s): $(IFS=';'; echo "${names[*]}" | sed 's/;/, /g')."
    log "On the next reboot a blue MOK Manager screen appears: choose Enroll MOK, Continue, Yes, then type the password you chose here. That one password enrolls all of these keys; a password chosen earlier, here or in another app, no longer works."
}

# One-time setup: tools, key, sign every installed kernel, queue enrollment.
# The enrollment password is the first line of stdin.
do_setup_signing() {
    local pw="" missing v signed=0 already=0 failed=0
    IFS= read -r pw || true
    valid_mok_password "$pw" || die "The enrollment password must be 8 to 16 printable ASCII characters without spaces"
    ensure_efivarfs

    log "==== Kernel signing setup started ===="
    missing="$(missing_signing_packages)"
    if [[ -n "$missing" ]]; then
        log "Installing signing tools: $missing"
        # shellcheck disable=SC2086
        apt-get install -y $missing </dev/null >>"$LOGFILE" 2>&1 || die "apt-get could not install: $missing"
    fi

    generate_signing_key || die "Could not create the signing key"

    while IFS= read -r v; do
        [[ -n "$v" ]] || continue
        if ! valid_kver "$v"; then
            log "Skipping oddly named kernel image: vmlinuz-$v"
            continue
        fi
        if kernel_signed "$v"; then
            write_signed_marker "$v" || log "SIGN WARNING: could not write the signed marker for $v"
            log "Already signed: $v"
            already=$((already + 1))
        elif sign_kernel "$v"; then
            sync_signed_to_esp "$v"
            signed=$((signed + 1))
        else
            log "$v could not be signed (see the SIGN WARNING above) — continuing with the rest"
            failed=$((failed + 1))
        fi
    done < <(installed_kvers)
    log "Kernels: $signed signed now, $already already signed, $failed failed"

    queue_key_enrollment "$pw"
    log "==== Kernel signing setup done ===="
}

do_sign() {
    local kver="$1" boot
    boot="$(boot_dir)"
    [[ -n "$kver" ]] || die "No kernel version specified"
    valid_kver "$kver" || die "Invalid kernel version string: '$kver'"
    [[ -f "$boot/vmlinuz-$kver" ]] || die "Kernel $kver is not installed (no $boot/vmlinuz-$kver)"
    signing_key_present || die "Kernel signing is not set up — run the signing setup first"
    sign_kernel "$kver" || die "Signing $kver failed — the kernel image was left unchanged"
    sync_signed_to_esp "$kver"
    log "==== Done. Kernel $kver is signed. ===="
}

# Remove several kernels under ONE authentication. Each removal runs as its
# own child process of this script, so a refusal (e.g. it would drag out the
# kernel metapackages) only skips that kernel. The running kernel and the
# newest installed kernel are never removed here, whatever was asked for.
do_remove_many() {
    (( $# > 0 )) || die "No kernel versions specified"
    local running newest v removed=0 skipped=0 failed=0
    running="$(uname -r)"
    newest="$(installed_kvers | newest_kver_in_list)"
    log "==== Bulk kernel removal started: $* ===="
    for v in "$@"; do
        if ! valid_kver "$v"; then
            log "Skipping invalid kernel version string: '$v'"
            failed=$((failed + 1))
        elif [[ "$v" == "$running" ]]; then
            log "Skipping $v: it is the running kernel"
            skipped=$((skipped + 1))
        elif [[ "$v" == "$newest" ]]; then
            log "Skipping $v: it is the newest installed kernel"
            skipped=$((skipped + 1))
        elif bash "${BASH_SOURCE[0]}" --remove "$v"; then
            removed=$((removed + 1))
        else
            log "Removal of $v failed — continuing with the rest"
            failed=$((failed + 1))
        fi
    done
    log "==== Bulk removal done: $removed removed, $skipped skipped, $failed failed ===="
    (( failed == 0 )) || exit 1
}

# ── Build dependencies ──────────────────────────────────────────────────
# The packages a kernel build needs (the app's BUILD_DEPS, kept equal by a
# test). Only these names are ever installed through this mode.
BUILD_DEPS_ALLOWED=(build-essential dpkg-dev bc bison flex debhelper kmod libdw-dev libelf-dev libssl-dev python3 rsync xz-utils pahole zstd cpio openssl gawk)

build_dep_allowed() {
    local p
    for p in "${BUILD_DEPS_ALLOWED[@]}"; do
        [[ "$1" == "$p" ]] && return 0
    done
    return 1
}

do_install_build_deps() {
    (( $# > 0 )) || die "No packages specified"
    local p
    for p in "$@"; do
        build_dep_allowed "$p" || die "Refusing to install '$p': not one of the kernel build tools"
    done
    log "==== Installing kernel build tools: $* ===="
    if ! DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" </dev/null >>"$LOGFILE" 2>&1; then
        log "apt-get install failed — refreshing the package index and trying once more…"
        apt-get update </dev/null >>"$LOGFILE" 2>&1 || log "apt-get update reported errors"
        DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" </dev/null >>"$LOGFILE" 2>&1 \
            || die "apt-get could not install: $*"
    fi
    log "==== Done. Build tools installed: $* ===="
}

# Run only when executed, not when sourced (scripts/tests/ sources this file
# to unit-test the helper functions).
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "$MODE" in
        --install)     do_install "$ARG" ;;
        --remove)      do_remove  "$ARG" ;;
        --remove-many) shift; do_remove_many "$@" ;;
        --boot-once)   do_boot_once "$ARG" ;;
        --setup-signing) do_setup_signing ;;
        --sign)        do_sign "$ARG" ;;
        --install-build-deps) shift; do_install_build_deps "$@" ;;
        *) die "Usage: $0 --install <build-dir> | --remove <kernel-version> | --remove-many <kernel-version>... | --boot-once <kernel-version> | --setup-signing (password on stdin) | --sign <kernel-version> | --install-build-deps <package>..." ;;
    esac
    exit 0
fi
