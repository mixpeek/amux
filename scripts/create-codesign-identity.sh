#!/usr/bin/env bash
# Create the stable self-signed code-signing identity the auto-builder wants.
#
# WHY. rust-auto-build.sh signs each new server binary as `amux-dev` (or
# $AMUX_CODESIGN_IDENTITY). With no such identity the binary stays ADHOC-signed,
# and an ad-hoc signature is derived from the BINARY — so every rebuild is a
# different program to macOS. TCC grants are per-program, so "Allow" can never
# stick and the user is re-prompted with:
#
#   "amux-server-rs" was prevented from modifying apps on your Mac.
#
# A stable identity makes every rebuild the SAME program, so one Allow holds.
#
# Idempotent: exits 0 if the identity already exists. Safe to re-run.
# To undo: security delete-certificate -c amux-dev
set -euo pipefail

CS_ID="${AMUX_CODESIGN_IDENTITY:-amux-dev}"
KEYCHAIN="${AMUX_CODESIGN_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$CS_ID"; then
  echo "identity '$CS_ID' already exists — nothing to do"
  security find-identity -v -p codesigning | grep -F "$CS_ID"
  exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A PREVIOUS FAILED ATTEMPT LEAVES A CERT BEHIND, AND TWO CERTS WITH THE SAME
# COMMON NAME MAKE CODESIGN REFUSE:
#   amux-dev: ambiguous (matches "amux-dev" and "amux-dev" in login.keychain-db)
# `security delete-identity` removes the key and leaves the certificate, so the
# obvious cleanup is what creates this state. Sweep any orphan first — this is
# only reached when find-identity found NO usable identity, so nothing here is
# in use.
for h in $(security find-certificate -a -c "$CS_ID" -Z "$KEYCHAIN" 2>/dev/null \
           | awk '/^SHA-1 hash:/ {print $3}'); do
  echo "removing an orphaned '$CS_ID' certificate ($h) left by an earlier attempt"
  security delete-certificate -Z "$h" "$KEYCHAIN" >/dev/null 2>&1 || true
done

# codeSigning EKU is what makes `-p codesigning` match it; without the EKU the
# cert imports fine and find-identity still reports zero, which reads as the
# import having failed.
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
  -subj "/CN=$CS_ID/O=amux local build" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" >/dev/null 2>&1

# macOS `security` reads only the LEGACY PKCS#12 encryption. OpenSSL 3
# defaults to AES-256-CBC + SHA-256, which imports with
# "MAC verification failed during PKCS12 import (wrong password?)" — a message
# that blames the password when the password is right and the CIPHER is wrong.
# Pin 3DES/SHA1 explicitly rather than relying on -legacy, which is absent on
# some builds and silently leaves the modern default in place.
P12_ARGS=(-export -inkey "$WORK/key.pem" -in "$WORK/cert.pem"
          -out "$WORK/$CS_ID.p12" -passout pass:amux
          -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1)
if openssl pkcs12 -legacy "${P12_ARGS[@]}" >/dev/null 2>&1; then :
elif openssl pkcs12 "${P12_ARGS[@]}" >/dev/null 2>&1; then :
else
  echo "FAILED: could not write a legacy PKCS#12 that macOS can import" >&2
  exit 1
fi

# -T /usr/bin/codesign pre-authorises codesign to use the key, so the builder
# does not raise a keychain prompt on every deploy.
security import "$WORK/$CS_ID.p12" -k "$KEYCHAIN" -P amux \
  -T /usr/bin/codesign -T /usr/bin/security >/dev/null

# A self-signed cert is not trusted for code signing until it is said to be.
# The USER trust domain is enough for codesign and needs no sudo; -d (admin
# domain) would.
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$WORK/cert.pem" >/dev/null 2>&1 \
  || echo "note: add-trusted-cert did not complete; if find-identity below is empty, run this script from a Terminal that can show a keychain prompt"

# PROVE IT SIGNS. find-identity listing the name is not the same as codesign
# accepting it — a cert with the wrong keyUsage lists AND fails, which is how
# the first attempt at this looked like success.
PROBE="$WORK/probe"
cp /bin/echo "$PROBE"
if codesign --force --sign "$CS_ID" --identifier com.amux.codesign-probe "$PROBE" >/dev/null 2>&1 \
   && codesign -dv "$PROBE" 2>&1 | grep -q "Authority=$CS_ID"; then
  echo "verified: codesign produced a signature authored by '$CS_ID'"
else
  echo "FAILED: '$CS_ID' exists but codesign will not sign with it." >&2
  codesign --force --sign "$CS_ID" --identifier com.amux.codesign-probe "$PROBE" 2>&1 | head -3 >&2
  exit 1
fi

# Stop the partition-list prompt on first use (macOS Sierra+).
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "" "$KEYCHAIN" >/dev/null 2>&1 || true

echo "--- identities now visible to codesign ---"
security find-identity -v -p codesigning || true
if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$CS_ID"; then
  echo "OK: '$CS_ID' created. The next auto-build signs with it, and the macOS"
  echo "    App Management grant will stick from the first Allow after that."
else
  echo "FAILED: '$CS_ID' is not visible to codesign. Nothing was left behind but"
  echo "        the certificate; remove it with: security delete-certificate -c $CS_ID"
  exit 1
fi
