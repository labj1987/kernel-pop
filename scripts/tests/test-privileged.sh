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

# ── GRUB entry lookup from a fixture grub.cfg ────────────────────────────
cat > "$tmp/grub.cfg" <<'EOF'
menuentry 'Ubuntu' --class ubuntu $menuentry_id_option 'gnulinux-simple-abc' {
}
submenu 'Advanced options for Ubuntu' $menuentry_id_option 'gnulinux-advanced-abc' {
	menuentry 'Ubuntu, with Linux 7.1.3-070103-generic' --class ubuntu $menuentry_id_option 'gnulinux-7.1.3-070103-generic-advanced-abc' {
	}
	menuentry 'Ubuntu, with Linux 7.1.3-070103-generic (recovery mode)' --class ubuntu $menuentry_id_option 'gnulinux-7.1.3-070103-generic-recovery-abc' {
	}
	menuentry 'Ubuntu, with Linux 6.8.0-45-generic' --class ubuntu $menuentry_id_option 'gnulinux-6.8.0-45-generic-advanced-abc' {
	}
}
EOF
got="$(KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 7.1.3-070103-generic)"
[[ "$got" == "gnulinux-advanced-abc>gnulinux-7.1.3-070103-generic-advanced-abc" ]] || bad "grub_entry_path_for_kver got [$got]"
KERNEL_POP_GRUB_CFG="$tmp/grub.cfg" grub_entry_path_for_kver 9.9.9-generic >/dev/null && bad "grub_entry_path_for_kver should fail for an unknown kernel"

# restore_grub_default with no previous default is a no-op that succeeds
restore_grub_default "" || bad "restore_grub_default empty should succeed"

if (( fails )); then echo "$fails failure(s)"; exit 1; fi
echo "all privileged-install helper tests passed"
