#!/usr/bin/env bash
# One-time setup: a fixed self-signed code-signing identity for local macOS builds.
#
# Why: macOS privacy permissions (Accessibility, Input Monitoring, ...) are keyed to the app's designated
# requirement. An ad-hoc signed build's requirement is its cdhash, which changes on every rebuild, so each
# reinstall asks for the permissions again. Signed with a fixed certificate, the requirement becomes
# `identifier "com.quicktext.desktop" and certificate leaf = H"<cert hash>"`, the same on every build, and the
# granted permissions carry over. Local builds (scripts/build-macos.sh) sign with it.
#
# The certificate is not trusted by Gatekeeper and needs no trust setting: TCC compares the certificate hash,
# not its trust. Keep the stored .p12: a new certificate has a new hash, and the permissions must then be
# granted once more.
#
# Usage: scripts/setup-macos-signing.sh
#   QUICKTEXT_SIGNING_IDENTITY  certificate name (default "Quicktext Local Signing")
#   QUICKTEXT_SIGNING_DIR       where the .p12 and its password are kept (default ~/.config/quicktext-signing)
#   QUICKTEXT_SIGNING_KEYCHAIN  keychain to import into (default: login keychain)
set -euo pipefail

NAME="${QUICKTEXT_SIGNING_IDENTITY:-Quicktext Local Signing}"
DIR="${QUICKTEXT_SIGNING_DIR:-$HOME/.config/quicktext-signing}"
KEYCHAIN="${QUICKTEXT_SIGNING_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"
P12="$DIR/identity.p12"
PASSFILE="$DIR/identity.password"
# The system LibreSSL writes a PKCS#12 that `security import` reads (OpenSSL 3 defaults need -legacy).
OPENSSL=/usr/bin/openssl

has_identity() { security find-identity -p codesigning "$KEYCHAIN" | grep -qF "\"$NAME\""; }

# 1. The certificate file: created once, reused afterwards (its hash is what macOS remembers).
if [[ ! -f "$P12" ]]; then
  if has_identity; then
    echo "\"$NAME\" is in the keychain but $P12 is missing." >&2
    echo "Export it from Keychain Access to $P12 (password in $PASSFILE), or delete it and rerun." >&2
    exit 1
  fi
  mkdir -p "$DIR"
  chmod 700 "$DIR"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  cat >"$tmp/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
  "$OPENSSL" req -x509 -newkey rsa:2048 -nodes -days 3650 -sha256 \
    -config "$tmp/cert.cnf" -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null
  (umask 077 && uuidgen >"$PASSFILE")
  (umask 077 && "$OPENSSL" pkcs12 -export -name "$NAME" -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -out "$P12" -passout "file:$PASSFILE")
  echo "created certificate: $P12 (valid 10 years)"
fi
pass="$(cat "$PASSFILE")"

# 2. The local keychain (-T: codesign may use the key without a prompt on every build).
if has_identity; then
  echo "signing identity already in keychain: $NAME"
else
  security import "$P12" -k "$KEYCHAIN" -P "$pass" -T /usr/bin/codesign >/dev/null
  has_identity || { echo "import failed: $NAME not found in $KEYCHAIN" >&2; exit 1; }
  echo "imported signing identity: $NAME"
fi
