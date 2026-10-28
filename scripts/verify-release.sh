#!/usr/bin/env bash
set -euo pipefail
: "${LOCALSHELLD_RELEASE_TAG:?expected release tag is required}"
roles=(trustd trustd-user trustd-stdio)
suffix=''
if [[ "$RUNNER_OS" == Windows ]]; then
  suffix=.exe
  roles+=(trustd-system)
else
  roles+=(trustd-root)
fi
for role in "${roles[@]}"; do
  actual="$("target/release/$role$suffix" --version | tr -d '\r')"
  test "$actual" = "$role ${LOCALSHELLD_RELEASE_TAG#v}"
done
printf 'trustd %s\n' "${LOCALSHELLD_RELEASE_TAG#v}" > target/release/release-version.txt
