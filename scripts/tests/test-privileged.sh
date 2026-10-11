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
FIXTURES="$HERE/../../src/testdata"
REAL_KEYRING="$HERE/../../data/kernel-ppa.gpg"

for v in 7.1.3-070103-generic 6.8.0-45-generic 6.10.0+rc1 a.b_c; do
    valid_kver "$v" || bad "valid_kver should accept [$v]"
done
for v in '*' '../..' '../../etc' 'a/b' '' ' ' '-x' '.x' '6.8*' 'a b' '..' 'a..b' $'a\nb' '$(id)' ';rm' '?' '[a]'; do
    valid_kver "$v" && bad "valid_kver should reject [$v]"
done

# dpkg-deb stub: "<file>" -> Package name is the file's basename up to '_'
dpkg-deb() { local b; b="$(basename "$2")"; echo "${b%%_*}"; }
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

deb_allowed "$tmp/linux-image-unsigned-1.2.3-generic_1_amd64.deb" || bad "deb_allowed image"
deb_allowed "$tmp/linux-modules-1.2.3-generic_1_amd64.deb"        || bad "deb_allowed modules"
deb_allowed "$tmp/linux-headers-1.2.3_1_all.deb"                  || bad "deb_allowed headers"
deb_allowed "$tmp/evil-pkg_1_amd64.deb"  && bad "deb_allowed should reject evil-pkg"
deb_allowed "$tmp/bash_1_amd64.deb"      && bad "deb_allowed should reject bash"

touch "$tmp/linux-headers-7.1.3-070103_1_all.deb" \
      "$tmp/linux-image-unsigned-7.1.3-070103-generic_1_amd64.deb" \
      "$tmp/linux-modules-7.1.3-070103-generic_1_amd64.deb"
got="$(kver_from_debs "$tmp")"
[[ "$got" == "7.1.3-070103-generic" ]] || bad "kver_from_debs got [$got]"

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

# ── Download verification (root side) ───────────────────────────────────
printf 'hash file\n' > "$tmp/ck"
cat > "$tmp/CHECKSUMS" <<EOF2
# Checksums-Sha1:
$(printf '%040d' 1)  linux-a_1_amd64.deb
# Checksums-Sha256:
$(printf 'A%.0s' {1..64})  linux-a_1_amd64.deb
$(printf 'b%.0s' {1..64}) *amd64/linux-b_1_all.deb
EOF2
[[ "$(checksum_for "$tmp/CHECKSUMS" linux-a_1_amd64.deb)" == "$(printf 'a%.0s' {1..64})" ]] || bad "checksum_for sha256 (lowercased)"
[[ "$(checksum_for "$tmp/CHECKSUMS" linux-b_1_all.deb)" == "$(printf 'b%.0s' {1..64})" ]] || bad "checksum_for with a path prefix"
[[ -z "$(checksum_for "$tmp/CHECKSUMS" linux-c_1_amd64.deb)" ]] || bad "checksum_for an unlisted file"

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

# The real Ubuntu kernel PPA signature over a real mainline CHECKSUMS
if ! command -v gpgv >/dev/null 2>&1; then
    skip "gpgv not installed — signature tests"
else
    mkdir -p "$tmp/real"
    cp "$FIXTURES/v6.17-amd64-CHECKSUMS" "$tmp/real/CHECKSUMS"
    cp "$FIXTURES/v6.17-amd64-CHECKSUMS.gpg" "$tmp/real/CHECKSUMS.gpg"
    KERNEL_POP_KEYRING="$REAL_KEYRING" checksums_signature_ok "$tmp/real" >/dev/null || bad "the real CHECKSUMS signature should verify"
    [[ -n "$(compgen -G "$tmp/real/.gnupg.*")" ]] && bad "checksums_signature_ok left its gpg home behind"
    KERNEL_POP_KEYRING="$REAL_KEYRING" KERNEL_POP_SIGNER_FPR=0000000000000000000000000000000000000000 \
        checksums_signature_ok "$tmp/real" >/dev/null && bad "a signature from another key than the pinned one should be refused"
    echo "extra line" >> "$tmp/real/CHECKSUMS"
    KERNEL_POP_KEYRING="$REAL_KEYRING" checksums_signature_ok "$tmp/real" >/dev/null && bad "a tampered CHECKSUMS should not verify"
    KERNEL_POP_KEYRING="$tmp/no-such-keyring" checksums_signature_ok "$tmp/real" >/dev/null && bad "a missing keyring should fail"
