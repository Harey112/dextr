#!/usr/bin/env bash
# Builds dextr's Linux packages into dist/:
#   dextr-<version>-linux-<arch>.tar.gz, .deb, .rpm, and (x86_64) .AppImage
#
#   packaging/linux/build.sh [x86_64|aarch64]
#
# Needs cargo-zigbuild (with zig), cargo-deb and cargo-generate-rpm. The
# binary is linked against glibc 2.31, so it runs on Ubuntu 20.04, Debian
# 11, Fedora 32 and anything newer; its other libraries (Wayland,
# xkbcommon, the GPU driver) are loaded at runtime.
set -euo pipefail

arch="${1:-x86_64}"
target="${arch}-unknown-linux-gnu"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
dist="$root/dist"
mkdir -p "$dist"

rustup target add "$target" >/dev/null
cargo zigbuild --release --locked -p dextr-app --target "${target}.2.31"
bin="target/${target}/release/dextr"

# Plain archive.
name="dextr-${version}-linux-${arch}"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/icons"
cp "$bin" "$stage/$name/dextr"
cp packaging/linux/dev.dextr.dextr.desktop packaging/linux/dev.dextr.dextr.metainfo.xml LICENSE README.md "$stage/$name/"
cp assets/icons/dextr-*.png "$stage/$name/icons/"
tar -C "$stage" -czf "$dist/$name.tar.gz" "$name"

# Debian/Ubuntu and Fedora/openSUSE packages.
cargo deb -p dextr-app --no-build --no-strip --target "$target" -o "$dist/"
cargo generate-rpm -p crates/app --target "$target" -o "$dist/"

# AppImage (x86_64 only for now).
if [ "$arch" = x86_64 ]; then
    tool="$stage/appimagetool"
    curl -fsSL -o "$tool" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
    chmod +x "$tool"
    appdir="$stage/dextr.AppDir"
    mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/metainfo"
    cp "$bin" "$appdir/usr/bin/dextr"
    cp packaging/linux/dev.dextr.dextr.desktop "$appdir/usr/share/applications/"
    cp packaging/linux/dev.dextr.dextr.metainfo.xml "$appdir/usr/share/metainfo/dev.dextr.dextr.appdata.xml"
    for n in 16 24 32 48 64 128 256 512; do
        mkdir -p "$appdir/usr/share/icons/hicolor/${n}x${n}/apps"
        cp "assets/icons/dextr-${n}.png" "$appdir/usr/share/icons/hicolor/${n}x${n}/apps/dev.dextr.dextr.png"
    done
    cp packaging/linux/dev.dextr.dextr.desktop "$appdir/"
    cp assets/icons/dextr-256.png "$appdir/dev.dextr.dextr.png"
    ln -s usr/bin/dextr "$appdir/AppRun"
    ARCH=x86_64 "$tool" --appimage-extract-and-run --no-appstream "$appdir" "$dist/dextr-${version}-x86_64.AppImage"
fi

ls -la "$dist"
