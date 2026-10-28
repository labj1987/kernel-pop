#!/usr/bin/env bash
# Unit tests for the pure helpers in scripts/privileged-install.sh.
# Sources the script (its main dispatch is guarded) and stubs dpkg-deb.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export KERNEL_POP_LOG=/dev/null
# shellcheck source=../privileged-install.sh
source "$HERE/../privileged-install.sh"
set +e

fails=0
ok()  { :; }
bad() { echo "FAIL: $*"; fails=$((fails + 1)); }
# CI sets KERNEL_POP_REQUIRE_ALL_TESTS=1: there a skipped group is a failure,
# so a missing tool can never let the signing or verification paths go
# untested.
skip() {
    if [[ -n "${KERNEL_POP_REQUIRE_ALL_TESTS:-}" ]]; then
        bad "skipped although KERNEL_POP_REQUIRE_ALL_TESTS is set: $*"
    else
        echo "SKIP: $*"
    fi
}

for v in 7.1.3-070103-generic 6.8.0-45-generic 6.10.0+rc1 a.b_c; do
    valid_kver "$v" || bad "valid_kver should accept [$v]"
done
for v in '*' '../..' '../../etc' 'a/b' '' ' ' '-x' '.x' '6.8*' 'a b' '..' 'a..b' $'a\nb' '$(id)' ';rm' '?' '[a]'; do
    valid_kver "$v" && bad "valid_kver should reject [$v]"
done

# dpkg-deb stub: "<file>" -> Package name is the file's basename up to '_'
dpkg-deb() { local b; b="$(basename "$2")"; echo "${b%%_*}"; }
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
# No EFI firmware directory unless a test sets one, so nothing ever mounts
# the real machine's EFI variables; the shared list of waiting keys in $tmp.
export KERNEL_POP_EFI="$tmp/no-efi" KERNEL_POP_MOK_PENDING="$tmp/mok-pending/list"
# No real dpkg locks either: an empty /proc/locks and no lock files.
mkdir -p "$tmp/proc" "$tmp/dpkg"; : > "$tmp/proc/locks"
export KERNEL_POP_PROC="$tmp/proc" KERNEL_POP_DPKG_ADMIN="$tmp/dpkg"
APT_OPTS="-o DPkg::Lock::Timeout=300"

deb_allowed "$tmp/linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb"   || bad "deb_allowed image"
deb_allowed "$tmp/linux-headers-7.2.12-kernelpop_7.2.12-1_amd64.deb" || bad "deb_allowed headers"
deb_allowed "$tmp/linux-libc-dev_7.2.12-1_amd64.deb"      && bad "deb_allowed must refuse linux-libc-dev (it would replace Ubuntu's)"
deb_allowed "$tmp/linux-image-7.2.12-kernelpop-dbg_7.2.12-1_amd64.deb" && bad "deb_allowed must refuse a -dbg package"
deb_allowed "$tmp/linux-modules-7.2.12-generic_1_amd64.deb" && bad "deb_allowed should refuse linux-modules"
deb_allowed "$tmp/linux-image-_1_amd64.deb" && bad "deb_allowed should refuse an empty kernel name"
deb_allowed "$tmp/evil-pkg_1_amd64.deb"  && bad "deb_allowed should reject evil-pkg"
deb_allowed "$tmp/bash_1_amd64.deb"      && bad "deb_allowed should reject bash"

mkdir -p "$tmp/kv"
touch "$tmp/kv/linux-headers-7.2.12-kernelpop_7.2.12-1_amd64.deb" \
      "$tmp/kv/linux-image-7.2.12-kernelpop_7.2.12-1_amd64.deb" \
      "$tmp/kv/linux-image-7.2.12-kernelpop-dbg_7.2.12-1_amd64.deb" \
      "$tmp/kv/linux-libc-dev_7.2.12-1_amd64.deb"
got="$(kver_from_debs "$tmp/kv")"
[[ "$got" == "7.2.12-kernelpop" ]] || bad "kver_from_debs got [$got]"
touch "$tmp/kv/linux-image-7.2.11-kernelpop_7.2.11-1_amd64.deb"
kver_from_debs "$tmp/kv" >/dev/null && bad "kver_from_debs should fail with two kernel images"
rm -rf "$tmp/kv"

empty="$(mktemp -d)"
kver_from_debs "$empty" >/dev/null && bad "kver_from_debs should fail on empty dir"
rmdir "$empty"

# ── DKMS status parsing (dkms 3 and dkms 2 formats) ─────────────────────
l3="nvidia/550.54, 6.8.0-45-generic, x86_64: installed"
l2="nvidia, 550.54, 6.8.0-45-generic, x86_64: built"
lw="zfs/2.2.2, 7.1.3-070103-generic, x86_64: installed (WARNING! Diff between built and installed module!)"
[[ "$(dkms_line_state "$l3")" == "installed" ]] || bad "dkms_line_state dkms3"
[[ "$(dkms_line_state "$l2")" == "built" ]]     || bad "dkms_line_state dkms2"
[[ "$(dkms_line_state "$lw")" == "installed" ]] || bad "dkms_line_state warning suffix"
[[ "$(dkms_line_module "$l3")" == "nvidia/550.54" ]] || bad "dkms_line_module dkms3 got [$(dkms_line_module "$l3")]"
[[ "$(dkms_line_module "$l2")" == "nvidia/550.54" ]] || bad "dkms_line_module dkms2 got [$(dkms_line_module "$l2")]"
[[ "$(dkms_line_module "$lw")" == "zfs/2.2.2" ]]     || bad "dkms_line_module warning suffix"

mkdir -p "$tmp/dkms/nvidia/550.54/6.8.0-45-generic/x86_64/log"
echo "make failed" > "$tmp/dkms/nvidia/550.54/6.8.0-45-generic/x86_64/log/make.log"
got="$(KERNEL_POP_DKMS_TREE="$tmp/dkms" dkms_make_log nvidia/550.54 6.8.0-45-generic)"
[[ "$got" == "$tmp/dkms/nvidia/550.54/6.8.0-45-generic/x86_64/log/make.log" ]] || bad "dkms_make_log got [$got]"
got="$(KERNEL_POP_DKMS_TREE="$tmp/dkms" dkms_make_log nvidia/550.54 9.9.9-generic)"
[[ -z "$got" ]] || bad "dkms_make_log should be empty for an unknown kernel, got [$got]"

# dkms_postcheck must never fail and must flag a non-installed module
dkms() {
    if [[ "$1" == "status" ]]; then echo "$DKMS_TEST_STATUS"; return 0; fi
    return 1   # autoinstall fails
}
export DKMS_TEST_STATUS="$l2"
export KERNEL_POP_MODULES="$tmp/modules" KERNEL_POP_DKMS_TREE="$tmp/dkms"
# No build dir for this kernel: counted as a warning, still returns 0
DKMS_FAILED=0
dkms_postcheck "6.8.0-45-generic" >/dev/null || bad "dkms_postcheck must return 0 when headers are missing"
(( DKMS_FAILED == 1 )) || bad "dkms_postcheck should count a missing build dir, got $DKMS_FAILED"
# Build dir present, module only 'built', autoinstall failing: one failure, log tail shown
mkdir -p "$tmp/modules/6.8.0-45-generic/build"
DKMS_FAILED=0
out="$(dkms_postcheck "6.8.0-45-generic")" || bad "dkms_postcheck must return 0 on a failed module"
echo "$out" | grep -q "DKMS WARNING: nvidia/550.54 is 'built'" || bad "dkms_postcheck should warn about the module"
echo "$out" | grep -q "make failed" || bad "dkms_postcheck should show the make.log tail"
# Same, in the current shell, to check the counter
dkms_postcheck "6.8.0-45-generic" >/dev/null
(( DKMS_FAILED == 1 )) || bad "dkms_postcheck failure count should be 1, got $DKMS_FAILED"
# Healthy module: no failures
export DKMS_TEST_STATUS="$l3"
DKMS_FAILED=0
dkms_postcheck "6.8.0-45-generic" >/dev/null
(( DKMS_FAILED == 0 )) || bad "dkms_postcheck should report no failures for an installed module, got $DKMS_FAILED"
unset -f dkms
unset KERNEL_POP_MODULES KERNEL_POP_DKMS_TREE DKMS_TEST_STATUS

# ── Installed kernels / newest ───────────────────────────────────────────
mkdir -p "$tmp/boot"
touch "$tmp/boot/vmlinuz-6.8.0-45-generic" "$tmp/boot/vmlinuz-7.1.3-070103-generic" \
      "$tmp/boot/vmlinuz-6.10.0-9-generic" "$tmp/boot/initrd.img-7.1.3-070103-generic"
got="$(KERNEL_POP_BOOT="$tmp/boot" installed_kvers | sort | tr '\n' ' ')"
[[ "$got" == "6.10.0-9-generic 6.8.0-45-generic 7.1.3-070103-generic " ]] || bad "installed_kvers got [$got]"
got="$(KERNEL_POP_BOOT="$tmp/boot" installed_kvers | newest_kver_in_list)"
[[ "$got" == "7.1.3-070103-generic" ]] || bad "newest_kver_in_list got [$got]"
got="$(printf '6.9.0-1-generic\n6.10.0-1-generic\n' | newest_kver_in_list)"
[[ "$got" == "6.10.0-1-generic" ]] || bad "newest_kver_in_list should sort numerically, got [$got]"
mkdir -p "$tmp/emptyboot"
[[ -z "$(KERNEL_POP_BOOT="$tmp/emptyboot" installed_kvers)" ]] || bad "installed_kvers on an empty /boot"

# Bulk removal never touches the newest or the running kernel, and rejects bad names
out="$( (KERNEL_POP_BOOT="$tmp/boot" do_remove_many 7.1.3-070103-generic "$(uname -r)") 2>&1 )" \
    || bad "do_remove_many skipping newest/running should succeed"
echo "$out" | grep -q "newest installed kernel" || bad "do_remove_many should skip the newest kernel"
echo "$out" | grep -q "running kernel" || bad "do_remove_many should skip the running kernel"
(KERNEL_POP_BOOT="$tmp/boot" do_remove_many '../..' >/dev/null 2>&1) && bad "do_remove_many should fail on an invalid version"
(do_remove_many >/dev/null 2>&1) && bad "do_remove_many should fail with no arguments"

# ── Initramfs tool detection (PATH stubs) ────────────────────────────────
mkdir -p "$tmp/bin-both" "$tmp/bin-dracut" "$tmp/bin-none"
for b in update-initramfs dracut; do printf '#!/bin/sh\n' > "$tmp/bin-both/$b"; chmod +x "$tmp/bin-both/$b"; done
printf '#!/bin/sh\n' > "$tmp/bin-dracut/dracut"; chmod +x "$tmp/bin-dracut/dracut"
got="$(PATH="$tmp/bin-both" initramfs_tool)";   [[ "$got" == "update-initramfs" ]] || bad "initramfs_tool both got [$got]"
got="$(PATH="$tmp/bin-dracut" initramfs_tool)"; [[ "$got" == "dracut" ]]           || bad "initramfs_tool dracut got [$got]"
got="$(PATH="$tmp/bin-none" initramfs_tool)";   [[ "$got" == "none" ]]             || bad "initramfs_tool none got [$got]"
PATH="$tmp/bin-none" generate_initramfs 0.0.0-fake >/dev/null 2>&1 && bad "generate_initramfs should fail with no tool"

