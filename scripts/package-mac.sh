#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) ebobo.app, signs it with Developer ID, notarizes and
# staples it, and packs dist/ebobo-mac.zip. NOTARIZE=0 skips notarization (local testing).
set -euo pipefail
cd "$(dirname "$0")/.."

# Signing identity and App Store Connect API key (for notarytool) come from the environment,
# falling back to the local app-store-connect-mcp profile. Nothing secret lives in the repo.
identity="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1)}"
asc_profile="$HOME/.config/app-store-connect-mcp/config.toml"
asc_get() { sed -n "s/^$1 = '\(.*\)'/\1/p" "$asc_profile" 2>/dev/null | head -n 1; }
asc_key="${ASC_KEY:-$(asc_get key_path)}"
asc_key_id="${ASC_KEY_ID:-$(asc_get key_id)}"
asc_issuer="${ASC_ISSUER:-$(asc_get issuer_id)}"
if [ -z "$identity" ]; then echo "no Developer ID Application identity in the keychain (set SIGN_IDENTITY)" >&2; exit 1; fi
if [ "${NOTARIZE:-1}" = "1" ] && { [ -z "$asc_key" ] || [ -z "$asc_key_id" ] || [ -z "$asc_issuer" ]; }; then
    echo "set ASC_KEY, ASC_KEY_ID, ASC_ISSUER (or NOTARIZE=0)" >&2; exit 1
fi
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"

export MACOSX_DEPLOYMENT_TARGET=11.0
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p ebobo --locked --target "$target"
done

app=dist/ebobo.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
tdir="${CARGO_TARGET_DIR:-target}"
lipo -create -output "$app/Contents/MacOS/ebobo" \
    "$tdir/aarch64-apple-darwin/release/ebobo" \
    "$tdir/x86_64-apple-darwin/release/ebobo"
if [ -f packaging/icon/ebobo.icns ]; then
    cp packaging/icon/ebobo.icns "$app/Contents/Resources/ebobo.icns"
fi

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>ebobo</string>
    <key>CFBundleDisplayName</key><string>ebobo</string>
    <key>CFBundleIdentifier</key><string>se.jvcr.ebobo</string>
    <key>CFBundleExecutable</key><string>ebobo</string>
    <key>CFBundleIconFile</key><string>ebobo</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.games</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>GCSupportsControllerUserInteraction</key><true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>NES ROM</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>CFBundleTypeExtensions</key><array><string>nes</string></array>
        </dict>
    </array>
</dict>
</plist>
EOF

codesign --force --options runtime --timestamp -s "$identity" "$app"
codesign --verify --strict --verbose=2 "$app"

# --norsrc --noextattr: no AppleDouble `._*` entries in the zip. Finder merges them back, but
# `unzip` writes them into the bundle as files and the signature then fails ("sealed resource").
zip=dist/ebobo-mac.zip
pack() { rm -f "$zip"; ditto -c -k --norsrc --noextattr --keepParent "$app" "$zip"; }
pack

if [ "${NOTARIZE:-1}" = "1" ]; then
    xcrun notarytool submit "$zip" --key "$asc_key" --key-id "$asc_key_id" --issuer "$asc_issuer" --wait
    xcrun stapler staple "$app"
    pack
    spctl --assess --type execute --verbose "$app"
fi
# The zip must verify after a plain `unzip`, not just after Finder's Archive Utility.
check="$(mktemp -d)"
unzip -q "$zip" -d "$check"
codesign --verify --deep --strict "$check/ebobo.app"
rm -rf "$check"
echo "$zip"
