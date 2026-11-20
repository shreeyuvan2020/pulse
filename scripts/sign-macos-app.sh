#!/usr/bin/env bash
# Signs Pulse.app with a Developer ID certificate, notarizes it with Apple and
# staples the ticket, so Gatekeeper opens it without a warning. Runs in the
# release workflow on a macOS runner.
#
#   scripts/sign-macos-app.sh <Pulse.app>
#
# Environment (all required; see docs/signing.md):
#   MACOS_CERTIFICATE_P12       base64 of the Developer ID Application .p12
#   MACOS_CERTIFICATE_PASSWORD  the .p12 password
#   APPLE_API_KEY_P8            base64 of the App Store Connect API key (.p8)
#   APPLE_API_KEY_ID            that key's ID
#   APPLE_API_ISSUER_ID         the issuer ID shown next to the keys list
set -euo pipefail

APP="${1:?pass the .app path}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTITLEMENTS="$ROOT/packaging/macos/entitlements.plist"

: "${MACOS_CERTIFICATE_P12:?}"
: "${MACOS_CERTIFICATE_PASSWORD:?}"
: "${APPLE_API_KEY_P8:?}"
: "${APPLE_API_KEY_ID:?}"
: "${APPLE_API_ISSUER_ID:?}"

WORK="$(mktemp -d)"
KEYCHAIN="$WORK/pulse-signing.keychain-db"
KEYCHAIN_PASSWORD="$(uuidgen)"
ORIGINAL_KEYCHAINS="$(security list-keychains -d user | tr -d '"')"

cleanup() {
  # shellcheck disable=SC2086
  security list-keychains -d user -s $ORIGINAL_KEYCHAINS 2>/dev/null || true
  security delete-keychain "$KEYCHAIN" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "sign: importing the certificate into a temporary keychain"
printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$WORK/cert.p12"
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security set-keychain-settings -lut 3600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security import "$WORK/cert.p12" -P "$MACOS_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12 -k "$KEYCHAIN"
security set-key-partition-list -S apple-tool:,apple: -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
# shellcheck disable=SC2086
security list-keychains -d user -s "$KEYCHAIN" $ORIGINAL_KEYCHAINS

IDENTITY="$(security find-identity -p codesigning "$KEYCHAIN" \
  | grep "Developer ID Application" | head -1 | sed -E 's/.*"(.*)".*/\1/' || true)"
[ -n "$IDENTITY" ] || { echo "sign: no Developer ID Application identity in the .p12" >&2; exit 1; }
echo "sign: signing as $IDENTITY"

# hardened runtime is required for notarization
codesign --force --timestamp --options runtime \
  --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" "$APP"
codesign --verify --strict --verbose=2 "$APP"

echo "sign: submitting to Apple for notarization (usually a few minutes)"
ditto -c -k --keepParent "$APP" "$WORK/Pulse.zip"
printf '%s' "$APPLE_API_KEY_P8" | base64 --decode > "$WORK/AuthKey.p8"
xcrun notarytool submit "$WORK/Pulse.zip" \
  --key "$WORK/AuthKey.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" \
  --wait --timeout 30m

# the ticket goes into the .app so it also opens offline
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP"
echo "sign: $APP is signed, notarized and stapled"
