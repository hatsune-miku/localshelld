#!/usr/bin/env bash
# Stable and RC releases ship the same complete, role-separated bundle.
set -euo pipefail
: "${LOCALSHELLD_RELEASE_TAG:?expected release tag is required}"
mkdir -p assets packages
for platform in windows-x86_64 linux-x86_64 macos-aarch64; do
  source="binaries/trustd-$platform"
  actual="$(tr -d '\r\n' < "$source/release-version.txt")"
  test "$actual" = "trustd ${LOCALSHELLD_RELEASE_TAG#v}" || {
    echo "$platform version mismatch; rerun every CI platform" >&2; exit 1;
  }
  package="trustd-$platform"
  mkdir -p "packages/$package"
  suffix=''
  roles=(trustd trustd-user trustd-stdio)
  if [[ "$platform" == windows-* ]]; then
    suffix=.exe
    roles+=(trustd-system)
  else
    roles+=(trustd-root)
  fi
  for role in "${roles[@]}"; do
    test -f "$source/$role$suffix"
    cp "$source/$role$suffix" "packages/$package/"
    if [[ -z "$suffix" ]]; then chmod +x "packages/$package/$role"; fi
  done
  cp README.md LICENSE-MIT "packages/$package/"
  cp scripts/package/START-HERE.txt "packages/$package/"
  if [[ "$platform" == windows-* ]]; then
    cp scripts/package/INSTALL.cmd "packages/$package/"
  elif [[ "$platform" == macos-* ]]; then
    cp scripts/package/INSTALL.sh "packages/$package/INSTALL.command"
    chmod +x "packages/$package/INSTALL.command"
  else
    cp scripts/package/INSTALL.sh "packages/$package/"
    chmod +x "packages/$package/INSTALL.sh"
  fi
  if [[ "$platform" == windows-* ]]; then
    (cd packages && zip -qr "../assets/$package.zip" "$package")
  else
    tar -czf "assets/$package.tar.gz" -C packages "$package"
  fi
done
(cd assets && sha256sum trustd-* > SHA256SUMS)
