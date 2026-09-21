#!/usr/bin/env bash
# privileged-install.sh — runs as root via pkexec.
#
# THE WHOLE POINT OF THIS SCRIPT
# ------------------------------
# ukuu and hand-rolled scripts have repeatedly installed mainline kernels
# without generating the initramfs, because they parsed the kernel version
# out of a FILENAME with a regex and the mainline naming convention
# (7.1.3-070103-generic) broke the parse. One reboot later: VFS panic.
#
# This script never parses filenames. The kernel version is read from the
# .deb package metadata (dpkg-deb -f Package), cross-checked against the
# /lib/modules directory that actually appears after install, and the
# initramfs is VERIFIED to exist on disk before the script reports success.
# If the initramfs is missing, this script fails loudly instead of leaving
# an unbootable kernel behind.
#
# Usage:
#   privileged-install.sh --install <dir-of-debs>
#   privileged-install.sh --remove  <kernel-version-string>

set -euo pipefail

LOGFILE="${KERNELPOP_LOG:-/var/log/kernelpop.log}"
log() {
    local msg="[kernelpop] $*"
    echo "$msg"
    { echo "$(date '+%Y-%m-%d %H:%M:%S') $msg" >> "$LOGFILE"; } 2>/dev/null || true
}
die() { log "ERROR: $*"; exit 1; }

MODE="${1:-}"
ARG="${2:-}"

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
# be read out of the actual config, not constructed. Prints
# "<submenu-id>><entry-id>" for use with grub-set-default, or nothing if no
# match was found.
grub_entry_path_for_kver() {
    local kver="$1" cfg="/boot/grub/grub.cfg"
    [[ -f "$cfg" ]] || return 1

    local submenu_id entry_id
    submenu_id="$(grep -oP "submenu '[^']*' \\\$menuentry_id_option '\\K[^']+" "$cfg" | head -1)" || true
    # Excludes recovery-mode entries, which also contain $kver in their
    # title — the default must never land on one of those.
    entry_id="$(grep "menuentry '[^']*${kver}[^']*'" "$cfg" 2>/dev/null \
        | grep -v 'recovery mode' \
        | grep -oP "\\\$menuentry_id_option '\\K[^']+" | head -1)" || true
    [[ -n "$submenu_id" && -n "$entry_id" ]] || return 1
    echo "${submenu_id}>${entry_id}"
}

# ── Derive kernel version strings from .deb metadata ──────────────────
# The image/modules package NAME embeds the full version string:
#   linux-image-unsigned-7.1.3-070103-generic  ->  7.1.3-070103-generic
kver_from_debs() {
    local dir="$1"
    local deb pkg
    for deb in "$dir"/linux-image-*_amd64.deb "$dir"/linux-modules-*_amd64.deb; do
        [[ -f "$deb" ]] || continue
        pkg="$(dpkg-deb -f "$deb" Package 2>/dev/null)" || continue
        case "$pkg" in
            linux-image-unsigned-*) echo "${pkg#linux-image-unsigned-}"; return 0 ;;
            linux-image-*)          echo "${pkg#linux-image-}";          return 0 ;;
            linux-modules-*)        echo "${pkg#linux-modules-}";        return 0 ;;
        esac
    done
    return 1
}

# A kernel version string is spliced into globs and rm -rf paths, so it must
# be a plain token: no '*', '/', '..' or whitespace. Returns 0 if safe.
valid_kver() {
    [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] && [[ "$1" != *..* ]]
}

# Only kernel packages may be installed as root through this helper.
deb_allowed() {
    local pkg
    pkg="$(dpkg-deb -f "$1" Package 2>/dev/null)" || return 1
    case "$pkg" in
        linux-image*|linux-modules*|linux-headers*) return 0 ;;
        *) return 1 ;;
    esac
}