fi

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
            mokutil() {
                echo "ARGS: $*" >> "$calls"
                case "$1" in
                    --test-key) echo "$2 is not enrolled" ;;
                    --generate-hash)
                        local a b; IFS= read -r a; IFS= read -r b
                        [[ "$a" == "$b" ]] || return 1
                        echo "input password:"; echo '$6$teststalt$fakehash' ;;
                    --import)
                        [[ "$3" == "--hash-file" && "$(stat -c '%a' "$4")" == "600" ]] || return 1
                        cat "$4" >> "$calls"; echo "$4" > "$tmp/hashfile-path" ;;
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
            grep -q -- "--import $KERNEL_POP_MOK_DIR/signing.der --hash-file" "$calls" || { echo "FAIL: mokutil --import call"; r=1; }
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
    fi
fi
unset KERNEL_POP_MOK_DIR KERNEL_POP_SIGNED_DIR KERNEL_POP_BOOT

# ── Staging and verifying a download, then do_install end to end ────────
# A throwaway gpg key stands in for Ubuntu's kernel PPA key; every system
# command do_install runs is stubbed, and every path points into $tmp.
if ! command -v gpg >/dev/null 2>&1 || ! command -v gpgv >/dev/null 2>&1; then
    skip "gpg/gpgv not installed — download verification and do_install tests"
