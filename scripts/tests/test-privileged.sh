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

if (( fails )); then echo "$fails failure(s)"; exit 1; fi
echo "all privileged-install helper tests passed"
