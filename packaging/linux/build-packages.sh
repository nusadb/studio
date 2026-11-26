#!/bin/sh
# Build the Linux release and package it as a .deb and a portable .tar.gz.
#   packaging/linux/build-packages.sh            -> dist/
# Needs: cargo, dpkg-deb (the .deb is skipped without it).
set -eu
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH_DEB=$(dpkg --print-architecture 2>/dev/null || echo amd64)
ARCH=$(uname -m)
DATE=$(date -u +%Y-%m-%d)
DIST="$ROOT/dist"
# Debian requires "Name <email>"; set DEB_MAINTAINER to the team's real address when releasing.
MAINTAINER="${DEB_MAINTAINER:-NusaDB Studio maintainers <maintainers@nusadb.com>}"
STAGE="$DIST/stage"

cargo build --release --locked
BIN="$ROOT/target/release/nusadb-studio"

rm -rf "$STAGE"
mkdir -p "$STAGE/usr/bin" "$STAGE/usr/share/applications" "$STAGE/usr/share/metainfo" \
         "$STAGE/usr/share/doc/nusadb-studio"
install -m 0755 "$BIN" "$STAGE/usr/bin/nusadb-studio"
install -m 0644 packaging/linux/nusadb-studio.desktop "$STAGE/usr/share/applications/"
sed -e "s/@VERSION@/$VERSION/" -e "s/@DATE@/$DATE/" packaging/linux/com.nusadb.Studio.metainfo.xml \
    > "$STAGE/usr/share/metainfo/com.nusadb.Studio.metainfo.xml"
for size in 16 24 32 48 64 128 256 512; do
    dir="$STAGE/usr/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    install -m 0644 "packaging/icons/nusadb-studio-$size.png" "$dir/nusadb-studio.png"
done
mkdir -p "$STAGE/usr/share/icons/hicolor/scalable/apps"
install -m 0644 packaging/icons/nusadb-studio.svg "$STAGE/usr/share/icons/hicolor/scalable/apps/"
install -m 0644 README.md "$STAGE/usr/share/doc/nusadb-studio/"
install -m 0644 ui/fonts/OFL-Inter.txt ui/fonts/OFL-JetBrainsMono.txt "$STAGE/usr/share/doc/nusadb-studio/"

# Portable archive: the same tree, plus an install script for ~/.local.
TAR_DIR="$DIST/nusadb-studio-$VERSION-linux-$ARCH"
rm -rf "$TAR_DIR"
mkdir -p "$TAR_DIR"
cp -r "$STAGE/usr/." "$TAR_DIR/"
install -m 0755 packaging/linux/install-local.sh "$TAR_DIR/install.sh"
tar -C "$DIST" -czf "$DIST/nusadb-studio-$VERSION-linux-$ARCH.tar.gz" "$(basename "$TAR_DIR")"
rm -rf "$TAR_DIR"

if command -v dpkg-deb >/dev/null; then
    mkdir -p "$STAGE/DEBIAN"
    SIZE=$(du -sk "$STAGE/usr" | cut -f1)
    cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: nusadb-studio
Version: $VERSION
Section: database
Priority: optional
Architecture: $ARCH_DEB
Installed-Size: $SIZE
Depends: libc6, libgcc-s1, libfontconfig1, libfreetype6, libxkbcommon0, libxkbcommon-x11-0, libx11-6, libx11-xcb1, libxcursor1, libxi6, libxrandr2, libwayland-client0, libwayland-cursor0, libegl1 | libgl1
Recommends: gnome-keyring | kwalletmanager, xdg-desktop-portal
Maintainer: $MAINTAINER
Homepage: https://nusadb.com
Description: Desktop database manager for NusaDB
 SQL editor, editable data grid with import and export, table designer,
 ER diagram, live server metrics, roles and row-level security, and
 LISTEN/NOTIFY, in one native application.
CONTROL
    dpkg-deb --root-owner-group --build "$STAGE" "$DIST/nusadb-studio_${VERSION}_${ARCH_DEB}.deb" >/dev/null
fi
rm -rf "$STAGE"
ls -l "$DIST"
