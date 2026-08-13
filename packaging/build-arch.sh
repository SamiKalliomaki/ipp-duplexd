#!/bin/bash
# Build an Arch Linux package from the working tree.
# Result: packaging/arch/ipp-duplexd-<ver>-<rel>-<arch>.pkg.tar.zst
set -euo pipefail
cd "$(dirname "$0")/.."

# only for the closing hint; the PKGBUILD reads Cargo.toml for pkgver itself
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# leftovers from when the tarball name carried the version
rm -f packaging/arch/ipp-duplexd-*.tar.gz

tar czf "packaging/arch/ipp-duplexd.tar.gz" \
    --transform "s,^,ipp-duplexd/," \
    Cargo.toml Cargo.lock src README.md LICENSE \
    packaging/ipp-duplexd.service packaging/ipp-duplexd.conf.example

cd packaging/arch
makepkg -f -C "$@"
echo
echo "install with: sudo pacman -U packaging/arch/ipp-duplexd-$ver-*.pkg.tar.zst"
