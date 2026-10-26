#!/usr/bin/env bash
set -euo pipefail

repo_dir="${1:-}"
remote_url="${2:-https://johanwes.github.io/wow-rustcorder/}"
key_id="${FLATPAK_GPG_KEY_ID:-}"

if [[ -z "$repo_dir" || -z "$key_id" ]]; then
  printf 'usage: FLATPAK_GPG_KEY_ID=<public-key-id> %s REPO [REMOTE_URL]\n' "$0" >&2
  exit 2
fi
if [[ ! -d "$repo_dir" ]]; then
  printf 'repository does not exist: %s\n' "$repo_dir" >&2
  exit 1
fi

flatpak build-update-repo \
  --gpg-sign="$key_id" \
  --generate-static-deltas \
  --prune \
  "$repo_dir"

public_key=$(gpg --export "$key_id" | base64 -w0)
template="$(dirname "$0")/../flatpak/io.github.JohanWes.WarcraftRustcorder.flatpakrepo.in"
sed \
  -e "s|^Url=.*$|Url=${remote_url%/}/|" \
  -e "s|@FLATPAK_GPG_KEY_BASE64@|${public_key}|" \
  "$template" >"$repo_dir/index.flatpakrepo"

printf 'prepared signed repository: %s\n' "$repo_dir"