# ── GRUB entry lookup from fixture grub.cfg files ───────────────────────
cat > "$tmp/grub.cfg" <<'EOF2'
menuentry 'Ubuntu' --class ubuntu $menuentry_id_option 'gnulinux-simple-abc' {
	linux /boot/vmlinuz-7.1.3-070103-generic root=UUID=abc ro
}
submenu 'Advanced options for Ubuntu' $menuentry_id_option 'gnulinux-advanced-abc' {
	menuentry 'Ubuntu, with Linux 17.1.3-070103-generic' --class ubuntu $menuentry_id_option 'gnulinux-17.1.3-070103-generic-advanced-abc' {
	}
	menuentry 'Ubuntu, with Linux 7.1.3-070103-generic' --class ubuntu $menuentry_id_option 'gnulinux-7.1.3-070103-generic-advanced-abc' {
	}
	menuentry 'Ubuntu, with Linux 7.1.3-070103-generic (recovery mode)' --class ubuntu $menuentry_id_option 'gnulinux-7.1.3-070103-generic-recovery-abc' {
	}
	menuentry 'Ubuntu, with Linux 6.8.0-45-generic' --class ubuntu $menuentry_id_option 'gnulinux-6.8.0-45-generic-advanced-abc' {
	}
}
menuentry 'UEFI Firmware Settings' $menuentry_id_option 'uefi-firmware' {
	fwsetup
}
submenu 'Advanced options for Ubuntu 24.04 (on /dev/sdb2)' $menuentry_id_option 'osprober-gnulinux-advanced-def' {
	menuentry 'Ubuntu, with Linux 9.9.9-generic (on /dev/sdb2)' --class gnu-linux $menuentry_id_option 'osprober-gnulinux-/boot/vmlinuz-9.9.9-generic--def' {
	}
}
EOF2
got="$(KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 7.1.3-070103-generic)"
[[ "$got" == "gnulinux-advanced-abc>gnulinux-7.1.3-070103-generic-advanced-abc" ]] || bad "grub_entry_path_for_kver got [$got]"
got="$(KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 6.8.0-45-generic)"
[[ "$got" == "gnulinux-advanced-abc>gnulinux-6.8.0-45-generic-advanced-abc" ]] || bad "grub_entry_path_for_kver last entry got [$got]"
KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 9.9.9-generic >/dev/null && bad "grub_entry_path_for_kver should fail for an unknown kernel (an os-prober entry is not ours)"
# '.' is not a regex wildcard, and a longer version that contains kver is not kver
KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 7x1x3-070103-generic >/dev/null && bad "grub_entry_path_for_kver treated '.' as a wildcard"
KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 1.3-070103-generic >/dev/null && bad "grub_entry_path_for_kver matched a version that only contains the asked one"
KERNEL_POP_GRUB_CFG="$tmp/missing.cfg" grub_entry_path_for_kver 7.1.3-070103-generic >/dev/null && bad "grub_entry_path_for_kver should fail without grub.cfg"
# GRUB_DISABLE_SUBMENU=y: every kernel is a top-level entry, path is the bare id
cat > "$tmp/grub-flat.cfg" <<'EOF2'
menuentry 'Ubuntu, with Linux 7.1.3-070103-generic' --class ubuntu $menuentry_id_option 'gnulinux-7.1.3-070103-generic-advanced-abc' {
	linux /boot/vmlinuz-7.1.3-070103-generic root=UUID=abc ro
}
menuentry 'Ubuntu, with Linux 7.1.3-070103-generic (recovery mode)' --class ubuntu $menuentry_id_option 'gnulinux-7.1.3-070103-generic-recovery-abc' {
}
menuentry 'Ubuntu, with Linux 6.8.0-45-generic' --class ubuntu $menuentry_id_option 'gnulinux-6.8.0-45-generic-advanced-abc' {
}
EOF2
got="$(KERNEL_POP_GRUB_CFG="$tmp/grub-flat.cfg" grub_entry_path_for_kver 6.8.0-45-generic)"
[[ "$got" == "gnulinux-6.8.0-45-generic-advanced-abc" ]] || bad "grub_entry_path_for_kver flat layout got [$got]"
got="$(KERNEL_POP_GRUB_CFG="$tmp/grub-flat.cfg" grub_entry_path_for_kver 7.1.3-070103-generic)"
[[ "$got" == "gnulinux-7.1.3-070103-generic-advanced-abc" ]] || bad "grub_entry_path_for_kver flat layout first entry got [$got]"

# restore_grub_default with no previous default is a no-op that succeeds
restore_grub_default "" || bad "restore_grub_default empty should succeed"

# ── Exact package matching for removal ──────────────────────────────────
dpkg-query() {
    printf '%s\n' "ii  linux-image-unsigned-7.1.3-070103-generic" "ii  linux-modules-7.1.3-070103-generic" \
        "ii  linux-headers-7.1.3-070103-generic" "ii  linux-headers-7.1.3-070103" \
        "ii  linux-image-unsigned-17.1.3-070103-generic" "ii  linux-modules-7.1.3-070103-generic-64k" \
        "un  linux-modules-extra-7.1.3-070103-generic" "rc  linux-modules-nvidia-550-7.1.3-070103-generic" \
        "ii  linux-firmware" "ii  linux-headers-7.1.3-0701033"
}
got="$(packages_for_kver 7.1.3-070103-generic | sort | tr '\n' ' ')"
[[ "$got" == "linux-headers-7.1.3-070103 linux-headers-7.1.3-070103-generic linux-image-unsigned-7.1.3-070103-generic linux-modules-7.1.3-070103-generic linux-modules-nvidia-550-7.1.3-070103-generic " ]] \
    || bad "packages_for_kver got [$got]"
unset -f dpkg-query

# ── systemd-boot entries and kernelstub slots ────────────────────────────
mkdir -p "$tmp/sd/loader/entries"
printf 'title Ubuntu\nversion 7.1.3-070103-generic\nlinux /mid/7.1.3-070103-generic/linux\n' > "$tmp/sd/loader/entries/mid-7.1.3-070103-generic.conf"
printf 'title Ubuntu\nversion 17.1.3-070103-generic\nlinux /mid/17.1.3-070103-generic/linux\n' > "$tmp/sd/loader/entries/mid-17.1.3-070103-generic.conf"
printf 'title Ubuntu\nlinux /mid/6.8.0-45-generic/linux\n' > "$tmp/sd/loader/entries/mid-6.8.0-45-generic+3-0.conf"
got="$(sdboot_entry_for_kver "$tmp/sd" 7.1.3-070103-generic)"
[[ "$got" == "$tmp/sd/loader/entries/mid-7.1.3-070103-generic.conf" ]] || bad "sdboot_entry_for_kver by version line got [$got]"
[[ "$(sdboot_entry_id "$tmp/sd/loader/entries/mid-6.8.0-45-generic+3-0.conf")" == "mid-6.8.0-45-generic.conf" ]] || bad "sdboot_entry_id should strip the boot counter"
[[ "$(sdboot_entry_id mid-6.8.0-45-generic+3.conf)" == "mid-6.8.0-45-generic.conf" ]] || bad "sdboot_entry_id should strip a +N counter"
[[ "$(sdboot_entry_id mid-7.1.3-generic.conf)" == "mid-7.1.3-generic.conf" ]] || bad "sdboot_entry_id should keep a plain name"
(
    bootctl() { echo "$*" > "$tmp/bootctl-args"; }
    set_boot_once systemd-boot "$tmp/sd" 6.8.0-45-generic >/dev/null
) || bad "set_boot_once systemd-boot failed"
[[ "$(cat "$tmp/bootctl-args")" == "set-oneshot mid-6.8.0-45-generic.conf" ]] || bad "set-oneshot should get the entry ID without the counter, got $(cat "$tmp/bootctl-args")"
got="$(sdboot_entry_for_kver "$tmp/sd" 6.8.0-45-generic)"
[[ "$got" == "$tmp/sd/loader/entries/mid-6.8.0-45-generic+3-0.conf" ]] || bad "sdboot_entry_for_kver boot-counted file name got [$got]"
[[ -z "$(sdboot_entry_for_kver "$tmp/sd" 1.3-070103-generic)" ]] || bad "sdboot_entry_for_kver matched a partial version"
got="$(esp_image_for_kver systemd-boot "$tmp/sd" 7.1.3-070103-generic)"
[[ "$got" == "$tmp/sd/mid/7.1.3-070103-generic/linux" ]] || bad "esp_image_for_kver systemd-boot got [$got]"
[[ -z "$(esp_image_for_kver grub "$tmp/sd" 7.1.3-070103-generic)" ]] || bad "esp_image_for_kver grub should be empty"

mkdir -p "$tmp/ksboot" "$tmp/ksroot" "$tmp/ksesp/EFI/Pop_OS-abcd"
touch "$tmp/ksesp/EFI/Pop_OS-abcd/vmlinuz.efi" "$tmp/ksesp/EFI/Pop_OS-abcd/vmlinuz-previous.efi"
ln -s vmlinuz-7.1.3-070103-generic "$tmp/ksboot/vmlinuz"
ln -s vmlinuz-6.8.0-45-generic "$tmp/ksboot/vmlinuz.old"
export KERNEL_POP_BOOT="$tmp/ksboot" KERNEL_POP_ROOTFS="$tmp/ksroot"
[[ "$(kernelstub_slot_for_kver 7.1.3-070103-generic)" == "vmlinuz.efi" ]] || bad "kernelstub slot current (/boot links)"
[[ "$(kernelstub_slot_for_kver 6.8.0-45-generic)" == "vmlinuz-previous.efi" ]] || bad "kernelstub slot previous (/boot links)"
[[ -z "$(kernelstub_slot_for_kver 6.5.0-1-generic)" ]] || bad "kernelstub slot for a kernel with no ESP copy"
[[ "$(esp_image_for_kver kernelstub "$tmp/ksesp" 6.8.0-45-generic)" == "$tmp/ksesp/EFI/Pop_OS-abcd/vmlinuz-previous.efi" ]] || bad "esp_image_for_kver kernelstub previous"
[[ -z "$(esp_image_for_kver kernelstub "$tmp/ksesp" 6.5.0-1-generic)" ]] || bad "esp_image_for_kver kernelstub with no slot"
# Links in / win over /boot
ln -s boot/vmlinuz-6.5.0-1-generic "$tmp/ksroot/vmlinuz"
[[ "$(kernelstub_slot_for_kver 6.5.0-1-generic)" == "vmlinuz.efi" ]] || bad "kernelstub slot should follow the / links"
[[ -z "$(kernelstub_slot_for_kver 7.1.3-070103-generic)" ]] || bad "kernelstub slot should ignore /boot links when / has them"
unset KERNEL_POP_BOOT KERNEL_POP_ROOTFS

