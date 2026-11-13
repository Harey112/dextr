#!/usr/bin/env bash
# Builds dist/dextr-<version>-macos.dmg: a universal (Apple Silicon + Intel)
# dextr.app with an Applications shortcut.
#
# Run on a Mac from anywhere in the repository:
#   packaging/macos/build-dmg.sh
#
# Signing (optional):
#   SIGN_IDENTITY  a code-signing identity in the keychain: "Developer ID
#                  Application: Your Name (TEAMID)", or a self-signed one.
#                  Any fixed identity keeps macOS permissions across updates;
#                  without it the app is signed ad hoc (permissions reset on
#                  every update).
#   NOTARY_PROFILE a notarytool keychain profile, made once with
#                  xcrun notarytool store-credentials <profile> \
#                    --apple-id you@example.com --team-id TEAMID
#                  (or set APPLE_ID, APPLE_TEAM_ID and APPLE_APP_PASSWORD).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }

export MACOSX_DEPLOYMENT_TARGET=11.0
targets=(aarch64-apple-darwin x86_64-apple-darwin)
rustup target add "${targets[@]}" >/dev/null
for t in "${targets[@]}"; do
    echo "Building ${t}…"
    cargo build --release --locked -p dextr-app --target "$t"
done

dist="$root/dist"
app="$dist/dextr.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/dextr" \
    "target/aarch64-apple-darwin/release/dextr" "target/x86_64-apple-darwin/release/dextr"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp assets/dextr.icns "$app/Contents/Resources/dextr.icns"
printf 'APPL????' > "$app/Contents/PkgInfo"

# A secure timestamp is needed (and only possible) for notarization.
if [ -n "${NOTARY_PROFILE:-}${APPLE_ID:-}" ]; then timestamp=--timestamp; else timestamp=--timestamp=none; fi

if [ -n "${SIGN_IDENTITY:-}" ]; then
    echo "Signing with ${SIGN_IDENTITY}…"
    codesign --force --options runtime "$timestamp" --sign "$SIGN_IDENTITY" "$app"
else
    echo "No SIGN_IDENTITY: signing ad hoc."
    codesign --force --sign - "$app"
fi
codesign --verify --strict "$app"
codesign -d --requirements - "$app" 2>&1 | sed -n 's/^designated => /Designated requirement: /p'

dmg="$dist/dextr-$version-macos.dmg"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
hdiutil create -quiet -volname "dextr $version" -srcfolder "$stage" -fs HFS+ -format UDZO "$dmg"

if [ -n "${SIGN_IDENTITY:-}" ]; then
    codesign --force "$timestamp" --sign "$SIGN_IDENTITY" "$dmg"
    if [ -n "${NOTARY_PROFILE:-}" ]; then
        auth=(--keychain-profile "$NOTARY_PROFILE")
    elif [ -n "${APPLE_ID:-}" ]; then
        auth=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD")
    else
        auth=()
    fi
    if [ ${#auth[@]} -gt 0 ]; then
        echo "Notarizing (a few minutes)…"
        xcrun notarytool submit "$dmg" "${auth[@]}" --wait
        xcrun stapler staple "$dmg"
    else
        echo "No notary credentials: signed but not notarized."
    fi
fi

echo "Done: $dmg"