else
    export GNUPGHOME="$tmp/gnupg"
    mkdir -p "$GNUPGHOME"; chmod 0700 "$GNUPGHOME"
    gpg --batch --pinentry-mode loopback --passphrase '' \
        --quick-gen-key 'Kernel Pop test <test@invalid>' rsa2048 sign never >/dev/null 2>&1 \
        || bad "could not create the throwaway gpg key"
    test_fpr="$(gpg --with-colons --list-keys 2>/dev/null | grep -m1 '^fpr' | cut -d: -f10)"
    gpg --export > "$tmp/test-ring.gpg" 2>/dev/null
    export KERNEL_POP_KEYRING="$tmp/test-ring.gpg" KERNEL_POP_SIGNER_FPR="$test_fpr"
    PKEXEC_UID="$(id -u)"; export PKEXEC_UID
    kv=7.7.7-070707-generic
    make_download() {   # <dir>: a signed, consistent download of fake debs
        local d="$1" f
        rm -rf "$d"; mkdir -p "$d"; chmod 0755 "$d"
        for f in "linux-headers-7.7.7-070707_7.7.7-070707.1_all.deb" \
                 "linux-image-unsigned-${kv}_7.7.7-070707.1_amd64.deb" \
                 "linux-modules-${kv}_7.7.7-070707.1_amd64.deb"; do
            echo "contents of $f" > "$d/$f"; chmod 0644 "$d/$f"
        done
        (cd "$d" && { echo "# Checksums-Sha256:"; sha256sum ./*.deb | sed 's| \./| |'; } > CHECKSUMS)
        gpg --batch --yes --detach-sign -o "$d/CHECKSUMS.gpg" "$d/CHECKSUMS" 2>/dev/null
        chmod 0644 "$d/CHECKSUMS" "$d/CHECKSUMS.gpg"
    }
    stage_ok() { local s; s="$(mktemp -d)"; (stage_verified_debs "$1" "$s") >"$tmp/stage-out" 2>&1; local r=$?; rm -rf "$s"; return $r; }

    make_download "$tmp/dl"
    stage_ok "$tmp/dl" || bad "stage_verified_debs should accept a good download: $(cat "$tmp/stage-out")"
    s="$(mktemp -d)"; (stage_verified_debs "$tmp/dl" "$s") >/dev/null 2>&1
    [[ "$(ls "$s" | sort | tr '\n' ' ')" == "CHECKSUMS CHECKSUMS.gpg linux-headers-7.7.7-070707_7.7.7-070707.1_all.deb linux-image-unsigned-${kv}_7.7.7-070707.1_amd64.deb linux-modules-${kv}_7.7.7-070707.1_amd64.deb " ]] \
        || bad "stage_verified_debs copied [$(ls "$s" | tr '\n' ' ')]"
    rm -rf "$s"
    echo tampered >> "$tmp/dl/linux-modules-${kv}_7.7.7-070707.1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a deb that does not match its hash"
    grep -q "SHA256 mismatch" "$tmp/stage-out" || bad "a hash mismatch should say so: $(cat "$tmp/stage-out")"
    make_download "$tmp/dl"; echo extra > "$tmp/dl/linux-extra_1_amd64.deb"; chmod 0644 "$tmp/dl/linux-extra_1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a deb missing from CHECKSUMS"
    grep -q "not listed in the signed CHECKSUMS" "$tmp/stage-out" || bad "an unlisted deb should say so"
    make_download "$tmp/dl"; rm "$tmp/dl/CHECKSUMS.gpg"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a download without CHECKSUMS.gpg"
    make_download "$tmp/dl"; echo "$(printf '0%.0s' {1..64})  linux-x.deb" >> "$tmp/dl/CHECKSUMS"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a CHECKSUMS changed after signing"
    make_download "$tmp/dl"; chmod 0664 "$tmp/dl/linux-modules-${kv}_7.7.7-070707.1_amd64.deb"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a group-writable deb"
    make_download "$tmp/dl"; chmod 0775 "$tmp/dl"
    stage_ok "$tmp/dl" && bad "stage_verified_debs should refuse a group-writable directory"
    make_download "$tmp/dl"
    (KERNEL_POP_SIGNER_FPR=0000000000000000000000000000000000000000 stage_ok "$tmp/dl") && bad "stage_verified_debs should refuse a signature by another key"

    # do_install, with the boot loader, dpkg and friends stubbed.
    running="$(uname -r)"
    run_install() {   # <scenario-name> [extra settings via env]; prints the script output
        local root="$tmp/inst-$1"
        rm -rf "$root"; mkdir -p "$root/boot" "$root/modules" "$root/staging" "$root/mok"
        make_download "$root/dl"
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
            apt-get() { echo "$*" >> "$root/apt-args"; [[ "$1" == "purge" ]]; }
            update-initramfs() { touch "$KERNEL_POP_BOOT/initrd.img-$3"; }   # -c|-u -k <kver>
            if [[ -n "${DKMS_BROKEN:-}" ]]; then
                dkms() { [[ "$1" == "status" ]] && echo "nvidia/550.54, $kv, x86_64: built"; [[ "$1" == "status" ]]; }
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
    grep -q "Reboot when ready" <<< "$out" || bad "a bootable install should end with Reboot when ready"
    grep -q "BOOT PENDING" <<< "$out" && bad "no BOOT marker expected with Secure Boot off"
    [[ "$(count 'DKMS WARNING:' "$out")" == 1 ]] || bad "one broken module should give one DKMS WARNING line, got $(count 'DKMS WARNING:' "$out")"
    [[ "$(count 'DKMS SUMMARY:' "$out")" == 1 ]] || bad "a DKMS problem should give one DKMS SUMMARY line"
    while IFS= read -r f; do
        [[ "$f" == "$root/staging/kernel-pop-install."*/*.deb ]] || bad "dpkg installed $f, not a staged copy"
    done < "$root/dpkg-args"
    [[ "$(wc -l < "$root/dpkg-args")" == 3 ]] || bad "dpkg should get all three debs"
    [[ -z "$(ls -A "$root/staging")" ]] || bad "do_install left its staging directory behind"

    # GRUB_DISABLE_SUBMENU=y: the saved default is the bare entry id
    out="$(FLAT_GRUB=1 run_install flat)" || bad "do_install (flat GRUB menu) failed: $out"
    [[ "$(cat "$tmp/inst-flat/saved")" == "gnulinux-$kv-advanced-x" ]] || bad "flat menu saved default got $(cat "$tmp/inst-flat/saved")"

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
    [[ "$(cat "$root/apt-args" 2>/dev/null)" == "purge -y linux-headers-7.7.7-070707 linux-image-unsigned-$kv linux-modules-$kv" ]] \
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

    # A tampered download never reaches dpkg
    root="$tmp/inst-off"
    out="$(
        make_download "$root/dl"; echo bad >> "$root/dl/linux-image-unsigned-${kv}_7.7.7-070707.1_amd64.deb"
        rm -f "$root/dpkg-args"
        dpkg() { echo called > "$root/dpkg-args"; }
        KERNEL_POP_STAGING="$root/staging" do_install "$root/dl" 2>&1
    )" && bad "do_install should fail on a tampered download"
    [[ -e "$root/dpkg-args" ]] && bad "dpkg ran on a tampered download"
    [[ -z "$(ls -A "$root/staging")" ]] || bad "a refused install left its staging directory behind"

    gpgconf --homedir "$GNUPGHOME" --kill gpg-agent >/dev/null 2>&1 || true
    unset GNUPGHOME KERNEL_POP_KEYRING KERNEL_POP_SIGNER_FPR PKEXEC_UID
fi

# Optional: a real mainline download (debs + CHECKSUMS + CHECKSUMS.gpg) in
# $KERNEL_POP_TEST_REAL_DEBS is verified with the real keyring.
if [[ -n "${KERNEL_POP_TEST_REAL_DEBS:-}" ]]; then
    s="$(mktemp -d)"
    out="$(PKEXEC_UID="$(id -u)" KERNEL_POP_KEYRING="$REAL_KEYRING" stage_verified_debs "$KERNEL_POP_TEST_REAL_DEBS" "$s" 2>&1)" \
        || bad "the real download did not verify: $out"
    echo "$out" | grep "Verified"
    rm -rf "$s"
fi

if (( fails )); then echo "$fails failure(s)"; exit 1; fi
echo "all privileged-install helper tests passed"