# ── EFI variables mounted before anything about MokNew is decided ───────
(
    r=0
    mount() { echo "mount $*" >> "$tmp/mount-calls"; return "${MOUNT_RC:-0}"; }
    : > "$tmp/mount-calls"
    ensure_efivarfs || r=1                                             # no EFI: nothing to do
    [[ -s "$tmp/mount-calls" ]] && { echo "FAIL: mounted without EFI firmware"; r=1; }
    export KERNEL_POP_EFI="$tmp/efi" KERNEL_POP_EFIVARS="$tmp/efi/efivars"
    mkdir -p "$KERNEL_POP_EFIVARS"
    ensure_efivarfs >/dev/null || r=1                                  # not a mountpoint: mounted
    [[ "$(cat "$tmp/mount-calls")" == "mount -t efivarfs -o nosuid,nodev,noexec efivarfs $tmp/efi/efivars" ]] \
        || { echo "FAIL: efivarfs mount call: $(cat "$tmp/mount-calls")"; r=1; }
    : > "$tmp/mount-calls"
    out="$(MOUNT_RC=32 ensure_efivarfs 2>&1)" && { echo "FAIL: a failed mount must stop the run"; r=1; }
    grep -q "EFI variables are not mounted and could not be mounted" <<< "$out" || { echo "FAIL: failed mount message: $out"; r=1; }
    : > "$tmp/mount-calls"
    KERNEL_POP_EFIVARS=/proc ensure_efivarfs || r=1                     # another filesystem: mounted
    [[ -s "$tmp/mount-calls" ]] && { echo "FAIL: mounted over a mountpoint"; r=1; }
    # The signing setup and the install preflight both check first.
    out="$(printf 'Secret-pw1\n' | MOUNT_RC=32 do_setup_signing 2>&1)" && { echo "FAIL: setup went on without EFI variables"; r=1; }
    grep -q "could not be mounted" <<< "$out" || { echo "FAIL: setup without EFI variables: $out"; r=1; }
    grep -q "setup started" <<< "$out" && { echo "FAIL: setup started without EFI variables"; r=1; }
    out="$(MOUNT_RC=32 secure_boot_preflight 2>&1)" && { echo "FAIL: install preflight went on without EFI variables"; r=1; }
    rm -rf "$tmp/efi"
    exit "$r"
) || bad "efivarfs mount checks"

# ── Enrollment state and the Secure Boot preflight ───────────────────────
for pair in "is already enrolled:enrolled" "is already in the enrollment request:pending" \
            "is not enrolled:not-enrolled" "EFI variables are not supported:unknown"; do
    MOKUTIL_TEXT="/x/signing.der ${pair%%:*}"
    mokutil() { echo "$MOKUTIL_TEXT"; }
    got="$(key_enrollment_state)"
    [[ "$got" == "${pair##*:}" ]] || bad "key_enrollment_state for [${pair%%:*}] got [$got]"
done
unset -f mokutil
(
    export KERNEL_POP_MOK_DIR="$tmp/pf-mok"
    mkdir -p "$KERNEL_POP_MOK_DIR"
    sbsign() { :; }; sbverify() { :; }
    secure_boot_enforcing() { return 1; }
    (secure_boot_preflight) >/dev/null || exit 1                     # Secure Boot off: anything goes
    secure_boot_enforcing() { return 0; }
    out="$(secure_boot_preflight)" && exit 2                          # no key: refused
    grep -q "not set up.*Nothing was installed" <<< "$out" || exit 3
    touch "$KERNEL_POP_MOK_DIR/signing.key" "$KERNEL_POP_MOK_DIR/signing.pem" "$KERNEL_POP_MOK_DIR/signing.der"
    key_enrollment_state() { echo enrolled; }
    secure_boot_preflight >/dev/null || exit 4
    (( SB_PENDING == 0 )) || exit 5
    key_enrollment_state() { echo pending; }
    secure_boot_preflight >/dev/null || exit 6
    (( SB_PENDING == 1 )) || exit 7
    key_enrollment_state() { echo not-enrolled; }
    (secure_boot_preflight >/dev/null) && exit 8
    key_enrollment_state() { echo unknown; }
    (secure_boot_preflight >/dev/null) && exit 9
    exit 0
) || bad "secure_boot_preflight case $?"

# ── Build verification (root side) ──────────────────────────────────────
cat > "$tmp/SHA256SUMS" <<EOF2
$(printf 'A%.0s' {1..64})  linux-a_1_amd64.deb
$(printf 'b%.0s' {1..64})  linux-b_1_amd64.deb
$(printf 'c%.0s' {1..64})  sub/linux-c_1_amd64.deb
EOF2
[[ "$(manifest_hash_for "$tmp/SHA256SUMS" linux-a_1_amd64.deb)" == "$(printf 'a%.0s' {1..64})" ]] || bad "manifest_hash_for (lowercased)"
[[ "$(manifest_hash_for "$tmp/SHA256SUMS" linux-b_1_amd64.deb)" == "$(printf 'b%.0s' {1..64})" ]] || bad "manifest_hash_for second entry"
[[ -z "$(manifest_hash_for "$tmp/SHA256SUMS" linux-c_1_amd64.deb)" ]] || bad "manifest_hash_for must compare the whole name, not a path suffix"
[[ -z "$(manifest_hash_for "$tmp/SHA256SUMS" linux-x_1_amd64.deb)" ]] || bad "manifest_hash_for an unlisted file"

PKEXEC_UID="$(id -u)"; export PKEXEC_UID
mkdir -p "$tmp/trust"; chmod 0755 "$tmp/trust"
echo x > "$tmp/trust/f"; chmod 0644 "$tmp/trust/f"
path_trusted "$tmp/trust/f" >/dev/null || bad "path_trusted should accept an own 0644 file"
chmod 0664 "$tmp/trust/f"; path_trusted "$tmp/trust/f" >/dev/null && bad "path_trusted should refuse a group-writable file"
chmod 0646 "$tmp/trust/f"; path_trusted "$tmp/trust/f" >/dev/null && bad "path_trusted should refuse a world-writable file"
chmod 0644 "$tmp/trust/f"; ln -s f "$tmp/trust/l"
path_trusted "$tmp/trust/l" >/dev/null && bad "path_trusted should refuse a symlink"
PKEXEC_UID=$(( $(id -u) + 1 )) path_trusted "$tmp/trust/f" >/dev/null && bad "path_trusted should refuse a file owned by someone other than the caller"

# copy_trusted_file checks the file it opened and never blocks
(
    cd "$tmp/trust" || exit 1
    copy_trusted_file f "$tmp/trust-copy" >/dev/null || exit 2
    [[ "$(cat "$tmp/trust-copy")" == x ]] || exit 3
    copy_trusted_file l "$tmp/trust-copy" >/dev/null && exit 4                 # symlink
    mkfifo fifo; chmod 0644 fifo
    export -f copy_trusted_file caller_uid
    out="$(timeout 10 bash -c 'copy_trusted_file fifo "$1"' _ "$tmp/trust-copy" 2>&1)"
    rc=$?; (( rc != 0 && rc != 124 )) || exit 5                                # refused, did not hang
    grep -q "not a regular file" <<< "$out" || exit 6
    chmod 0664 f; copy_trusted_file f "$tmp/trust-copy" >/dev/null && exit 7   # group-writable
    chmod 0644 f
    PKEXEC_UID=$(( $(id -u) + 1 )) copy_trusted_file f "$tmp/trust-copy" >/dev/null && exit 8
    rm -f fifo
    exit 0
) || bad "copy_trusted_file case $?"

# ── Kernel signing ───────────────────────────────────────────────────────
for p in 'abcdefgh' 'Abc123!@#$%^&*()' 'p4ssw0rd~'; do
    valid_mok_password "$p" || bad "valid_mok_password should accept [$p]"
done
for p in '' 'short' 'abcdefghijklmnopq' 'has space1' $'tab\there1' 'pässwörd1' $'new\nline12'; do
    valid_mok_password "$p" && bad "valid_mok_password should reject [$p]"
done
got="$(printf 'input password: \ninput password again: \n$6$salt$hashvalue\n' | mok_hash_from_output)"
[[ "$got" == '$6$salt$hashvalue' ]] || bad "mok_hash_from_output got [$got]"
got="$(printf 'input password: \nerror\n' | mok_hash_from_output)"
[[ -z "$got" ]] || bad "mok_hash_from_output should be empty without a hash line, got [$got]"

export KERNEL_POP_MOK_DIR="$tmp/mok" KERNEL_POP_SIGNED_DIR="$tmp/signed" KERNEL_POP_BOOT="$tmp/sboot"
mkdir -p "$KERNEL_POP_BOOT"
signing_key_present && bad "signing_key_present should be false with no key"
# Without a key, sign_kernel refuses and touches nothing
echo "image" > "$KERNEL_POP_BOOT/vmlinuz-1.0.0-nokey"
sign_kernel 1.0.0-nokey >/dev/null && bad "sign_kernel should fail without a key"
[[ "$(cat "$KERNEL_POP_BOOT/vmlinuz-1.0.0-nokey")" == "image" ]] || bad "sign_kernel without a key changed the image"
rm -f "$KERNEL_POP_BOOT/vmlinuz-1.0.0-nokey"

if ! command -v openssl >/dev/null 2>&1; then
    skip "openssl not installed — key generation and signing tests"
