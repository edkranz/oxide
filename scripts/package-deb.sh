#!/bin/sh
# Builds a .deb from a release build: scripts/package-deb.sh BINARY VERSION ARCH OUT_DIR
# ARCH is a Debian architecture (amd64, arm64). The file name has no version, so
# releases/latest/download/oxide_ARCH.deb always points at the newest one.
set -eu

binary=$1 version=$2 arch=$3 out=$4
root=$(cd "$(dirname "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
chmod 755 "$stage"  # mktemp makes it private; this becomes the package's root

install -Dm755 "$binary" "$stage/usr/bin/oxide"
ln -s oxide "$stage/usr/bin/ox"
install -Dm644 "$root/packaging/linux/oxide.desktop" "$stage/usr/share/applications/oxide.desktop"
install -Dm644 "$root/packaging/linux/oxide.png" "$stage/usr/share/icons/hicolor/512x512/apps/oxide.png"
install -Dm644 "$root/LICENSE" "$stage/usr/share/doc/oxide/copyright"

mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<CONTROL
Package: oxide
Version: $version
Architecture: $arch
Maintainer: Eddie Kranz <edkranz@users.noreply.github.com>
Installed-Size: $(du -sk "$stage/usr" | cut -f1)
Depends: libc6 (>= 2.35), libgcc-s1
Recommends: libxkbcommon-x11-0, libwayland-client0, libwayland-cursor0, libx11-xcb1, libxcursor1
Section: editors
Priority: optional
Homepage: https://github.com/edkranz/oxide
Description: Super light text editor built in Rust
 oxide opens instantly, edits text well, and leaves the rest to add-ons:
 syntax highlighting, multiple cursors, find and replace, formatters, and a
 single running instance so `ox file` opens a tab in milliseconds.
CONTROL

mkdir -p "$out"
dpkg-deb --root-owner-group --build "$stage" "$out/oxide_${arch}.deb"
