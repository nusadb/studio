#!/bin/sh
# Build the Windows installer and portable .zip from Linux (cross-compiled with MinGW-w64).
#   packaging/windows/build-installer.sh        -> dist/
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64, nsis, zip.
#   rustup target add x86_64-pc-windows-gnu && sudo apt install mingw-w64 nsis zip
# Where rustup cannot download the Windows target, set BUILD_STD=1 to compile the standard
# library from the rust-src component instead (needs rust-src matching the compiler).
set -eu
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET=x86_64-pc-windows-gnu
DIST="$ROOT/dist"
mkdir -p "$DIST"

export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
if [ "${BUILD_STD:-0}" = 1 ]; then
    RUSTC_BOOTSTRAP=1 cargo build --release --locked --target "$TARGET" -Zbuild-std=std,panic_abort
else
    cargo build --release --locked --target "$TARGET"
fi
EXE="$ROOT/target/$TARGET/release/nusadb-studio.exe"

# Portable: unzip anywhere and run.
PORTABLE="nusadb-studio-$VERSION-windows-x64"
STAGE=$(mktemp -d)
mkdir -p "$STAGE/$PORTABLE/licenses"
cp "$EXE" README.md "$STAGE/$PORTABLE/"
cp ui/fonts/OFL-*.txt "$STAGE/$PORTABLE/licenses/"
rm -f "$DIST/$PORTABLE.zip"
(cd "$STAGE" && zip -qr "$DIST/$PORTABLE.zip" "$PORTABLE")
rm -rf "$STAGE"

# Installer.
(cd packaging/windows && makensis -V2 -DVERSION="$VERSION" -DEXE="$EXE" -DOUTDIR="$DIST" nusadb-studio.nsi)
ls -l "$DIST"
