#!/usr/bin/env bash
# Construye TransmitIA.app autocontenido (GStreamer incluido) y un .dmg.
#
#   ./scripts/bundle-macos.sh            # release
#   ./scripts/bundle-macos.sh --debug    # más rápido para probar
#
# Firma: ad hoc por omisión. Para distribuir fuera de esta Mac define
# APPLE_SIGNING_IDENTITY="Developer ID Application: …" (y notariza el .dmg).
set -euo pipefail

cd "$(dirname "$0")/.."
PROFILE=release
FLAG=()
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE=debug
  FLAG=(--debug)
fi

APP="src-tauri/target/$PROFILE/bundle/macos/TransmitIA.app"
rm -rf "${APP:?}"
npm run tauri build -- ${FLAG[@]+"${FLAG[@]}"} --bundles app

python3 scripts/macos_bundle_gst.py "$APP"

# Comprobar que nada apunte a Homebrew.
if find "$APP" -type f \( -perm -111 -o -name '*.dylib' -o -name '*.so' \) -print0 |
  xargs -0 otool -L 2>/dev/null | grep -E '^\s+/(opt/homebrew|usr/local)/'; then
  echo "ERROR: quedan referencias a Homebrew" >&2
  exit 1
fi

IDENTITY="${APPLE_SIGNING_IDENTITY:--}"
# Cada binario interno ya se firmó en macos_bundle_gst.py; aquí solo el paquete
# (--deep no sirve: rechaza la carpeta de plugins dentro de Contents/MacOS).
codesign --force --options runtime --entitlements src-tauri/entitlements.plist --sign "$IDENTITY" "$APP"
codesign --verify --strict "$APP"

VERSION=$(node -p "require('./src-tauri/tauri.conf.json').version")
ARCH=$(uname -m)
DMG="src-tauri/target/$PROFILE/bundle/TransmitIA_${VERSION}_${ARCH}.dmg"
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname TransmitIA -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "${STAGE:?}"
echo "Listo: $APP"
echo "       $DMG ($(du -h "$DMG" | cut -f1))"
