#!/bin/sh
# Install NusaDB Studio for the current user only (no root): ~/.local/bin and ~/.local/share.
# Run it from the unpacked archive. Remove with: ./install.sh --uninstall
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PREFIX="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN="$HOME/.local/bin"
if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN/nusadb-studio" "$PREFIX/applications/nusadb-studio.desktop" \
          "$PREFIX/metainfo/com.nusadb.Studio.metainfo.xml"
    find "$PREFIX/icons/hicolor" -name 'nusadb-studio.*' -delete 2>/dev/null || true
    # The shared "Open with" cache must stop naming the removed application.
    command -v update-desktop-database >/dev/null && update-desktop-database "$PREFIX/applications" || true
    echo "NusaDB Studio removed. Saved servers stay in ~/.config/nusadb-studio."
    exit 0
fi
mkdir -p "$BIN" "$PREFIX"
install -m 0755 "$HERE/bin/nusadb-studio" "$BIN/nusadb-studio"
cp -r "$HERE/share/applications" "$HERE/share/icons" "$HERE/share/metainfo" "$PREFIX/"
sed -i "s|^Exec=nusadb-studio|Exec=$BIN/nusadb-studio|" "$PREFIX/applications/nusadb-studio.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$PREFIX/applications" || true
# Only a theme with an index has a cache to refresh.
if [ -f "$PREFIX/icons/hicolor/index.theme" ] && command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -q "$PREFIX/icons/hicolor" || true
fi
echo "NusaDB Studio installed to $BIN/nusadb-studio — it is in your application menu."