else
    generate_signing_key >/dev/null || bad "generate_signing_key failed"
    signing_key_present || bad "signing_key_present should be true after generation"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR/signing.key")" == "600" ]] || bad "signing.key should be mode 600"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR/signing.pem")" == "644" ]] || bad "signing.pem should be mode 644"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR/signing.der")" == "644" ]] || bad "signing.der should be mode 644"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR")" == "755" ]] || bad "the MOK dir should be mode 755"
    certtext="$(openssl x509 -in "$KERNEL_POP_MOK_DIR/signing.pem" -noout -text)"
    # Ubuntu's module-signing EKU: shim refuses kernels signed by a key carrying it.
    grep -qF "1.3.6.1.4.1.2312.16.1.2" <<< "$certtext" && bad "certificate must not carry the module-only EKU"
    grep -q "Code Signing" <<< "$certtext" || bad "certificate should carry the codeSigning EKU"
    grep -q "CA:FALSE" <<< "$certtext" || bad "certificate should be CA:FALSE"
    grep -q "CN *= *Kernel Pop kernel signing key" <<< "$certtext" || bad "certificate subject CN"
    openssl x509 -inform DER -in "$KERNEL_POP_MOK_DIR/signing.der" -noout >/dev/null 2>&1 || bad "signing.der is not a DER certificate"
    keysum="$(sha256sum < "$KERNEL_POP_MOK_DIR/signing.key")"
    generate_signing_key >/dev/null || bad "second generate_signing_key should succeed"
    [[ "$(sha256sum < "$KERNEL_POP_MOK_DIR/signing.key")" == "$keysum" ]] || bad "generate_signing_key overwrote an existing key"
    # An incomplete set is never overwritten
    mkdir -p "$tmp/partial"
    echo "keep" > "$tmp/partial/signing.key"
    KERNEL_POP_MOK_DIR="$tmp/partial" generate_signing_key >/dev/null && bad "generate_signing_key should refuse an incomplete key set"
    [[ "$(cat "$tmp/partial/signing.key")" == "keep" ]] || bad "generate_signing_key touched an incomplete key set"
    # Atomic: a failure part way leaves no key directory (and no temp dir) behind
    (
        openssl() { if [[ "$*" == *"-outform DER"* ]]; then return 1; fi; command openssl "$@"; }
        KERNEL_POP_MOK_DIR="$tmp/atomic/mok" generate_signing_key >/dev/null
    ) && bad "generate_signing_key should fail when the DER conversion fails"
    [[ -e "$tmp/atomic/mok" ]] && bad "a failed generate_signing_key left a key directory behind"
    [[ -n "$(compgen -G "$tmp/atomic/.mok.new.*")" ]] && bad "a failed generate_signing_key left its temp directory behind"
    # An existing empty key directory is replaced in one step
    mkdir -p "$tmp/atomic2/mok"
    KERNEL_POP_MOK_DIR="$tmp/atomic2/mok" generate_signing_key >/dev/null || bad "generate_signing_key into an empty directory"
    KERNEL_POP_MOK_DIR="$tmp/atomic2/mok" signing_key_present || bad "key set missing after generating into an empty directory"
    [[ -e "$tmp/atomic2/mok/req.cnf" ]] && bad "generate_signing_key left req.cnf in the key directory"
    # A non-empty directory without a key is refused, untouched
    mkdir -p "$tmp/atomic3/mok"; echo other > "$tmp/atomic3/mok/notes"
    KERNEL_POP_MOK_DIR="$tmp/atomic3/mok" generate_signing_key >/dev/null && bad "generate_signing_key should refuse a non-empty directory"
    [[ "$(ls "$tmp/atomic3/mok")" == "notes" ]] || bad "generate_signing_key changed a non-empty directory"

    pe=""
    for f in /usr/lib/systemd/boot/efi/linuxx64.efi.stub /usr/lib/systemd/boot/efi/*.efi \
             /usr/lib/systemd/boot/efi/*.efi.stub /usr/lib/shim/*.efi; do
        if [[ -f "$f" && -r "$f" && "$(head -c 2 "$f" 2>/dev/null)" == "MZ" ]]; then pe="$f"; break; fi
    done
    if ! command -v sbsign >/dev/null 2>&1 || ! command -v sbverify >/dev/null 2>&1; then
        skip "sbsign/sbverify not installed — kernel signing tests"
    elif [[ -z "$pe" ]]; then
        skip "no readable PE/EFI binary found — kernel signing tests"
    else
        k=9.9.9-070909-generic
        img="$KERNEL_POP_BOOT/vmlinuz-$k"
        cp "$pe" "$img"; chmod 0640 "$img"
        kernel_signed "$k" && bad "a copied unsigned EFI binary should not verify against our key"
        out="$(sign_kernel "$k")" || bad "sign_kernel failed on $pe: $out"
        kernel_signed "$k" || bad "the image should verify after sign_kernel"
        sbverify --cert "$KERNEL_POP_MOK_DIR/signing.pem" "$img" >/dev/null 2>&1 || bad "sbverify should accept the signed image"
        [[ "$(stat -c '%a' "$img")" == "640" ]] || bad "sign_kernel should keep the image mode, got $(stat -c '%a' "$img")"
        [[ "$(cat "$KERNEL_POP_SIGNED_DIR/$k")" == "$(stat -c '%s %Y' "$img")" ]] || bad "signed marker should hold size and mtime"
        [[ "$(stat -c '%a' "$KERNEL_POP_SIGNED_DIR/$k")" == "644" ]] || bad "signed marker should be mode 644"
        [[ -z "$(compgen -G "$KERNEL_POP_BOOT/.vmlinuz-*")" ]] || bad "sign_kernel left a temp file behind"
        # Second call is a no-op
        sum="$(sha256sum < "$img")"
        out="$(sign_kernel "$k")" || bad "second sign_kernel should succeed"
        grep -q "Already signed" <<< "$out" || bad "second sign_kernel should report already signed"
        [[ "$(sha256sum < "$img")" == "$sum" ]] || bad "second sign_kernel changed the image"
        got="$(KERNEL_POP_BOOT="$tmp/sboot" installed_kvers)"
        [[ "$got" == "$k" ]] || bad "installed_kvers should list only the kernel, got [$got]"
        remove_signed_marker "$k"
        [[ -e "$KERNEL_POP_SIGNED_DIR/$k" ]] && bad "remove_signed_marker left the marker"

        # sbsign failure (not a PE image): original byte-identical, no temp file, no marker
        bk=9.9.8-bad
        printf 'not a kernel image\n' > "$KERNEL_POP_BOOT/vmlinuz-$bk"
        cp "$KERNEL_POP_BOOT/vmlinuz-$bk" "$tmp/orig-bad"
        out="$(sign_kernel "$bk")" && bad "sign_kernel should fail on a non-PE image"
        grep -q "SIGN WARNING" <<< "$out" || bad "a failed sign should log a SIGN WARNING"
        cmp -s "$KERNEL_POP_BOOT/vmlinuz-$bk" "$tmp/orig-bad" || bad "a failed sign changed the original"
        [[ -z "$(compgen -G "$KERNEL_POP_BOOT/.vmlinuz-*")" ]] || bad "a failed sign left a temp file behind"
        [[ -e "$KERNEL_POP_SIGNED_DIR/$bk" ]] && bad "a failed sign wrote a marker"
        rm -f "$KERNEL_POP_BOOT/vmlinuz-$bk"

        # sbsign "succeeds" but writes garbage: the verify step must catch it
        gk=9.9.7-garbage
        cp "$pe" "$KERNEL_POP_BOOT/vmlinuz-$gk"
        cp "$pe" "$tmp/orig-garbage"
        sbsign() { local o="" a; for a in "$@"; do [[ "$o" == "--output" ]] && echo junk > "$a"; o="$a"; done; return 0; }
        out="$(sign_kernel "$gk")" && bad "sign_kernel should fail when the signed copy does not verify"
        unset -f sbsign
        cmp -s "$KERNEL_POP_BOOT/vmlinuz-$gk" "$tmp/orig-garbage" || bad "an unverified sign changed the original"
        [[ -z "$(compgen -G "$KERNEL_POP_BOOT/.vmlinuz-*")" ]] || bad "an unverified sign left a temp file behind"

        # sync_signed_to_esp checks the ESP copy that belongs to the kernel
        # asked about, and nothing for a kernel the boot loader keeps no copy of.
        (
            t_esp="$tmp/esp"; r=0
            mkdir -p "$t_esp/EFI/Pop_OS-1234" "$tmp/rootfs"
            detect_bootloader() { echo "kernelstub $t_esp"; }
            kernelstub() { echo "kernelstub ran" >> "$tmp/ks-calls"; }
            : > "$tmp/ks-calls"
            export KERNEL_POP_ROOTFS="$tmp/rootfs"
            ln -sfn "boot/vmlinuz-$k" "$tmp/rootfs/vmlinuz"
            ln -sfn "boot/vmlinuz-$gk" "$tmp/rootfs/vmlinuz.old"
            cp "$img" "$t_esp/EFI/Pop_OS-1234/vmlinuz.efi"                   # signed copy of $k
            cp "$tmp/orig-garbage" "$t_esp/EFI/Pop_OS-1234/vmlinuz-previous.efi"  # unsigned copy of $gk
            out="$(sync_signed_to_esp "$k")"
            grep -q "Verified: ESP copy .*/vmlinuz.efi is signed" <<< "$out" || { echo "FAIL: kernelstub current: $out"; r=1; }
            out="$(sync_signed_to_esp "$gk")"
            grep -q "SIGN WARNING: the ESP copy .*/vmlinuz-previous.efi does not" <<< "$out" || { echo "FAIL: kernelstub previous: $out"; r=1; }
            : > "$tmp/ks-calls"
            out="$(sync_signed_to_esp 9.9.1-other)"
            grep -q "SIGN WARNING\|Verified" <<< "$out" && { echo "FAIL: kernelstub reported on a kernel with no ESP copy: $out"; r=1; }
            [[ -s "$tmp/ks-calls" ]] && { echo "FAIL: kernelstub ran for a kernel with no ESP copy"; r=1; }
            # systemd-boot: kernel-install gets the initrd, the entry's own linux path is checked
            detect_bootloader() { echo "systemd-boot $t_esp"; }
            kernel-install() { echo "$*" > "$tmp/ki-args"; }
            mkdir -p "$t_esp/loader/entries" "$t_esp/mid/$k"
            printf 'title Ubuntu\nversion %s\nlinux /mid/%s/linux\ninitrd /mid/%s/initrd.img-%s\n' "$k" "$k" "$k" "$k" > "$t_esp/loader/entries/mid-$k.conf"
            cp "$img" "$t_esp/mid/$k/linux"
            touch "$KERNEL_POP_BOOT/initrd.img-$k"
            out="$(sync_signed_to_esp "$k")"
            [[ "$(cat "$tmp/ki-args")" == "add $k $KERNEL_POP_BOOT/vmlinuz-$k $KERNEL_POP_BOOT/initrd.img-$k" ]] \
                || { echo "FAIL: kernel-install args: $(cat "$tmp/ki-args")"; r=1; }
            grep -q "Verified: ESP copy $t_esp/mid/$k/linux is signed" <<< "$out" || { echo "FAIL: systemd-boot verify: $out"; r=1; }
            rm -f "$KERNEL_POP_BOOT/initrd.img-$k"
            out="$(sync_signed_to_esp "$k")"
            [[ "$(cat "$tmp/ki-args")" == "add $k $KERNEL_POP_BOOT/vmlinuz-$k" ]] \
                || { echo "FAIL: kernel-install args without an initrd: $(cat "$tmp/ki-args")"; r=1; }
            rm -rf "$t_esp" "$tmp/rootfs"
            exit "$r"
        ) || bad "sync_signed_to_esp checks"

        # do_setup_signing end to end, with mokutil, apt-get and the boot
        # loader stubbed: signs every kernel it can, carries on past one
        # that fails, and the password never reaches an argument list.
        (
            calls="$tmp/mokutil-calls"
            : > "$calls"
            export KERNEL_POP_EFIVARS="$tmp/efivars-empty" KERNEL_POP_DKMS_MOK="$tmp/no-dkms-key.der"
            mkdir -p "$KERNEL_POP_EFIVARS"
            mokutil() { mokutil_stub "$@"; }
            mokutil_stub() {
                echo "ARGS: $*" >> "$calls"
                case "$1" in
                    --test-key) echo "$2 is not enrolled" ;;
                    --generate-hash)
                        local a b; IFS= read -r a; IFS= read -r b
                        [[ "$a" == "$b" ]] || return 1
                        echo "input password:"; echo '$6$teststalt$fakehash' ;;
                    --import)
                        local hf="${*: -1}"
                        [[ "${*: -2:1}" == "--hash-file" && "$(stat -c '%a' "$hf")" == "600" ]] || return 1
                        cat "$hf" >> "$calls"; echo "$hf" > "$tmp/hashfile-path"
                        printf 'attrdata' > "$KERNEL_POP_EFIVARS/MokNew-$MOK_GUID" ;;
                esac
            }
            apt-get() { echo "apt-get should not run: tools are installed" >> "$calls"; return 1; }
            detect_bootloader() { echo "grub"; }
            rm -rf "$KERNEL_POP_SIGNED_DIR"
            cp "$pe" "$KERNEL_POP_BOOT/vmlinuz-9.9.6-setup"
            printf 'not a kernel image\n' > "$KERNEL_POP_BOOT/vmlinuz-9.9.5-broken"
            out="$(printf 'Secret-pw1\n' | do_setup_signing 2>&1)" || { echo "FAIL: do_setup_signing failed: $out"; exit 1; }
            r=0
            grep -q "Kernels: 2 signed now, 1 already signed, 1 failed" <<< "$out" || { echo "FAIL: setup summary: $out"; r=1; }
            grep -q "Enrollment queued" <<< "$out" || { echo "FAIL: setup should queue the enrollment"; r=1; }
            grep -q "Secret-pw1" "$calls" && { echo "FAIL: the password reached mokutil's argv"; r=1; }
            grep -q "Secret-pw1" <<< "$out" && { echo "FAIL: the password was printed"; r=1; }
            grep -qE -- "--import [^ ]*/key-1.der --hash-file" "$calls" || { echo "FAIL: mokutil --import call: $(cat "$calls")"; r=1; }
            [[ "$(cat "$KERNEL_POP_MOK_PENDING")" == "moknew-size 8"$'\n'"$(cert_sha1 "$KERNEL_POP_MOK_DIR/signing.der")" ]] || { echo "FAIL: the queued key was not recorded for the apps: $(cat "$KERNEL_POP_MOK_PENDING")"; r=1; }
            [[ "$(stat -c %a "$KERNEL_POP_MOK_PENDING")" == 644 && "$(stat -c %a "$(dirname "$KERNEL_POP_MOK_PENDING")")" == 755 ]] || { echo "FAIL: the shared list must be readable by the apps"; r=1; }
            [[ -z "$(compgen -G "$(dirname "$KERNEL_POP_MOK_PENDING")/.list.*")" ]] || { echo "FAIL: a temp list was left behind"; r=1; }
            grep -qF '$6$teststalt$fakehash' "$calls" || { echo "FAIL: the hash file did not hold the hash"; r=1; }
            [[ -e "$(cat "$tmp/hashfile-path")" ]] && { echo "FAIL: the hash file was left behind"; r=1; }
            grep -q "apt-get should not run" "$calls" && { echo "FAIL: apt-get ran with every tool installed"; r=1; }
            kernel_signed 9.9.6-setup || { echo "FAIL: setup did not sign 9.9.6-setup"; r=1; }
            [[ -f "$KERNEL_POP_SIGNED_DIR/$k" && -f "$KERNEL_POP_SIGNED_DIR/9.9.6-setup" ]] || { echo "FAIL: setup markers"; r=1; }
            [[ -e "$KERNEL_POP_SIGNED_DIR/9.9.5-broken" ]] && { echo "FAIL: marker for a kernel that failed"; r=1; }
            # Bad passwords are refused before anything runs
            : > "$calls"
            (printf 'short\n' | do_setup_signing >/dev/null 2>&1) && { echo "FAIL: setup accepted a short password"; r=1; }
            [[ -s "$calls" ]] && { echo "FAIL: setup ran mokutil despite a bad password"; r=1; }
            exit "$r"
        ) || bad "do_setup_signing end-to-end checks"

        # One request, one password: keys another tool already queued are
        # carried over, the DKMS key is added, enrolled keys are left out.
        (
            r=0
            fail() { echo "FAIL: $*"; r=1; }
            calls="$tmp/enroll-calls"
            mkcert() {   # <dir> <CN>
                mkdir -p "$1"
                openssl req -new -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=$2" \
                    -keyout "$1/k.pem" -out "$1/c.pem" >/dev/null 2>&1
                openssl x509 -in "$1/c.pem" -outform DER -out "$1/c.der"
            }
            mkcert "$tmp/other" "Other app key"
            mkcert "$tmp/dkms" "Test DKMS key"
            export KERNEL_POP_DKMS_MOK="$tmp/dkms/c.der" KERNEL_POP_EFIVARS="$tmp/efivars"
            moknew="$KERNEL_POP_EFIVARS/MokNew-$MOK_GUID"
            write_moknew() {   # <x509|sha256> <file>...: an efivarfs MokNew holding one list per file
                local kind="$1"; shift
                mkdir -p "$KERNEL_POP_EFIVARS"
                perl -e '
                    my $kind = shift;
                    my %g = (x509 => "a159c0a5e494a74a87b5ab155c2bf072", sha256 => "2616c4c14c509240aca941f936934328");
                    my $out = pack("V", 7);
                    for my $f (@ARGV) {
                        open(my $h, "<:raw", $f) or die; local $/; my $d = <$h>;
                        my $entry = ("\x11" x 16) . $d;
                        $out .= pack("H*", $g{$kind}) . pack("V3", 28 + length $entry, 0, length $entry) . $entry;
                    }
                    binmode STDOUT; print $out;
                ' "$kind" "$@" > "$moknew"
            }
            ENROLLED_CN=""
            mokutil() {
                echo "ARGS: $*" >> "$calls"
                case "$1" in
                    --test-key)
                        if [[ -n "$ENROLLED_CN" && "$(cert_cn "$2")" == "$ENROLLED_CN" ]]; then echo "$2 is already enrolled"; else echo "$2 is not enrolled"; fi ;;
                    --generate-hash) local a b; IFS= read -r a; IFS= read -r b; echo '$6$salt$hash' ;;
                    --revoke-import) echo "REVOKE" >> "$calls"; rm -f "$moknew" ;;
                    --import)
                        local -a files=(); shift
                        while [[ "$1" != "--hash-file" ]]; do files+=("$1"); shift; done
                        # $tmp/import-fails: how many imports fail before one works.
                        if [[ -s "$tmp/import-fails" ]] && (( $(cat "$tmp/import-fails") > 0 )); then
                            echo $(( $(cat "$tmp/import-fails") - 1 )) > "$tmp/import-fails"
                            echo "FAILED-IMPORT" >> "$calls"; return 1
                        fi
                        for f in "${files[@]}"; do echo "IMPORT $(cert_cn "$f")" >> "$calls"; done
                        write_moknew x509 "${files[@]}" ;;
                esac
            }
            list_fps() { tail -n +2 "$KERNEL_POP_MOK_PENDING" | sort | tr '\n' ' '; }
            fps_of() { local f; for f in "$@"; do cert_sha1 "$f"; done | sort | tr '\n' ' '; }
            queued() { grep '^IMPORT' "$calls" | sed 's/^IMPORT //' | sort | tr '\n' '|'; }

            # Another app's key is waiting: it stays, ours and the DKMS key join it, one password.
            : > "$calls"; write_moknew x509 "$tmp/other/c.der"
            out="$(queue_key_enrollment Secret-pw2 2>&1)" || fail "queue with a waiting request: $out"
            [[ "$(queued)" == "Kernel Pop kernel signing key|Other app key|Test DKMS key|" ]] || fail "queued [$(queued)]"
            grep -n "REVOKE\|IMPORT" "$calls" | head -1 | grep -q REVOKE || fail "the waiting request must be replaced before the import"
            grep -q "already waiting for enrollment" <<< "$out" || fail "the carried-over key should be named: $out"
            grep -q "That one password enrolls all of these keys" <<< "$out" || fail "the user should learn one password covers all: $out"
            [[ "$(list_fps)" == "$(fps_of "$tmp/other/c.der" "$tmp/dkms/c.der" "$KERNEL_POP_MOK_DIR/signing.der")" ]] || fail "the shared list should hold the three fingerprints: $(cat "$KERNEL_POP_MOK_PENDING")"
            [[ "$(head -1 "$KERNEL_POP_MOK_PENDING")" == "moknew-size $(stat -c %s "$moknew")" ]] || fail "the shared list should record MokNew's size: $(head -1 "$KERNEL_POP_MOK_PENDING")"
            grep -q "Secret-pw2" "$calls" && fail "the password reached mokutil's argv"

            # Each queue_key_enrollment below runs in a subshell of its own: its
            # EXIT trap removes its work directory, which a second call in the
            # same shell would replace.
            # The DKMS key was already waiting (queued by another tool): listed once.
            : > "$calls"; write_moknew x509 "$tmp/dkms/c.der"
            (queue_key_enrollment Secret-pw3 >/dev/null 2>&1) || fail "queue with the DKMS key waiting"
            [[ "$(queued)" == "Kernel Pop kernel signing key|Test DKMS key|" ]] || fail "dedupe: queued [$(queued)]"

            # Kernel Pop's key is already enrolled: only the DKMS key is queued; no request to replace.
            : > "$calls"; rm -f "$moknew"; ENROLLED_CN="Kernel Pop kernel signing key"
            (queue_key_enrollment Secret-pw4 >/dev/null 2>&1) || fail "queue with our key enrolled"
            [[ "$(queued)" == "Test DKMS key|" ]] || fail "enrolled key skipped: queued [$(queued)]"
            grep -q REVOKE "$calls" && fail "nothing was waiting, so nothing should be revoked"

            # Everything enrolled: nothing queued, the app's list is removed.
            : > "$calls"; ENROLLED_CN=""
            mokutil_all_enrolled() { :; }
            mokutil() { echo "ARGS: $*" >> "$calls"; [[ "$1" == "--test-key" ]] && echo "$2 is already enrolled"; return 0; }
            out="$(queue_key_enrollment Secret-pw5 2>&1)" || fail "queue with everything enrolled"
            grep -q "nothing to queue" <<< "$out" || fail "should say there is nothing to queue: $out"
            grep -q -- "--import" "$calls" && fail "nothing should be imported when every key is enrolled"
            [[ -e "$KERNEL_POP_MOK_PENDING" ]] && fail "the shared list should be removed"

            # A waiting request with a hash entry cannot be carried over: refused, untouched.
            : > "$calls"; write_moknew sha256 "$tmp/other/c.der"
            (queue_key_enrollment Secret-pw6 >/dev/null 2>&1) && fail "a request holding hashes must not be replaced"
            grep -q -- "--revoke-import\|--import" "$calls" && fail "nothing may be revoked or imported then"
            [[ -e "$moknew" ]] || fail "the waiting request must be left alone"
            # A damaged variable: refused as well.
            printf 'xxxxshort' > "$moknew"
            (queue_key_enrollment Secret-pw7 >/dev/null 2>&1) && fail "a damaged request must not be replaced"

            # Back to the full mokutil stub (the everything-enrolled case replaced it).
            mokutil() {
                echo "ARGS: $*" >> "$calls"
                case "$1" in
                    --test-key) echo "$2 is not enrolled" ;;
                    --generate-hash) local a b; IFS= read -r a; IFS= read -r b; echo '$6$salt$hash' ;;
                    --revoke-import) echo "REVOKE" >> "$calls"; rm -f "$moknew" ;;
                    --import)
                        local -a files=(); shift
                        while [[ "$1" != "--hash-file" ]]; do files+=("$1"); shift; done
                        if [[ -s "$tmp/import-fails" ]] && (( $(cat "$tmp/import-fails") > 0 )); then
                            echo $(( $(cat "$tmp/import-fails") - 1 )) > "$tmp/import-fails"
                            echo "FAILED-IMPORT" >> "$calls"; return 1
                        fi
                        for f in "${files[@]}"; do echo "IMPORT $(cert_cn "$f")" >> "$calls"; done
                        write_moknew x509 "${files[@]}" ;;
                esac
            }
            malformed_moknew() {   # <header size> <list size adjustment>: one x509 list, sizes off
                perl -e '
                    my ($hsize, $adj, $f) = @ARGV;
                    open(my $h, "<:raw", $f) or die; local $/; my $d = <$h>;
                    my $entry = ("\x11" x 16) . $d;
                    my $lsize = 28 + length($entry) + $adj;
                    my $body = $entry . ("\0" x ($adj > 0 ? $adj : 0));
                    binmode STDOUT;
                    print pack("V", 7) . pack("H*", "a159c0a5e494a74a87b5ab155c2bf072") . pack("V3", $lsize, $hsize, length $entry) . $body;
                ' "$1" "$2" "$tmp/other/c.der" > "$moknew"
            }

            # Several signature lists in the waiting request: every certificate is carried over.
            : > "$calls"; write_moknew x509 "$tmp/other/c.der" "$tmp/dkms/c.der"
            out="$(queue_key_enrollment Secret-pw8 2>&1)" || fail "queue with two waiting lists: $out"
            [[ "$(queued)" == "Kernel Pop kernel signing key|Other app key|Test DKMS key|" ]] || fail "two lists: queued [$(queued)]"
            [[ "$(grep -c "already waiting for enrollment" <<< "$out")" == 2 ]] || fail "both waiting keys should be named: $out"
            [[ "$(list_fps)" == "$(fps_of "$tmp/other/c.der" "$tmp/dkms/c.der" "$KERNEL_POP_MOK_DIR/signing.der")" ]] || fail "two lists: shared list $(cat "$KERNEL_POP_MOK_PENDING")"

            # A header that runs past its list, or a list that is not whole
            # entries: unreadable, left alone, the shared list kept as it was.
            cp "$KERNEL_POP_MOK_PENDING" "$tmp/list-before"
            for bad_sizes in "4096 0" "0 5"; do
                : > "$calls"; malformed_moknew $bad_sizes
                rc=0; pending_mok_certs "$tmp/malformed-out" 2>/dev/null || rc=$?
                [[ $rc == 2 ]] || fail "malformed list ($bad_sizes) should read as damaged, got $rc"
                out="$(queue_key_enrollment Secret-pw9 2>&1)" && fail "a malformed request ($bad_sizes) must not be replaced"
                grep -q "could not be read" <<< "$out" || fail "malformed request ($bad_sizes): $out"
                grep -q "REVOKE\|IMPORT" "$calls" && fail "nothing may be revoked or imported for a malformed request ($bad_sizes)"
                cmp -s "$KERNEL_POP_MOK_PENDING" "$tmp/list-before" || fail "the shared list changed for a malformed request"
            done

            # A waiting certificate openssl can't read: nothing is revoked, the request stays.
            printf 'not a certificate' > "$tmp/junk.der"
            : > "$calls"; write_moknew x509 "$tmp/other/c.der" "$tmp/junk.der"
            # With errexit on, as in a real run.
            ( set -e; queue_key_enrollment Secret-pw10 ) > "$tmp/q-out" 2>&1; rc=$?
            out="$(cat "$tmp/q-out")"
            (( rc != 0 )) || fail "a request with an unreadable certificate must not be replaced"
            grep -q "not a readable certificate, so Kernel Pop will not replace it" <<< "$out" || fail "unreadable waiting entry: $out"
            grep -q "REVOKE\|IMPORT" "$calls" && fail "nothing may be revoked or imported with an unreadable waiting entry"
            [[ -e "$moknew" ]] || fail "the request with an unreadable entry must be left alone"

            # One of our own certificates unreadable: a hard error, nothing changed.
            : > "$calls"; write_moknew x509 "$tmp/other/c.der"
            ( set -e; KERNEL_POP_DKMS_MOK="$tmp/junk.der" queue_key_enrollment Secret-pw11 ) > "$tmp/q-out" 2>&1; rc=$?
            out="$(cat "$tmp/q-out")"
            (( rc != 0 )) || fail "an unreadable DKMS certificate must be an error"
            grep -q "Could not read the certificate $tmp/junk.der" <<< "$out" || fail "unreadable own certificate: $out"
            grep -q "REVOKE\|IMPORT" "$calls" && fail "nothing may be revoked or imported with an unreadable own certificate"

            # The import fails once after the revoke: retried, and the request is in place.
            : > "$calls"; write_moknew x509 "$tmp/other/c.der"; echo 1 > "$tmp/import-fails"
            out="$(queue_key_enrollment Secret-pw12 2>&1)" || fail "one failed import should be retried: $out"
            [[ "$(grep -c FAILED-IMPORT "$calls")" == 1 ]] || fail "exactly one failed import expected"
            [[ "$(queued)" == "Kernel Pop kernel signing key|Other app key|Test DKMS key|" ]] || fail "retry: queued [$(queued)]"
            [[ "$(head -1 "$KERNEL_POP_MOK_PENDING")" == "moknew-size $(stat -c %s "$moknew")" ]] || fail "retry: shared list not rewritten"

            # It fails twice: every key is named with its fingerprint, the shared list goes.
            : > "$calls"; write_moknew x509 "$tmp/other/c.der"; echo 2 > "$tmp/import-fails"
            out="$(queue_key_enrollment Secret-pw13 2>&1)" && fail "two failed imports must fail the setup"
            [[ "$(grep -c FAILED-IMPORT "$calls")" == 2 ]] || fail "the import should be tried exactly twice"
            grep -q "None of these keys is waiting for enrollment now" <<< "$out" || fail "the lost keys should be announced: $out"
            for c in "$tmp/other/c.der" "$tmp/dkms/c.der" "$KERNEL_POP_MOK_DIR/signing.der"; do
                grep -qF "$(cert_cn "$c") (SHA1 $(cert_sha1 "$c"))" <<< "$out" || fail "the lost key $(cert_cn "$c") should be named with its SHA1: $out"
            done
            grep -q "Run Set Up Signing again" <<< "$out" || fail "the user should be told to run Set Up Signing again: $out"
            [[ -e "$KERNEL_POP_MOK_PENDING" ]] && fail "the shared list must be removed when nothing is queued"
            rm -f "$tmp/import-fails"
            exit "$r"
        ) || bad "enrollment request merging"
    fi
