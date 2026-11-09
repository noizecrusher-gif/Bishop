#!/bin/sh
# Installs a Bishop package and the smoke test's needs (Xvfb, D-Bus) in a fresh distro
# container, as root:  packaging/ci-install.sh <dist-dir>
# The distro is read from /etc/os-release.
set -eu
dist=$(cd "$1" && pwd)
. /etc/os-release
case "$ID" in
    ubuntu|debian|linuxmint)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq "$dist"/bishop_*.deb xvfb dbus >/dev/null
        ;;
    fedora)
        dnf install -y -q "$dist"/bishop-*.rpm xorg-x11-server-Xvfb dbus-daemon dbus-tools
        ;;
    opensuse-tumbleweed|opensuse-leap)
        zypper --non-interactive --quiet install --allow-unsigned-rpm "$dist"/bishop-*.rpm \
            xorg-x11-server-Xvfb dbus-1-daemon tar gzip
        ;;
    arch)
        pacman -Syu --noconfirm --needed --quiet gtk4 libadwaita vte4 xorg-server-xvfb dbus >/dev/null
        tmp=$(mktemp -d)
        tar -C "$tmp" -xzf "$dist"/bishop-*-"$(uname -m)"-linux.tar.gz
        "$tmp"/bishop-*/install.sh
        ;;
    *)
        echo "unsupported distro: $ID" >&2
        exit 1
        ;;
esac
