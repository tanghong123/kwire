#!/usr/bin/env bash
# Build a Developer-ID-signed + notarized universal Kwire.app and (optionally)
# upload it to a GitHub release as Kwire_universal.app.tar.gz — the asset the
# Homebrew cask installs.
#
# Signing + notarization run LOCALLY: the "Developer ID Application" certificate
# and the notarytool keychain profile live in the maintainer's login keychain,
# NOT in CI. CI (release.yml) only builds the UNSIGNED draft; this script
# replaces its .app.tar.gz with the signed one before the release is published.
#
# Usage:
#   scripts/sign-macos-app.sh [<tag>]
#     scripts/sign-macos-app.sh v2.4.0   # build+sign+notarize, upload --clobber
#     scripts/sign-macos-app.sh          # build+sign+notarize only (no upload)
#
# Env overrides:
#   SIGN_IDENTITY   default "Developer ID Application: Hong Tang (L95F5S5Y9R)"
#   NOTARY_PROFILE  default "ytdl-notarize"  (xcrun notarytool store-credentials)
set -euo pipefail
cd "$(dirname "$0")/.."

SIGN_IDENTITY="${SIGN_IDENTITY:-Developer ID Application: Hong Tang (L95F5S5Y9R)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-ytdl-notarize}"
TAG="${1:-}"
APP="target/universal-apple-darwin/release/bundle/macos/Kwire.app"
OUT="dist/Kwire_universal.app.tar.gz"

command -v xcrun >/dev/null || { echo "error: xcrun (Xcode CLT) required" >&2; exit 1; }
security find-identity -p codesigning -v | grep -qF "$SIGN_IDENTITY" \
  || { echo "error: signing identity not in keychain: $SIGN_IDENTITY" >&2; exit 1; }

echo "==> Building signed universal Kwire.app"
# The rustup toolchain has both apple targets; Homebrew's rust lacks the x86_64
# std, so force ~/.cargo/bin ahead on PATH. Tauri signs during bundling when
# APPLE_SIGNING_IDENTITY is set (hardened runtime + correct entitlements).
( cd app && PATH="$HOME/.cargo/bin:$PATH" APPLE_SIGNING_IDENTITY="$SIGN_IDENTITY" \
    cargo tauri build --target universal-apple-darwin )

echo "==> Verifying signature"
codesign --verify --strict "$APP"

echo "==> Notarizing (uploads to Apple; waits for the result)"
ZIP="$(mktemp -d)/Kwire.zip"
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl -a -vvv -t install "$APP" 2>&1 | head -3

echo "==> Packaging $OUT"
mkdir -p dist; rm -f "$OUT"
tar -C "$(dirname "$APP")" -czf "$OUT" "$(basename "$APP")"
shasum -a 256 "$OUT"

if [ -n "$TAG" ]; then
  echo "==> Uploading to release $TAG (clobber)"
  gh release upload "$TAG" "$OUT" --clobber
fi
echo "Done."
