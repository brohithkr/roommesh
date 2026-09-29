#!/usr/bin/env bash
# CI helper for .github/workflows/release.yml: optional Developer ID signing and notarization.
#
#   scripts/ci-signing.sh setup     import the certificates into a temporary keychain and create a
#                                   notarytool keychain profile, driven by whichever secrets are set
#   scripts/ci-signing.sh cleanup   delete that keychain and restore the default keychain
#
# setup reads these env vars (the workflow maps the repository secrets onto them):
#   MACOS_CERT_P12_BASE64            base64 of a .p12 holding the Developer ID Application identity
#                                    (and, usually, the Developer ID Installer identity too)
#   MACOS_CERT_PASSWORD              its password
#   MACOS_INSTALLER_CERT_P12_BASE64  optional: a separate .p12 for the Developer ID Installer identity
#   MACOS_INSTALLER_CERT_PASSWORD    its password (defaults to MACOS_CERT_PASSWORD)
#   DEVELOPER_ID_APP                 e.g. "Developer ID Application: Name (TEAMID)"
#   DEVELOPER_ID_INSTALLER           e.g. "Developer ID Installer: Name (TEAMID)"
#   NOTARY_APPLE_ID, NOTARY_TEAM_ID, NOTARY_PASSWORD (an app-specific password)
#
# Signing is all-or-nothing per layer:
#   no MACOS_CERT_P12_BASE64            -> unsigned (ad-hoc app and driver, unsigned pkg and dmg)
#   certificate + DEVELOPER_ID_APP      -> app, driver and dmg signed; the pkg too if
#                                          DEVELOPER_ID_INSTALLER is set
#   + all three NOTARY_* secrets        -> also notarized and stapled (needs both identities)
# A half-configured layer (e.g. only some NOTARY_* secrets) fails instead of silently downgrading.
#
# On success, setup appends DEVELOPER_ID_APP, DEVELOPER_ID_INSTALLER and NOTARY_PROFILE (whichever
# apply) to $GITHUB_ENV, so the later `make` steps pass them to scripts/package.sh and
# scripts/make-dmg.sh, and writes mode=unsigned|signed|notarized to $GITHUB_OUTPUT.
set -euo pipefail

KEYCHAIN="${RUNNER_TEMP:?RUNNER_TEMP is not set}/roommesh-signing.keychain-db"
NOTARY_PROFILE_NAME=roommesh-notary
GITHUB_ENV=${GITHUB_ENV:-/dev/null}
GITHUB_OUTPUT=${GITHUB_OUTPUT:-/dev/null}
# Developer ID intermediates: an exported .p12 usually holds only the leaf certificates, and
# codesign/productbuild need the chain to the Apple root.
INTERMEDIATES=(
  https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
  https://www.apple.com/certificateauthority/DeveloperIDCA.cer
)

die() { echo "::error::$*" >&2; exit 1; }
have() { [ -n "${!1:-}" ]; }
output() { echo "$1=$2" >>"$GITHUB_OUTPUT"; }
export_env() { echo "$1=$2" >>"$GITHUB_ENV"; }

import_p12() { # <base64> <password>
  local p12
  p12=$(mktemp "$RUNNER_TEMP/cert.XXXXXX")
  printf '%s' "$1" | tr -d '[:space:]' | base64 --decode >"$p12" || { rm -f "$p12"; die "a certificate secret is not valid base64"; }
  security import "$p12" -k "$KEYCHAIN" -P "$2" -f pkcs12 \
    -T /usr/bin/codesign -T /usr/bin/productbuild -T /usr/bin/productsign -T /usr/bin/pkgbuild \
    -T /usr/bin/security >/dev/null || { rm -f "$p12"; die "security import failed (wrong password?)"; }
  rm -f "$p12"
}

has_identity() { # <policy> <identity>: a valid (trusted, unexpired) identity with that exact name
  security find-identity -v -p "$1" "$KEYCHAIN" | grep -Fq "\"$2\""
}

