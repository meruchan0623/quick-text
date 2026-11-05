#!/usr/bin/env bash
# Local macOS build + install: bundles Quicktext.app and replaces /Applications/Quicktext.app.
#
# For use on this machine only. It skips build.beforeBundleCommand (the Windows-only release gate that
# demands the generated third-party notices and the dependency-sources archive), so the resulting app
# carries no distribution compliance files and must not be shared. Distributable builds go through
# scripts/build-windows.ps1 / the release workflow.
#
# Usage: scripts/build-macos.sh [--no-install]
set -euo pipefail

APP_NAME="Quicktext"
INSTALL_DIR="/Applications"
cd "$(dirname "$0")/.."

install=1
[[ "${1:-}" == "--no-install" ]] && install=0

npm run tauri -- build --bundles app --config '{"build":{"beforeBundleCommand":""}}'

bundle="target/release/bundle/macos/${APP_NAME}.app"
[[ -d "$bundle" ]] || { echo "bundle not found: $bundle" >&2; exit 1; }
echo "built: $bundle"

if [[ $install -eq 0 ]]; then
  exit 0
fi

# Quit the running instance (it lives in the tray) so the bundle can be replaced.
if pgrep -x quicktext >/dev/null; then
  pkill -x quicktext || true
  sleep 1
fi

rm -rf "${INSTALL_DIR:?}/${APP_NAME}.app"
cp -R "$bundle" "${INSTALL_DIR}/${APP_NAME}.app"
# Unsigned local build: drop the quarantine flag so Gatekeeper does not block the first launch.
xattr -dr com.apple.quarantine "${INSTALL_DIR}/${APP_NAME}.app" 2>/dev/null || true

echo "installed: ${INSTALL_DIR}/${APP_NAME}.app"
open "${INSTALL_DIR}/${APP_NAME}.app"
