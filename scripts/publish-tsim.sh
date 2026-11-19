#!/usr/bin/env bash
# Builds, signs and uploads the macOS (Apple silicon) release to the TSim update servers
# (docs/auto-update.md; contract: client-updates CONTRACT.md §3 and §6). Runs on the packaging Mac
# only; no GitHub involved.
#
#   1. version from the manifests (scripts/version.mjs check), update notes from --notes,
#      --notes-file or release-notes/<version>.txt (plain text, 1-4000 characters)
#   2. build Quicktext.app with the stable update channel (skipped with --skip-build: the .app
#      already in target/aarch64-apple-darwin/release/bundle/macos is reused)
#   3. codesign with the fixed self-signed identity (scripts/setup-macos-signing.sh); no ad-hoc
#      fallback: an app with another designated requirement loses the granted permissions
#   4. update bundle Quicktext_<v>_aarch64.app.tar.gz, signed with `tauri signer sign
#      --app-version` and verified against src-tauri/update-keys/current.pub. It is the only macOS
#      file on the server: auto-update and first install both use it (double-click to extract, drag
#      Quicktext.app to Applications); no dmg
#   5. multipart upload to POST <server>/client-updates/v1/publish (draft, or published with
#      --publish)
#
# Like the former CI build, the bundle skips build.beforeBundleCommand (the Windows-only gate for
# the third-party notices and dependency sources, which do not exist for macOS yet).
#
# Usage: scripts/publish-tsim.sh --server prod|test|both [--publish] [--skip-build]
#                                [--notes TEXT | --notes-file FILE]
#
# Environment (never committed, never printed):
#   TSIM_PUBLISH_KEY_PROD / TSIM_PUBLISH_KEY_TEST   publish keys of the servers uploaded to
#   TAURI_SIGNING_PRIVATE_KEY (content) or TAURI_SIGNING_PRIVATE_KEY_PATH (file; default
#     ~/.config/quicktext-signing/updater.key when it exists), TAURI_SIGNING_PRIVATE_KEY_PASSWORD
#   QUICKTEXT_SIGNING_IDENTITY  codesign identity (default "Quicktext Local Signing"); use the
#     same certificate for every release, so installed apps keep their permissions
#   QUICKTEXT_SIGNING_KEYCHAIN  keychain holding it (default: the search list)
set -euo pipefail
cd "$(dirname "$0")/.."

PROD_URL="https://csapi.tsim.tech"
TEST_URL="https://test-csapi.tsim.tech"
PRODUCT="quicktext"
TARGET="aarch64-apple-darwin"

die() { echo "error: $*" >&2; exit 1; }

server="" publish=0 skip_build=0 notes="" notes_file=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server) server="${2:-}"; shift 2 ;;
    --publish) publish=1; shift ;;
    --skip-build) skip_build=1; shift ;;
    --notes) notes="${2:-}"; shift 2 ;;
    --notes-file) notes_file="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

case "$server" in
  prod) servers=(prod) ;;
  test) servers=(test) ;;
  both) servers=(prod test) ;;
  *) die "--server prod|test|both is required" ;;
esac
[[ "$(uname -s)" == Darwin ]] || die "macOS only (Windows: scripts/publish-tsim.ps1)"

# 1. Version and notes: everything checked before the long build.
version="$(node scripts/version.mjs check | sed -n 's/^version \([^ ]*\) .*/\1/p')"
[[ -n "$version" ]] || die "version manifests disagree (node scripts/version.mjs check)"
if [[ -z "$notes" ]]; then
  [[ -n "$notes_file" ]] || notes_file="release-notes/$version.txt"
  [[ -f "$notes_file" ]] || die "no update notes: pass --notes, --notes-file or create $notes_file"
  notes="$(cat "$notes_file")"
fi
notes_len="$(printf '%s' "$notes" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log([...s.trim()].length))')"
(( notes_len >= 1 && notes_len <= 4000 )) || die "update notes must be 1-4000 characters (got $notes_len)"

for s in "${servers[@]}"; do
  var="TSIM_PUBLISH_KEY_$(echo "$s" | tr '[:lower:]' '[:upper:]')"
  [[ -n "${!var:-}" ]] || die "$var is not set"