setup() {
  local notary_set=0 v
  for v in NOTARY_APPLE_ID NOTARY_TEAM_ID NOTARY_PASSWORD; do have "$v" && notary_set=$((notary_set + 1)); done

  if ! have MACOS_CERT_P12_BASE64; then
    for v in DEVELOPER_ID_APP DEVELOPER_ID_INSTALLER MACOS_INSTALLER_CERT_P12_BASE64; do
      have "$v" && die "$v is set but MACOS_CERT_P12_BASE64 is not; add the certificate secret or remove $v"
    done
    [ "$notary_set" = 0 ] || die "NOTARY_* secrets are set but MACOS_CERT_P12_BASE64 is not; notarization needs Developer ID signing"
    echo "No signing secrets: building an unsigned release (ad-hoc app and driver, unsigned pkg and dmg)."
    output mode unsigned
    return
  fi
  have MACOS_CERT_PASSWORD || die "MACOS_CERT_P12_BASE64 is set but MACOS_CERT_PASSWORD is not"
  have DEVELOPER_ID_APP || die "MACOS_CERT_P12_BASE64 is set but DEVELOPER_ID_APP is not"
  case "$notary_set" in
    0 | 3) ;;
    *) die "set all of NOTARY_APPLE_ID, NOTARY_TEAM_ID and NOTARY_PASSWORD, or none of them" ;;
  esac
  if [ "$notary_set" = 3 ] && ! have DEVELOPER_ID_INSTALLER; then
    die "notarization needs DEVELOPER_ID_INSTALLER as well (scripts/package.sh notarizes the signed pkg)"
  fi

  # ---- temporary keychain ------------------------------------------------------------------------
  local kc_pass prev_default
  kc_pass=$(uuidgen)
  echo "::add-mask::$kc_pass"
  prev_default=$(security default-keychain -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')
  export_env ROOMMESH_PREV_DEFAULT_KEYCHAIN "$prev_default"
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  security create-keychain -p "$kc_pass" "$KEYCHAIN"
  security set-keychain-settings -lut 21600 "$KEYCHAIN" # stay unlocked for 6 h (a universal build is slow)
  security unlock-keychain -p "$kc_pass" "$KEYCHAIN"
  # Put it first in the search list (codesign/productbuild look identities up there) and make it
  # the default keychain, which is where notarytool stores and later looks up its profile.
  local existing=()
  while IFS= read -r line; do
    line=$(sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//' <<<"$line")
    [ -n "$line" ] && [ "$line" != "$KEYCHAIN" ] && existing+=("$line")
  done < <(security list-keychains -d user)
  security list-keychains -d user -s "$KEYCHAIN" "${existing[@]+"${existing[@]}"}"
  security default-keychain -d user -s "$KEYCHAIN"

  local url cer
  for url in "${INTERMEDIATES[@]}"; do
    cer="$RUNNER_TEMP/$(basename "$url")"
    if curl -fsSL --retry 3 -o "$cer" "$url"; then
      security import "$cer" -k "$KEYCHAIN" >/dev/null 2>&1 || true # already present is fine
    else
      echo "::warning::could not download $url; relying on the keychain's existing intermediates"
    fi
  done

  import_p12 "$MACOS_CERT_P12_BASE64" "$MACOS_CERT_PASSWORD"
  if have MACOS_INSTALLER_CERT_P12_BASE64; then
    import_p12 "$MACOS_INSTALLER_CERT_P12_BASE64" "${MACOS_INSTALLER_CERT_PASSWORD:-$MACOS_CERT_PASSWORD}"
  fi
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kc_pass" "$KEYCHAIN" >/dev/null

  echo "Identities in the signing keychain:"
  security find-identity -v "$KEYCHAIN" | sed 's/^/  /'
  has_identity codesigning "$DEVELOPER_ID_APP" ||
    die "no valid code-signing identity \"$DEVELOPER_ID_APP\" in the imported certificate(s) (check DEVELOPER_ID_APP)"
  export_env DEVELOPER_ID_APP "$DEVELOPER_ID_APP"
  if have DEVELOPER_ID_INSTALLER; then
    # Installer identities are not code-signing identities; list them under the basic policy.
    has_identity basic "$DEVELOPER_ID_INSTALLER" ||
      die "no valid identity \"$DEVELOPER_ID_INSTALLER\" in the imported certificate(s) (check DEVELOPER_ID_INSTALLER)"
    export_env DEVELOPER_ID_INSTALLER "$DEVELOPER_ID_INSTALLER"
  else
    echo "::warning::DEVELOPER_ID_INSTALLER is not set: the app, driver and dmg are signed but the pkg is not"
  fi

  if [ "$notary_set" = 0 ]; then
    echo "::warning::NOTARY_* secrets are not set: the release is signed but not notarized"
    output mode signed
    return
  fi
  # ---- notarytool profile ------------------------------------------------------------------------
  xcrun notarytool store-credentials "$NOTARY_PROFILE_NAME" \
    --apple-id "$NOTARY_APPLE_ID" --team-id "$NOTARY_TEAM_ID" --password "$NOTARY_PASSWORD" >/dev/null
  # Check the credentials now, not after an hour of building.
  xcrun notarytool history --keychain-profile "$NOTARY_PROFILE_NAME" >/dev/null ||
    die "notarytool rejected the NOTARY_* credentials"
  export_env NOTARY_PROFILE "$NOTARY_PROFILE_NAME"
  echo "Signing with Developer ID and notarizing (notarytool profile $NOTARY_PROFILE_NAME)."
  output mode notarized
}

cleanup() {
  [ -e "$KEYCHAIN" ] || return 0
  if [ -n "${ROOMMESH_PREV_DEFAULT_KEYCHAIN:-}" ]; then
    security default-keychain -d user -s "$ROOMMESH_PREV_DEFAULT_KEYCHAIN" || true
  fi
  security delete-keychain "$KEYCHAIN" || true
  echo "removed $KEYCHAIN"
}

case "${1:-}" in
  setup) setup ;;
  cleanup) cleanup ;;
  *) echo "usage: $0 setup|cleanup" >&2; exit 2 ;;
esac
