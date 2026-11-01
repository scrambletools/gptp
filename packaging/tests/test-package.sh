#!/bin/sh
# Installs a gptpd package in a fresh container, as someone would, checks
# what it puts where and that gptpd runs, then that removing it leaves
# nothing behind. run.sh starts it:
#
#   packaging/tests/test-package.sh /packages/gptpd_0.1.0-1_amd64.deb
set -eu

package=${1:?usage: test-package.sh PACKAGE}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

files="/usr/bin/gptpd
/usr/lib/systemd/system/gptpd@.service
/usr/share/polkit-1/rules.d/50-gptpd.rules
/usr/lib/sysusers.d/gptpd.conf
/usr/share/doc/gptpd/README.md
/usr/share/doc/gptpd/example.conf
/usr/share/licenses/gptpd/LICENSE-MIT
/usr/share/licenses/gptpd/LICENSE-APACHE"

# Ubuntu's and Arch's images leave /usr/share/doc out of every install,
# where a full system doesn't, so the test undoes that first.
case $package in
*.deb)
    sed -i '/^path-exclude/d' /etc/dpkg/dpkg.cfg.d/* 2>/dev/null || true
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$package"
    ;;
*.rpm) dnf install -y -q --setopt=install_weak_deps=False "$package" ;;
*.pkg.tar.*)
    sed -i '/^NoExtract/d' /etc/pacman.conf
    pacman -Syu --noconfirm -q && pacman -U --noconfirm "$package"
    ;;
*) fail "no package manager for $package" ;;
esac

for file in $files; do
    [ -e "$file" ] || fail "$file is missing"
done
[ -d /etc/gptpd ] || fail "/etc/gptpd is missing"
grep -q '^ExecStart=/usr/bin/gptpd ' /usr/lib/systemd/system/gptpd@.service ||
    fail "the unit does not run /usr/bin/gptpd"
if ldd /usr/bin/gptpd | grep 'not found'; then
    fail "gptpd misses a shared library"
fi
version=$(gptpd --version) || fail "gptpd --version failed"
echo "$version"
case $version in
"gptpd "[0-9]*) ;;
*) fail "gptpd --version said: $version" ;;
esac

case $package in
*.deb) DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq gptpd ;;
*.rpm) dnf remove -y -q gptpd ;;
*.pkg.tar.*) pacman -Rns --noconfirm gptpd ;;
esac
for file in $files /etc/gptpd; do
    [ -e "$file" ] && fail "$file is left after removal"
done
echo "PASS: $(basename "$package")"
