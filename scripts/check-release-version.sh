#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
if [[ -z "$tag" || "$tag" != v* ]]; then
  printf 'usage: %s vX.Y.Z\n' "$0" >&2
  exit 2
fi

version="${tag#v}"
cargo_version=$(awk -F'"' '/^version = "/ { print $2; exit }' native/Cargo.toml)
appstream_version=$(awk -F'"' '/<release version="/ { print $2; exit }' data/io.github.JohanWes.WarcraftRustcorder.metainfo.xml)

if [[ "$version" != "$cargo_version" || "$version" != "$appstream_version" ]]; then
  printf 'release version mismatch: tag=%s Cargo=%s AppStream=%s\n' \
    "$version" "$cargo_version" "$appstream_version" >&2
  exit 1
fi

# The "What's new" dialog reads this section out of the compiled-in file.
if ! grep -q "^## $version\$" data/release-notes.md; then
  printf 'data/release-notes.md has no section for %s; run scripts/generate-release-notes.sh %s\n' \
    "$version" "$version" >&2
  exit 1
fi

printf 'release version: %s (Cargo.toml, AppStream and release notes agree)\n' "$version"