do_install() {
    local dir="$1"
    [[ -d "$dir" ]] || die "Not a directory: $dir"

    # Reject staging directories other users could modify while we run as root
    local mode owner
    mode="$(stat -c '%a' "$dir")"
    owner="$(stat -c '%u' "$dir")"
    if (( (8#$mode & 8#002) != 0 )); then
        die "Refusing world-writable package directory: $dir"
    fi
    log "Package directory owner uid=$owner mode=$mode"

    local all_debs=("$dir"/*.deb) debs=() d
    [[ -f "${all_debs[0]}" ]] || die "No .deb files found in $dir"
    for d in "${all_debs[@]}"; do
        deb_allowed "$d" || die "Refusing to install $d: Package field is not linux-image/linux-modules/linux-headers"
        debs+=("$d")
    done

    log "==== Kernel install started ===="
    log "Package directory: $dir"

    # Version from package metadata, before anything is installed
    local kver
    kver="$(kver_from_debs "$dir")" || die "Could not read a kernel version from the package metadata"
    log "Kernel version (from package metadata): $kver"

    # Snapshot /lib/modules so the post-install cross-check is honest
    local before_modules
    before_modules="$(ls -1 /lib/modules 2>/dev/null || true)"

    log "Installing ${#debs[@]} packages…"
    if ! dpkg -i "${debs[@]}" >>"$LOGFILE" 2>&1; then
        log "dpkg -i reported errors — attempting to fix dependencies…"
        apt-get install -f -y >>"$LOGFILE" 2>&1 || die "dpkg install failed and apt-get -f could not repair it"
    fi
    log "Packages installed"

    # Cross-check: the modules directory for $kver must now exist
    if [[ ! -d "/lib/modules/$kver" ]]; then
        log "WARNING: /lib/modules/$kver not found after install."
        local new_dir
        new_dir="$(comm -13 <(echo "$before_modules") <(ls -1 /lib/modules) | head -1 || true)"
        if [[ -n "$new_dir" ]]; then
            log "Using newly appeared modules directory instead: $new_dir"
            kver="$new_dir"
        else
            die "No modules directory appeared for the installed kernel — aborting before initramfs"
        fi
    fi

    # dpkg can fail while apt-get -f finds nothing to fix and still exit 0;
    # never declare success unless the kernel image itself is on disk.
    [[ -f "/boot/vmlinuz-$kver" ]] || die "/boot/vmlinuz-$kver did NOT appear after install — the kernel package did not install correctly"
    log "Verified: /boot/vmlinuz-$kver exists"

    # ── Initramfs: generate AND verify ─────────────────────────────────
    log "Generating initramfs for $kver…"
    if [[ -f "/boot/initrd.img-$kver" ]]; then
        update-initramfs -u -k "$kver" >>"$LOGFILE" 2>&1 || die "update-initramfs -u failed for $kver"
    else
        update-initramfs -c -k "$kver" >>"$LOGFILE" 2>&1 || die "update-initramfs -c failed for $kver"
    fi

    [[ -f "/boot/initrd.img-$kver" ]] || die "initrd.img-$kver did NOT appear in /boot — DO NOT reboot into this kernel"
    log "Verified: /boot/initrd.img-$kver exists"

    # ── Boot loader: verify (systemd-boot) or update (GRUB) ─────────────
    local bl esp
    read -r bl esp <<< "$(detect_bootloader)"
    case "$bl" in
        kernelstub)
            # kernelstub's postinst hook (/etc/kernel/postinst.d/zz-kernelstub)
            # ran during dpkg. Entries are Pop_OS-current/oldkern, not
            # per-version, so verify the kernel image it copies to the ESP.
            log "kernelstub (Pop!_OS) detected (ESP: $esp) — verifying ESP kernel image…"
            local efi_img
            efi_img="$(compgen -G "$esp/EFI/Pop_OS-*/vmlinuz.efi" | head -1 || true)"
            [[ -n "$efi_img" ]] || die "No EFI/Pop_OS-*/vmlinuz.efi under $esp after install — kernelstub did not sync. Try 'kernelstub -p' — DO NOT reboot expecting the new kernel"
            log "Verified: kernelstub image $efi_img exists"
            ;;
        systemd-boot)
            # kernel-install already ran automatically via the dpkg postinst
            # hook (/usr/lib/kernel/install.d/90-loaderentry.install) — verify
            # it actually produced an entry rather than trusting it happened,
            # same philosophy as the initramfs check above.
            log "systemd-boot detected (ESP: $esp) — verifying boot menu entry…"
            local entry
            entry="$(find "$esp/loader/entries" -name "*-${kver}.conf" 2>/dev/null | head -1)" || true
            [[ -n "$entry" ]] || die "No systemd-boot entry appeared for $kver in $esp/loader/entries — DO NOT reboot expecting it in the menu"
            log "Verified: boot menu entry $entry exists"
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
            if grep -qE '^GRUB_DEFAULT=saved' /etc/default/grub 2>/dev/null; then
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
                        fi
                    else
                        log "WARNING: grub-set-default failed for $entry_path — $kver is installed but may not be the default boot entry"
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

    log "==== Done. Kernel $kver installed with initramfs verified. Reboot when ready. ===="
}

do_remove() {
    local kver="$1"
    [[ -n "$kver" ]] || die "No kernel version specified"
    valid_kver "$kver" || die "Invalid kernel version string: '$kver'"
    [[ -e "/boot/vmlinuz-$kver" || -d "/lib/modules/$kver" ]] \
        || die "Kernel $kver is not installed (no /boot/vmlinuz-$kver or /lib/modules/$kver)"

    local running
    running="$(uname -r)"
    [[ "$kver" == "$running" ]] && die "Refusing to remove the RUNNING kernel ($running)"

    log "==== Kernel removal started: $kver ===="

    # Every package whose name embeds this exact version string
    local pkgs
    pkgs="$(dpkg-query -W -f '${Package}\n' "linux-*${kver}*" 2>/dev/null | sort -u || true)"
    # Also catch the versioned headers base package (no -generic suffix)
    local base="${kver%-generic}"
    local base_pkgs
    base_pkgs="$(dpkg-query -W -f '${Package}\n' "linux-headers-${base}" 2>/dev/null || true)"
    pkgs="$(printf '%s\n%s\n' "$pkgs" "$base_pkgs" | sort -u | sed '/^$/d')"

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
        rm -f "/boot/vmlinuz-$kver" "/boot/initrd.img-$kver" \
              "/boot/System.map-$kver" "/boot/config-$kver"
        rm -rf "/lib/modules/$kver"
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

    log "==== Done. Kernel $kver removed. ===="
}

# Run only when executed, not when sourced (scripts/tests/ sources this file
# to unit-test the helper functions).
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "$MODE" in
        --install) do_install "$ARG" ;;
        --remove)  do_remove  "$ARG" ;;
        *) die "Usage: $0 --install <dir-of-debs> | --remove <kernel-version>" ;;
    esac
    exit 0
fi