fi
unset KERNEL_POP_MOK_DIR KERNEL_POP_SIGNED_DIR KERNEL_POP_BOOT

# ── Staging and verifying a build, then do_install end to end ───────────
# Every system command do_install runs is stubbed, and every path points
# into $tmp.
(
    PKEXEC_UID="$(id -u)"; export PKEXEC_UID
    kv=7.7.7-kernelpop
    make_build() {   # <dir> [extra deb names...]: a consistent build of fake debs with its manifest
        local d="$1" f; shift
        rm -rf "$d"; mkdir -p "$d"; chmod 0755 "$d"
        for f in "linux-headers-${kv}_7.7.7-1_amd64.deb" "linux-image-${kv}_7.7.7-1_amd64.deb" "$@"; do
            echo "contents of $f" > "$d/$f"; chmod 0644 "$d/$f"
        done
        (cd "$d" && sha256sum ./*.deb | sed 's| \./| |' > SHA256SUMS)
        chmod 0644 "$d/SHA256SUMS"
        echo "config" > "$d/config-$kv"; echo "log" > "$d/build.log"
    }
    stage_ok() { local s; s="$(mktemp -d)"; (stage_verified_debs "$1" "$s") >"$tmp/stage-out" 2>&1; local r=$?; rm -rf "$s"; return $r; }
    fails=0

    make_build "$tmp/dl"
    stage_ok "$tmp/dl" || bad "stage_verified_debs should accept a good build: $(cat "$tmp/stage-out")"
    s="$(mktemp -d)"; (stage_verified_debs "$tmp/dl" "$s") >/dev/null 2>&1
    [[ "$(ls "$s" | LC_ALL=C sort | tr '\n' ' ')" == "SHA256SUMS linux-headers-${kv}_7.7.7-1_amd64.deb linux-image-${kv}_7.7.7-1_amd64.deb " ]] \
        || bad "stage_verified_debs copied [$(ls "$s" | tr '\n' ' ')]"
    rm -rf "$s"
    echo tampered >> "$tmp/dl/linux-image-${kv}_7.7.7-1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a deb that does not match the manifest"
    grep -q "SHA256 mismatch" "$tmp/stage-out" || bad "a hash mismatch should say so: $(cat "$tmp/stage-out")"
    make_build "$tmp/dl"; echo extra > "$tmp/dl/linux-extra_1_amd64.deb"; chmod 0644 "$tmp/dl/linux-extra_1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a deb missing from the manifest"
    grep -q "not listed in SHA256SUMS" "$tmp/stage-out" || bad "an unlisted deb should say so"
    make_build "$tmp/dl"; rm "$tmp/dl/SHA256SUMS"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a build without SHA256SUMS"
    make_build "$tmp/dl"; echo "$(printf '0%.0s' {1..64})  ../../etc/passwd" >> "$tmp/dl/SHA256SUMS"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a manifest entry that is not a plain .deb name"
    grep -q "not a plain package file name" "$tmp/stage-out" || bad "an odd manifest name should say so: $(cat "$tmp/stage-out")"
    make_build "$tmp/dl"; rm "$tmp/dl/linux-headers-${kv}_7.7.7-1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a set missing a package the manifest lists"
    grep -q "the set is incomplete" "$tmp/stage-out" || bad "an incomplete set should say so: $(cat "$tmp/stage-out")"
    make_build "$tmp/dl"; chmod 0664 "$tmp/dl/linux-image-${kv}_7.7.7-1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a group-writable deb"
    make_build "$tmp/dl"; chmod 0664 "$tmp/dl/SHA256SUMS"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a group-writable manifest"
    make_build "$tmp/dl"; chmod 0775 "$tmp/dl"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a group-writable directory"
    make_build "$tmp/dl"; ln -s "$tmp/dl" "$tmp/dl-link"
    stage_ok "$tmp/dl-link" && bad "stage_verified_debs should refuse a symlinked directory"
    (PKEXEC_UID=$(( $(id -u) + 1 )) stage_ok "$tmp/dl") && bad "stage_verified_debs should refuse a build owned by someone other than the caller"

    # do_install, with the boot loader, dpkg and friends stubbed.
    running="$(uname -r)"
    run_install() {   # <scenario-name> [extra settings via env]; prints the script output
        local root="$tmp/inst-$1"
        rm -rf "$root"; mkdir -p "$root/boot" "$root/modules" "$root/staging" "$root/mok"
        # shellcheck disable=SC2086
        make_build "$root/dl" ${EXTRA_DEBS:-}
        cat > "$root/grub.cfg" <<EOF2
menuentry 'Ubuntu' --class ubuntu \$menuentry_id_option 'gnulinux-simple-x' {
}
submenu 'Advanced options for Ubuntu' \$menuentry_id_option 'gnulinux-advanced-x' {
	menuentry 'Ubuntu, with Linux $kv' --class ubuntu \$menuentry_id_option 'gnulinux-$kv-advanced-x' {
	}
	menuentry 'Ubuntu, with Linux $running' --class ubuntu \$menuentry_id_option 'gnulinux-$running-advanced-x' {
	}
}
EOF2
        [[ -n "${FLAT_GRUB:-}" ]] && cat > "$root/grub.cfg" <<EOF2
menuentry 'Ubuntu, with Linux $kv' --class ubuntu \$menuentry_id_option 'gnulinux-$kv-advanced-x' {
}
menuentry 'Ubuntu, with Linux $running' --class ubuntu \$menuentry_id_option 'gnulinux-$running-advanced-x' {
}
EOF2
        echo "GRUB_DEFAULT=saved" > "$root/default-grub"
        echo "gnulinux-advanced-x>gnulinux-$running-advanced-x" > "$root/saved"
        (
            export KERNEL_POP_BOOT="$root/boot" KERNEL_POP_MODULES="$root/modules" \
                   KERNEL_POP_STAGING="$root/staging" KERNEL_POP_MOK_DIR="$root/mok" \
                   KERNEL_POP_SIGNED_DIR="$root/signed" KERNEL_POP_GRUB_CFG="$root/grub.cfg" \
                   KERNEL_POP_GRUB_DEFAULTS="$root/default-grub" KERNEL_POP_DKMS_TREE="$root/dkms"
            if [[ -n "${WITH_KEY:-}" ]]; then touch "$root/mok/signing.key" "$root/mok/signing.pem" "$root/mok/signing.der"; fi
            dpkg() {
                shift; printf '%s\n' "$@" > "$root/dpkg-args"
                touch "$KERNEL_POP_BOOT/vmlinuz-$kv"; mkdir -p "$KERNEL_POP_MODULES/$kv/build"
            }
            apt-get() { echo "$*" >> "$root/apt-args"; [[ "$3" == "purge" ]]; }
            update-initramfs() { touch "$KERNEL_POP_BOOT/initrd.img-$3"; }   # -c|-u -k <kver>
            if [[ -n "${DKMS_BROKEN:-}" ]]; then
                dkms() { [[ "$1" == "status" ]] && echo "nvidia/615.78.08, $kv, x86_64: built"; [[ "$1" == "status" ]]; }
            else
                dkms() { return 0; }
            fi
            detect_bootloader() { echo "grub"; }
            update-grub() { :; }
            grub-editenv() { echo "saved_entry=$(cat "$root/saved")"; }
            grub-set-default() { echo "$1" > "$root/saved"; }
            grub-reboot() { echo "$1" > "$root/next"; }
            if [[ -n "${SB_ON:-}" ]]; then secure_boot_enforcing() { return 0; }; else secure_boot_enforcing() { return 1; }; fi
            [[ -n "${KEY_STATE:-}" ]] && key_enrollment_state() { echo "$KEY_STATE"; }
            [[ -n "${SIGN_OK:-}" ]] && sign_kernel() { log "Signed: stub"; }
            do_install "$root/dl"
        ) 2>&1
    }
    count() { grep -c "^\[kernel-pop\] $1" <<< "$2"; }

    # Secure Boot off: the saved default moves to the new kernel; one DKMS problem is counted once
    out="$(DKMS_BROKEN=1 run_install off)" || bad "do_install (Secure Boot off) failed: $out"
    root="$tmp/inst-off"
    [[ "$(cat "$root/saved")" == "gnulinux-advanced-x>gnulinux-$kv-advanced-x" ]] || bad "saved default should point at the new kernel, got $(cat "$root/saved")"
    grep -q "Kernel version (from package metadata): $kv" <<< "$out" || bad "the version should come from the image package: $out"
    grep -q "Reboot when ready" <<< "$out" || bad "a bootable install should end with Reboot when ready"
    grep -q "BOOT PENDING" <<< "$out" && bad "no BOOT marker expected with Secure Boot off"
    [[ "$(count 'DKMS WARNING:' "$out")" == 1 ]] || bad "one broken module should give one DKMS WARNING line, got $(count 'DKMS WARNING:' "$out")"
    [[ "$(count 'DKMS SUMMARY:' "$out")" == 1 ]] || bad "a DKMS problem should give one DKMS SUMMARY line"
    while IFS= read -r f; do
        [[ "$f" == "$root/staging/kernel-pop-install."*/*.deb ]] || bad "dpkg installed $f, not a staged copy"
    done < "$root/dpkg-args"
    [[ "$(wc -l < "$root/dpkg-args")" == 2 ]] || bad "dpkg should get the image and the headers"
    [[ -z "$(ls -A "$root/staging")" ]] || bad "do_install left its staging directory behind"

    # GRUB_DISABLE_SUBMENU=y: the saved default is the bare entry id
    out="$(FLAT_GRUB=1 run_install flat)" || bad "do_install (flat GRUB menu) failed: $out"
    [[ "$(cat "$tmp/inst-flat/saved")" == "gnulinux-$kv-advanced-x" ]] || bad "flat menu saved default got $(cat "$tmp/inst-flat/saved")"

    # linux-libc-dev in the set (even listed in the manifest): refused, nothing installed
    out="$(EXTRA_DEBS="linux-libc-dev_7.7.7-1_amd64.deb" run_install libc)" && bad "do_install must refuse a set with linux-libc-dev"
    [[ -e "$tmp/inst-libc/dpkg-args" ]] && bad "dpkg ran on a set with linux-libc-dev"
    grep -q "never linux-libc-dev" <<< "$out" || bad "the libc-dev refusal should say why: $out"
    # A -dbg package: refused the same way
    out="$(EXTRA_DEBS="linux-image-${kv}-dbg_7.7.7-1_amd64.deb" run_install dbg)" && bad "do_install must refuse a set with a -dbg package"
    [[ -e "$tmp/inst-dbg/dpkg-args" ]] && bad "dpkg ran on a set with a -dbg package"
    # Headers of another kernel: refused
    out="$(EXTRA_DEBS="linux-headers-7.7.6-kernelpop_7.7.6-1_amd64.deb" run_install otherheaders)" && bad "do_install must refuse headers of another kernel"
    grep -q "does not belong to kernel $kv" <<< "$out" || bad "the mixed-set refusal should say why: $out"
    [[ -e "$tmp/inst-otherheaders/dpkg-args" ]] && bad "dpkg ran on a mixed set"

    # Secure Boot enforcing, no key: refused before anything is installed
    out="$(SB_ON=1 run_install nokey)" && bad "do_install should refuse with Secure Boot on and no key"
    root="$tmp/inst-nokey"
    [[ -e "$root/dpkg-args" ]] && bad "dpkg ran although the kernel could not boot"
    grep -q "kernel signing is not set up.*Nothing was installed" <<< "$out" || bad "the refusal should say signing is not set up: $out"
    [[ "$(cat "$root/saved")" == "gnulinux-advanced-x>gnulinux-$running-advanced-x" ]] || bad "a refused install changed the default"

    # Key neither enrolled nor queued: refused before anything is installed
    out="$(SB_ON=1 WITH_KEY=1 KEY_STATE=not-enrolled run_install notenrolled)" && bad "do_install should refuse a key that is not enrolled"
    [[ -e "$tmp/inst-notenrolled/dpkg-args" ]] && bad "dpkg ran with a key that is not enrolled"
    grep -q "neither enrolled nor waiting" <<< "$out" || bad "the refusal should say the key is not enrolled"

    # Signing fails with Secure Boot on: the just-installed packages are purged again
    out="$(SB_ON=1 WITH_KEY=1 KEY_STATE=enrolled run_install signfail)" && bad "do_install should fail when signing fails under Secure Boot"
    root="$tmp/inst-signfail"
    [[ "$(count 'SIGN WARNING:' "$out")" == 1 ]] || bad "a failed sign should give exactly one SIGN WARNING line, got $(count 'SIGN WARNING:' "$out")"
    [[ "$(cat "$root/apt-args" 2>/dev/null)" == "$APT_OPTS purge -y linux-headers-$kv linux-image-$kv" ]] \
        || bad "the rollback should purge exactly the installed packages, got [$(cat "$root/apt-args" 2>/dev/null)]"
    grep -q "removed again. Nothing is left installed" <<< "$out" || bad "the rollback should be reported: $out"
    [[ "$(cat "$root/saved")" == "gnulinux-advanced-x>gnulinux-$running-advanced-x" ]] || bad "a rolled-back install changed the default"

    # Signing fails with Secure Boot off: kept, one SIGN WARNING, summary not counted
    out="$(WITH_KEY=1 run_install signfail-off)" || bad "do_install (sign fails, Secure Boot off) failed: $out"
    [[ "$(count 'SIGN WARNING:' "$out")" == 1 ]] || bad "Secure Boot off: one SIGN WARNING line expected"
    [[ "$(count 'SIGN SUMMARY:' "$out")" == 1 ]] || bad "Secure Boot off: one SIGN SUMMARY line expected"
    [[ -e "$tmp/inst-signfail-off/apt-args" ]] && bad "Secure Boot off: nothing should be rolled back"

    # Signed, enrollment pending: default moves, BOOT PENDING says what to do
    out="$(SB_ON=1 WITH_KEY=1 SIGN_OK=1 KEY_STATE=pending run_install pending)" || bad "do_install (pending) failed: $out"
    [[ "$(count 'BOOT PENDING:' "$out")" == 1 ]] || bad "pending enrollment needs one BOOT PENDING line"
    [[ "$(cat "$tmp/inst-pending/saved")" == "gnulinux-advanced-x>gnulinux-$kv-advanced-x" ]] || bad "pending: the saved default should move"
    grep -q "Reboot when ready" <<< "$out" && bad "pending enrollment must not end with a plain Reboot when ready"

    # Signed with an enrolled key: plain success
    out="$(SB_ON=1 WITH_KEY=1 SIGN_OK=1 KEY_STATE=enrolled run_install enrolled)" || bad "do_install (enrolled) failed: $out"
    grep -q "Reboot when ready" <<< "$out" || bad "enrolled: should end with Reboot when ready"

    # A tampered build never reaches dpkg
    root="$tmp/inst-off"
    out="$(
        make_build "$root/dl"; echo bad >> "$root/dl/linux-image-${kv}_7.7.7-1_amd64.deb"
        rm -f "$root/dpkg-args"
        dpkg() { echo called > "$root/dpkg-args"; }
        KERNEL_POP_STAGING="$root/staging" do_install "$root/dl" 2>&1
    )" && bad "do_install should fail on a tampered build"
    [[ -e "$root/dpkg-args" ]] && bad "dpkg ran on a tampered build"
    [[ -z "$(ls -A "$root/staging")" ]] || bad "a refused install left its staging directory behind"
    exit "$fails"
) || { echo "FAIL: staging/do_install group"; fails=$((fails + 1)); }

