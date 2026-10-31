#!/bin/sh
# Installs gptpd's release build into a tree, as the packages lay it out:
#
#   scripts/dist-install.sh TARGET_DIR DESTDIR PREFIX
#
# TARGET_DIR holds the gptpd binary (target/release), DESTDIR is the root
# to install under, and PREFIX is where the binary goes (/usr).
set -eu
if [ $# -ne 3 ]; then
    echo "usage: $0 TARGET_DIR DESTDIR PREFIX" >&2
    exit 2
fi
target=$1
root=$2
prefix=$3
here=$(dirname "$0")/..
install -D -m 0755 "$target/gptpd" "$root$prefix/bin/gptpd"
# The end station's unit, run from where the package puts the binary.
install -d "$root/usr/lib/systemd/system"
sed "s|/usr/local/bin/gptpd|$prefix/bin/gptpd|" "$here/config/systemd/gptpd@.service" \
    >"$root/usr/lib/systemd/system/gptpd@.service"
chmod 0644 "$root/usr/lib/systemd/system/gptpd@.service"
install -D -m 0644 "$here/packaging/linux/50-gptpd.rules" \
    "$root/usr/share/polkit-1/rules.d/50-gptpd.rules"
install -D -m 0644 "$here/packaging/linux/gptpd.sysusers" \
    "$root/usr/lib/sysusers.d/gptpd.conf"
install -d -m 0755 "$root/etc/gptpd"
install -D -m 0644 "$here/packaging/linux/example.conf" \
    "$root/usr/share/doc/gptpd/example.conf"
# A bridge's unit, as an example: its ports and LocalClock are the bridge's.
install -D -m 0644 "$here/config/systemd/gptpd.service" \
    "$root/usr/share/doc/gptpd/examples/gptpd.service"
install -D -m 0644 "$here/README.md" "$root/usr/share/doc/gptpd/README.md"
install -D -m 0644 "$here/LICENSE-MIT" "$root/usr/share/licenses/gptpd/LICENSE-MIT"
install -D -m 0644 "$here/LICENSE-APACHE" "$root/usr/share/licenses/gptpd/LICENSE-APACHE"