done
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" && -z "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]]; then
  default_key="$HOME/.config/quicktext-signing/updater.key"
  [[ -f "$default_key" ]] || die "set TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH (or put the key at $default_key)"
  export TAURI_SIGNING_PRIVATE_KEY_PATH="$default_key"
fi

identity="${QUICKTEXT_SIGNING_IDENTITY:-Quicktext Local Signing}"
keychain="${QUICKTEXT_SIGNING_KEYCHAIN:-}"
security find-identity -p codesigning ${keychain:+"$keychain"} | grep -qF "\"$identity\"" \
  || die "signing identity \"$identity\" not found (scripts/setup-macos-signing.sh, or import the release certificate)"

# 2. Build.
app="target/$TARGET/release/bundle/macos/Quicktext.app"
if [[ $skip_build -eq 0 ]]; then
  export QT_UPDATE_CHANNEL=stable
  npx tauri build --target "$TARGET" --bundles app \
    --config "{\"build\":{\"beforeBundleCommand\":\"\"},\"bundle\":{\"macOS\":{\"bundleVersion\":\"$version\"}}}"
fi
[[ -d "$app" ]] || die "app not found: $app"

# 3. Codesign with the fixed identity; the designated requirement must stay the same.
bundle_id="$(node -p "require('./src-tauri/tauri.conf.json').identifier")"
codesign --force --options runtime --sign "$identity" ${keychain:+--keychain "$keychain"} \
  --identifier "$bundle_id" "$app"
codesign --verify --deep --strict "$app"
requirement="$(codesign -d -r- "$app" 2>&1 | grep designated)"
case "$requirement" in
  *"identifier \"$bundle_id\" and certificate leaf = H\""*) ;;
  *) die "unstable designated requirement: $requirement" ;;
esac
short="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
[[ "$short" == "$version" ]] || die "CFBundleShortVersionString is $short, expected $version"

# 4. The signed update bundle.
out="target/tsim-publish/$version/macos"
rm -rf "$out"
mkdir -p "$out"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
bundle="$out/Quicktext_${version}_aarch64.app.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$bundle" -C "$(dirname "$app")" Quicktext.app
tar -tzf "$bundle" | head -n1 | grep -qx "Quicktext.app/" || die "update bundle must start with Quicktext.app/"
npx tauri signer sign --app-version "$version" "$bundle" >/dev/null
node scripts/update-manifest.mjs --version "$version" --pubkey src-tauri/update-keys/current.pub \
  --base-url https://example.invalid/check --out "$stage/check.json" "darwin-aarch64=$bundle" >/dev/null
echo "packaged and verified: $bundle"

# 5. Upload. The key goes to curl on stdin, not on its command line.
failed=0
for s in "${servers[@]}"; do
  if [[ "$s" == prod ]]; then base="$PROD_URL"; key="$TSIM_PUBLISH_KEY_PROD"; else base="$TEST_URL"; key="$TSIM_PUBLISH_KEY_TEST"; fi
  args=(-sS -o "$stage/response-$s.json" -w '%{http_code}' -X POST "$base/client-updates/v1/publish"
    --form-string "product=$PRODUCT" --form-string "platform=macos" --form-string "version=$version"
    --form-string "notes=$notes" --form-string "signature=$(cat "$bundle.sig")"
    -F "updater_file=@$bundle")
  [[ $publish -eq 1 ]] && args+=(--form-string "publish=true")
  echo "uploading $version to $s ($base)…"
  code="$(printf 'header = "X-Client-Publish-Key: %s"\n' "$key" | curl -K - "${args[@]}")" || code="000"
  if [[ "$code" == 2* ]]; then
    echo "$s: HTTP $code $(cat "$stage/response-$s.json")"
  else
    echo "$s: upload FAILED (HTTP $code) $(cat "$stage/response-$s.json" 2>/dev/null)" >&2
    failed=1
  fi
done
[[ $failed -eq 0 ]] || die "not every upload succeeded; rerun with --skip-build --server <the failed one>"
echo "done: $version $( [[ $publish -eq 1 ]] && echo published || echo 'uploaded as draft (publish it in the admin page)' )"