# ── Real packages: the Package field decides, never the file name ───────
# Tiny .debs built with the real dpkg-deb, named to mislead.
(
    fails=0
    unset -f dpkg-deb
    mkdeb() {   # <file> <Package>
        local d; d="$(mktemp -d)"
        mkdir -p "$d/DEBIAN"
        printf 'Package: %s\nVersion: 1\nArchitecture: all\nMaintainer: t <t@invalid>\nDescription: test\n' "$2" > "$d/DEBIAN/control"
        dpkg-deb --build --root-owner-group "$d" "$1" >/dev/null 2>&1 || echo "FAIL: could not build $1"
        rm -rf "$d"
    }
    r="$tmp/realdebs"; mkdir -p "$r"
    mkdeb "$r/linux-image-9.9.9-kernelpop_1_amd64.deb" linux-libc-dev
    deb_allowed "$r/linux-image-9.9.9-kernelpop_1_amd64.deb" && bad "a deb whose Package is linux-libc-dev must be refused whatever its file name"
    mkdeb "$r/linux-headers-9.9.9-kernelpop_1_amd64.deb" linux-image-9.9.9-kernelpop-dbg
    deb_allowed "$r/linux-headers-9.9.9-kernelpop_1_amd64.deb" && bad "a deb whose Package is a -dbg image must be refused whatever its file name"
    mkdeb "$r/evil_1_all.deb" linux-image-9.9.9-kernelpop
    deb_allowed "$r/evil_1_all.deb" || bad "a deb whose Package is a kernel image is allowed whatever its file name"
    mkdeb "$r/linux-image-1.1.1-kernelpop_1_amd64.deb" linux-headers-9.9.9-kernelpop
    got="$(kver_from_debs "$r")"; rc=$?
    [[ $rc -ne 0 || "$got" == "9.9.9-kernelpop" ]] || bad "kver_from_debs read [$got] from a file name instead of the Package field"
    rm -f "$r/linux-image-9.9.9-kernelpop_1_amd64.deb" "$r/linux-headers-9.9.9-kernelpop_1_amd64.deb"
    got="$(kver_from_debs "$r")"
    [[ "$got" == "9.9.9-kernelpop" ]] || bad "kver_from_debs should read 9.9.9-kernelpop from the Package field, got [$got]"
    exit "$fails"
) || { echo "FAIL: real packages group"; fails=$((fails + 1)); }

