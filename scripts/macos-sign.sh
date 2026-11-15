#!/bin/bash
# Signs a built binary with a certificate of this machine's own, so that
# what macOS was allowed for the app (Screen Recording, Accessibility)
# stays allowed after the next build. The linker's ad-hoc signature names
# the binary by its hash: each build is then an app macOS has never seen.
#   scripts/macos-sign.sh [BINARY]        default: target/release/ideae
# The first run makes the certificate in the login keychain and may ask to
# let codesign use it: Always Allow.
set -e
name="ideae local signing"
root=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$root/target/release/ideae}
id=io.github.scillarium.ideae
case $bin in */debug/*) id=io.github.scillarium.ideae-dev ;; esac

if ! security find-identity -p codesigning | grep -q "\"$name\""; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  cat > "$tmp/c.cnf" <<CNF
[req]
distinguished_name=dn
x509_extensions=ext
prompt=no
[dn]
CN=$name
[ext]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
CNF
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$tmp/c.cnf" \
    -keyout "$tmp/k.pem" -out "$tmp/c.pem" 2>/dev/null
  /usr/bin/openssl pkcs12 -export -inkey "$tmp/k.pem" -in "$tmp/c.pem" -out "$tmp/id.p12" \
    -passout pass:ideae -name "$name"
  security import "$tmp/id.p12" -k "$HOME/Library/Keychains/login.keychain-db" -P ideae -T /usr/bin/codesign
fi
codesign --force --sign "$name" --identifier "$id" "$bin"
codesign -d -r- "$bin" 2>&1 | tail -1
