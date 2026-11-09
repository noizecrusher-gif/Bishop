#!/bin/sh
# Builds the portable tarball from a release build:
#   packaging/make-tarball.sh <arch>   ->  target/dist/bishop-<version>-<arch>-linux.tar.gz
set -eu
cd "$(dirname "$0")/.."
arch=${1:-$(uname -m)}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
id=io.github.noizecrusher_gif.Bishop
name=bishop-$version-$arch-linux
stage=target/dist/$name
rm -rf "$stage"
install -D -m 755 target/release/bishop "$stage/bin/bishop"
install -D -m 644 data/$id.desktop "$stage/share/applications/$id.desktop"
install -D -m 644 data/$id.metainfo.xml "$stage/share/metainfo/$id.metainfo.xml"
for dir in data/icons/hicolor/*/apps; do
    size=$(basename "$(dirname "$dir")")
    install -D -m 644 "$dir/$id.png" "$stage/share/icons/hicolor/$size/apps/$id.png"
done
install -m 755 packaging/install.sh "$stage/install.sh"
install -m 644 README.md LICENSE "$stage/"
tar -C target/dist -czf "target/dist/$name.tar.gz" "$name"
echo "target/dist/$name.tar.gz"