# ── Build tools: only allowlisted packages ever reach apt-get ────────────
(
    fails=0
    apt-get() { echo "$*" >> "$tmp/deps-apt"; return 0; }
    rm -f "$tmp/deps-apt"
    out="$(do_install_build_deps pahole zstd 2>&1)" || bad "do_install_build_deps should install allowlisted packages: $out"
    [[ "$(cat "$tmp/deps-apt")" == "$APT_OPTS install -y pahole zstd" ]] || bad "apt-get got [$(cat "$tmp/deps-apt" 2>/dev/null)]"
    rm -f "$tmp/deps-apt"
    (do_install_build_deps pahole bash >/dev/null 2>&1) && bad "do_install_build_deps should refuse a package outside the allowlist"
    (do_install_build_deps 'pahole;id' >/dev/null 2>&1) && bad "do_install_build_deps should refuse an odd name"
    (do_install_build_deps '../x' >/dev/null 2>&1) && bad "do_install_build_deps should refuse a path"
    [[ -e "$tmp/deps-apt" ]] && bad "apt-get ran for a refused package list"
    (do_install_build_deps >/dev/null 2>&1) && bad "do_install_build_deps should fail with no packages"
    # A failed install is retried once after an index refresh
    apt-get() { echo "$*" >> "$tmp/deps-apt"; [[ "$3" == "update" || -e "$tmp/deps-updated" ]] && { touch "$tmp/deps-updated"; return 0; }; return 1; }
    rm -f "$tmp/deps-apt" "$tmp/deps-updated"
    do_install_build_deps cpio >/dev/null 2>&1 || bad "do_install_build_deps should succeed after an index refresh"
    [[ "$(tr '\n' '|' < "$tmp/deps-apt")" == "$APT_OPTS install -y cpio|$APT_OPTS update|$APT_OPTS install -y cpio|" ]] || bad "retry sequence got [$(tr '\n' '|' < "$tmp/deps-apt")]"
    exit "$fails"
) || { echo "FAIL: build tools group"; fails=$((fails + 1)); }

