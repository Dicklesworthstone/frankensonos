#!/usr/bin/env bash
# Build a local, ad-hoc signed app. No credentials, installers or services.
set -euo pipefail
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Packaging the native remote requires macOS.' >&2
  exit 1
fi
here="$(cd "$(dirname "$0")" && pwd)"
bundle="${1:-$here/.build/FrankenSonosRemote.app}"
if [[ -e "$bundle" ]]; then
  echo "Destination already exists; choose another path: $bundle" >&2
  exit 2
fi
swift build --package-path "$here" --configuration release --product FrankenSonosRemote -Xswiftc -warnings-as-errors
bin="$(swift build --package-path "$here" --configuration release --show-bin-path)"
mkdir -p "$bundle/Contents/MacOS"
cp "$bin/FrankenSonosRemote" "$bundle/Contents/MacOS/FrankenSonosRemote"
cp "$here/Resources/Info.plist" "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist"
codesign --sign - "$bundle"
codesign --verify --strict "$bundle"
printf 'Created %s\n' "$bundle"
