# Releasing

gptp and raw-ether go to crates.io; gptpd, which isn't published there,
ships as packages from this repository's GitHub releases, and triib's and
plaa's releases carry the same files.

1. Update the versions: each crate's `Cargo.toml`, the workspace
   dependencies in the root `Cargo.toml`, and `pkgver` in
   `packaging/arch/gptpd/PKGBUILD`, which must match gptpd's. Run
   `cargo check` so `Cargo.lock` follows.
2. Write the version's section in `CHANGELOG.md`, as `## [X.Y.Z]`; the
   release notes are taken from it.
3. Try the packaging without releasing: run the Release workflow by hand
   (`gh workflow run release.yml`). It builds the .deb and .rpm for
   x86_64, ARM64 and RISC-V, installs, runs and removes each in fresh
   containers of Debian 13, Ubuntu 24.04 and 26.04 and Fedora 44, and
   builds and tests the PKGBUILD in a fresh Arch container.
4. Publish the crates that changed, gptp before raw-ether's users:
   `cargo publish -p gptp`, then `cargo publish -p raw-ether`.
5. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`, where X.Y.Z
   is gptpd's version. The workflow checks the tag against it, builds and
   tests the packages, and drafts a release with them, `SHA256SUMS`, and
   a `PKGBUILD` with the source checksum filled in. Check the draft and
   publish it, then attach the same files to the next triib and plaa
   releases.

To test a package by hand, with Docker:
`packaging/tests/run.sh debian:13 dist/gptpd_X.Y.Z-1_amd64.deb`.
