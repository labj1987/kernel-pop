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
    echo "SKIP: openssl not installed — key generation and signing tests skipped"
else
    generate_signing_key >/dev/null || bad "generate_signing_key failed"
    signing_key_present || bad "signing_key_present should be true after generation"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR/signing.key")" == "600" ]] || bad "signing.key should be mode 600"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR/signing.pem")" == "644" ]] || bad "signing.pem should be mode 644"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR/signing.der")" == "644" ]] || bad "signing.der should be mode 644"
    [[ "$(stat -c '%a' "$KERNEL_POP_MOK_DIR")" == "755" ]] || bad "the MOK dir should be mode 755"
    certtext="$(openssl x509 -in "$KERNEL_POP_MOK_DIR/signing.pem" -noout -text)"
    grep -qF "$SIGN_OID_MODULE_ONLY" <<< "$certtext" && bad "certificate must not carry the module-only EKU"
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

    pe=""
    for f in /usr/lib/systemd/boot/efi/linuxx64.efi.stub /usr/lib/systemd/boot/efi/*.efi \
             /usr/lib/systemd/boot/efi/*.efi.stub /usr/lib/shim/*.efi; do
        if [[ -f "$f" && -r "$f" && "$(head -c 2 "$f" 2>/dev/null)" == "MZ" ]]; then pe="$f"; break; fi
    done
    if ! command -v sbsign >/dev/null 2>&1 || ! command -v sbverify >/dev/null 2>&1; then
        echo "SKIP: sbsign/sbverify not installed — kernel signing tests skipped"
    elif [[ -z "$pe" ]]; then
        echo "SKIP: no readable PE/EFI binary found — kernel signing tests skipped"
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

if (( fails )); then echo "$fails failure(s)"; exit 1; fi
echo "all privileged-install helper tests passed"