# ── Package manager lock: wait for an updater, then go on ──────────────
(
    fails=0
    : > "$tmp/dpkg/lock-frontend"; : > "$tmp/dpkg/lock"
    mkdir -p "$tmp/proc/4883"; echo packagekitd > "$tmp/proc/4883/comm"
    lock_line() {
        local dev ino
        read -r dev ino < <(stat -L -c '%d %i' "$1")
        printf '1: POSIX  ADVISORY  WRITE 4883 %02x:%02x:%s 0 EOF\n' \
            $(( ((dev >> 8) & 0xfff) | ((dev >> 32) & ~0xfff) )) \
            $(( (dev & 0xff) | ((dev >> 12) & ~0xff) )) "$ino"
    }
    [[ -z "$(package_lock_holder)" ]] || bad "package_lock_holder should see no holder with an empty /proc/locks"
    lock_line "$tmp/dpkg/lock-frontend" > "$tmp/proc/locks"
    [[ "$(package_lock_holder)" == "4883 packagekitd" ]] || bad "package_lock_holder got [$(package_lock_holder)]"
    sed 's/^1: /1: -> /' "$tmp/proc/locks" > "$tmp/proc/locks.w"; mv "$tmp/proc/locks.w" "$tmp/proc/locks"
    [[ -z "$(package_lock_holder)" ]] || bad "package_lock_holder should ignore a waiting request"
    lock_line "$tmp/dpkg/lock" > "$tmp/proc/locks"
    [[ "$(package_lock_holder)" == "4883 packagekitd" ]] || bad "package_lock_holder should see the database lock"
    # A lock on another file with another inode is not dpkg's
    : > "$tmp/not-dpkg-lock"; lock_line "$tmp/not-dpkg-lock" > "$tmp/proc/locks"
    [[ -z "$(package_lock_holder)" ]] || bad "package_lock_holder matched an unrelated lock"

    # The updater finishes after two polls: apt-get runs only then
    lock_line "$tmp/dpkg/lock-frontend" > "$tmp/proc/locks"
    polls=0
    sleep() { polls=$((polls + 1)); (( polls >= 2 )) && : > "$tmp/proc/locks"; return 0; }
    apt-get() { echo "polls=$polls $*" >> "$tmp/lock-apt"; return 0; }
    rm -f "$tmp/lock-apt"
    # The wait messages go to fd 3, the script's own output
    out="$(do_install_build_deps pahole 2>&1 3>&1)" || bad "do_install_build_deps should go on once the lock is free: $out"
    [[ "$(cat "$tmp/lock-apt")" == "polls=2 $APT_OPTS install -y pahole" ]] || bad "apt-get ran before the lock was free: [$(cat "$tmp/lock-apt")]"
    grep -q "packagekitd (process 4883).*Waiting" <<<"$out" || bad "the wait should name the holder: $out"
    [[ "$(grep -c "Waiting for it" <<<"$out")" == 1 ]] || bad "the wait message should appear once: $out"
    grep -q "free again" <<<"$out" || bad "no message when the lock was free again: $out"

    # A holder that never lets go: give up after LOCK_WAIT, apt-get never runs
    lock_line "$tmp/dpkg/lock-frontend" > "$tmp/proc/locks"
    sleep() { return 0; }
    rm -f "$tmp/lock-apt"
    out="$(LOCK_WAIT=20 LOCK_POLL=5 do_install_build_deps pahole 2>&1 3>&1)" && bad "do_install_build_deps should fail while the lock stays taken"
    grep -q "still in use by packagekitd" <<<"$out" || bad "the timeout should name the holder: $out"
    [[ -e "$tmp/lock-apt" ]] && bad "apt-get ran although the lock never came free"
    : > "$tmp/proc/locks"

    # dpkg -i that lost the lock race is run again; other failures are not
    # dpkg runs in a command substitution: count its runs in a file
    rm -f "$tmp/dpkg-runs"
    dpkg() { echo x >> "$tmp/dpkg-runs"; [[ "$(wc -l < "$tmp/dpkg-runs")" == 1 ]] && { echo "dpkg: error: dpkg frontend lock was locked by another process with pid 4883"; return 2; }; echo "dpkg $*"; return 0; }
    out="$(dpkg_install a.deb)" || bad "dpkg_install should retry after a lock error: $out"
    [[ "$(tail -1 <<<"$out")" == "dpkg -i a.deb" ]] || bad "dpkg_install retry got [$out]"
    dpkg() { echo "dpkg: error processing archive a.deb"; return 1; }
    dpkg_install a.deb >/dev/null && bad "dpkg_install should pass on a real dpkg failure"
    dpkg_install a.deb >/dev/null; [[ $? == 1 ]] || bad "dpkg_install should return dpkg's exit code"
    exit "$fails"
) || { echo "FAIL: package lock group"; fails=$((fails + 1)); }
rm -f "$tmp/dpkg/lock-frontend" "$tmp/dpkg/lock"

# Optional: a real finished build (debs + SHA256SUMS) in
# $KERNEL_POP_TEST_REAL_DEBS is staged and verified.
if [[ -n "${KERNEL_POP_TEST_REAL_DEBS:-}" ]]; then
    s="$(mktemp -d)"
    out="$(PKEXEC_UID="$(id -u)" stage_verified_debs "$KERNEL_POP_TEST_REAL_DEBS" "$s" 2>&1)" \
        || bad "the real build did not verify: $out"
    echo "$out" | grep "Verified"
    rm -rf "$s"
fi

if (( fails )); then echo "$fails failure(s)"; exit 1; fi
echo "all privileged-install helper tests passed"
