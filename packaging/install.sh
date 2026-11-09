#!/bin/sh
# Installs Bishop from this folder.
#
#   ./install.sh              for yourself, into ~/.local   (no root needed)
#   sudo ./install.sh         for everyone, into /usr/local
#   ./install.sh --uninstall  removes it again (with sudo if it was installed with sudo)
#
# Bishop needs GTK 4.14+, libadwaita 1.5+ and VTE 0.76+ (the GTK 4 build) from your
# distribution; see README.md.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
id=io.github.noizecrusher_gif.Bishop
if [ "$(id -u)" = 0 ]; then prefix=${PREFIX:-/usr/local}; else prefix=${PREFIX:-$HOME/.local}; fi
share=$prefix/share

files="bin/bishop share/applications/$id.desktop share/metainfo/$id.metainfo.xml"
for icon in "$here"/share/icons/hicolor/*/apps/$id.png; do
    files="$files ${icon#"$here"/}"
done

refresh() {
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$share/icons/hicolor" 2>/dev/null || true
    command -v update-desktop-database >/dev/null && update-desktop-database -q "$share/applications" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    for f in $files; do rm -f "$prefix/$f"; done
    refresh
    echo "Bishop removed from $prefix."
    exit 0
fi

for f in $files; do
    install -D -m "$( [ "$f" = bin/bishop ] && echo 755 || echo 644 )" "$here/$f" "$prefix/$f"
done
# Outside the system prefixes, the launcher may not find `bishop` on PATH: use the full path.
case $prefix in
    /usr|/usr/local) ;;
    *) sed -i "s|^Exec=bishop|Exec=$prefix/bin/bishop|" "$prefix/share/applications/$id.desktop" ;;
esac
refresh
echo "Bishop installed in $prefix. Open it from your applications menu, or run $prefix/bin/bishop."
case :$PATH: in *":$prefix/bin:"*) ;; *) echo "Note: $prefix/bin is not on your PATH." ;; esac
