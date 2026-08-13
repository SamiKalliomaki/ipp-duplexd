#!/bin/bash
# Build an Arch Linux package from the working tree.
# Result: packaging/arch/ipp-duplexd-<ver>-<rel>-<arch>.pkg.tar.zst
set -euo pipefail
cd "$(dirname "$0")/.."

ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tar czf "packaging/arch/ipp-duplexd-$ver.tar.gz" \
    --transform "s,^,ipp-duplexd-$ver/," \
    Cargo.toml Cargo.lock src README.md LICENSE \
    packaging/ipp-duplexd.service packaging/ipp-duplexd.conf.example

cd packaging/arch
makepkg -f "$@"
echo
echo "install with: sudo pacman -U packaging/arch/ipp-duplexd-$ver-*.pkg.tar.zst"
