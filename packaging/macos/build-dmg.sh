#!/bin/sh
# Build a universal (Apple silicon + Intel) NusaDB Studio.app and a .dmg. Run on macOS.
#   packaging/macos/build-dmg.sh               -> dist/
# Signing is optional: set CODESIGN_IDENTITY ("Developer ID Application: …") to sign, and
# NOTARY_PROFILE (a `xcrun notarytool store-credentials` profile) to notarize as well.
set -eu
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST="$ROOT/dist"
APP="$DIST/NusaDB Studio.app"

rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --locked --target x86_64-apple-darwin

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/nusadb-studio" \
    target/aarch64-apple-darwin/release/nusadb-studio \
    target/x86_64-apple-darwin/release/nusadb-studio
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

ICONSET=$(mktemp -d)/nusadb-studio.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    cp "packaging/icons/nusadb-studio-$size.png" "$ICONSET/icon_${size}x${size}.png"
done
cp packaging/icons/nusadb-studio-32.png "$ICONSET/icon_16x16@2x.png"
cp packaging/icons/nusadb-studio-64.png "$ICONSET/icon_32x32@2x.png"
cp packaging/icons/nusadb-studio-256.png "$ICONSET/icon_128x128@2x.png"
cp packaging/icons/nusadb-studio-512.png "$ICONSET/icon_256x256@2x.png"
sips -z 1024 1024 packaging/icons/nusadb-studio-512.png --out "$ICONSET/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/nusadb-studio.icns"

if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$APP"
else
    codesign --force --sign - "$APP"   # ad-hoc: runs locally, Gatekeeper still asks once
fi

DMG="$DIST/NusaDB-Studio-$VERSION-macos-universal.dmg"
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "NusaDB Studio" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
if [ -n "${CODESIGN_IDENTITY:-}" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
    codesign --sign "$CODESIGN_IDENTITY" "$DMG"
    xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
fi
ls -l "$DIST"
