#!/usr/bin/env bash
# Runs test-package.sh in a fresh container of IMAGE, with Docker, under
# QEMU when the package's architecture is not the host's:
#
#   packaging/tests/run.sh IMAGE PACKAGE
#   packaging/tests/run.sh debian:13 dist/gptpd_0.1.0-1_amd64.deb
set -euo pipefail

image=${1:?usage: run.sh IMAGE PACKAGE}
package=$(readlink -f "${2:?usage: run.sh IMAGE PACKAGE}")
here=$(cd "$(dirname "$0")" && pwd)

case $(basename "$package") in
*_amd64.deb | *.x86_64.rpm | *-x86_64.pkg.tar.*) platform=amd64 ;;
*_arm64.deb | *.aarch64.rpm | *-aarch64.pkg.tar.*) platform=arm64 ;;
*_riscv64.deb | *.riscv64.rpm | *-riscv64.pkg.tar.*) platform=riscv64 ;;
*) echo "run.sh: no architecture in $(basename "$package")" >&2; exit 2 ;;
esac

docker run --rm --platform "linux/$platform" \
    -v "$here:/tests:ro" -v "$package:/packages/$(basename "$package"):ro" \
    "$image" /tests/test-package.sh "/packages/$(basename "$package")"
