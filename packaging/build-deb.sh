#!/bin/bash
# Build a Debian/Ubuntu binary package from the working tree.
# Needs: cargo, dpkg-deb. Result: packaging/ipp-duplexd_<ver>-1_<arch>.deb
set -euo pipefail
cd "$(dirname "$0")/.."

ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(dpkg --print-architecture 2>/dev/null || echo amd64)

cargo build --release --locked

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
chmod 755 "$stage"

install -Dm755 target/release/ipp-duplexd "$stage/usr/bin/ipp-duplexd"
install -Dm755 target/release/ipp-duplexd-setup "$stage/usr/bin/ipp-duplexd-setup"
install -Dm644 packaging/ipp-duplexd.service "$stage/usr/lib/systemd/user/ipp-duplexd.service"
install -Dm644 packaging/ipp-duplexd.conf.example "$stage/usr/share/doc/ipp-duplexd/ipp-duplexd.conf.example"
install -Dm644 README.md "$stage/usr/share/doc/ipp-duplexd/README.md"

# debian copyright file from LICENSE
install -Dm644 /dev/stdin "$stage/usr/share/doc/ipp-duplexd/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: ipp-duplexd

Files: *
License: MIT
$(sed 's/^/ /;s/^ $/ ./' LICENSE)
EOF

gzip -9n <<EOF > "$stage/usr/share/doc/ipp-duplexd/changelog.gz"
ipp-duplexd ($ver-1) unstable; urgency=medium

  * Initial release.

 -- Sami Kalliomäki <sami@kalliomaki.me>  $(date -R)
EOF

mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: ipp-duplexd
Version: $ver-1
Section: net
Priority: optional
Architecture: $arch
Depends: qpdf, libc6
Suggests: cups-client, zenity
Installed-Size: $(du -sk "$stage" --exclude=DEBIAN | cut -f1)
Maintainer: Sami Kalliomäki <sami@kalliomaki.me>
Description: Manual duplex printing via a loopback virtual IPP printer
 ipp-duplexd is a virtual IPP printer listening on 127.0.0.1 that turns a
 simplex printer into a manual duplex printer: it prints the odd pages
 on the real printer, pauses (media-needed) so the user can flip the
 stack, then prints the even pages in reverse order on the backs.
 .
 Register it in CUPS as a driverless queue:
 lpadmin -p NAME -E -v ipp://127.0.0.1:6632/ipp/print -m everywhere
EOF

out="packaging/ipp-duplexd_${ver}-1_${arch}.deb"
dpkg-deb --build --root-owner-group "$stage" "$out"
echo
echo "install with: sudo apt install ./$out"
