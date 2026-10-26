#!/usr/bin/env bash
set -euo pipefail

repo_dir="${1:-}"
app_id="${2:-io.github.JohanWes.WarcraftRustcorder}"
second_repo="${3:-}"

if [[ -z "$repo_dir" ]]; then
  printf 'usage: %s REPO [APP_ID] [SECOND_REPO]\n' "$0" >&2
  exit 2
fi

work_dir=$(mktemp -d)
remote_name="wr014-candidate-${BASHPID}"
installed_here=0
cleanup() {
  if [[ "$installed_here" == 1 ]]; then
    flatpak uninstall --user --assumeyes "$app_ref" >/dev/null 2>&1 || true
  fi
  flatpak remote-delete --user "$remote_name" >/dev/null 2>&1 || true
  rm -rf "$work_dir"
}
trap cleanup EXIT
if [[ -n "${WR_FLATPAK_USER_DIR:-}" ]]; then
  export FLATPAK_USER_DIR="$WR_FLATPAK_USER_DIR"
  mkdir -p "$FLATPAK_USER_DIR"
fi

app_ref="$app_id//stable"

# This installs, launches and removes the app. Doing that on top of a real
# install would take the user's deployment away with it, so refuse instead;
# WR_FLATPAK_USER_DIR points the whole run at a throwaway installation.
if flatpak info --user "$app_ref" >/dev/null 2>&1; then
  printf '%s is already installed for this user; set WR_FLATPAK_USER_DIR to a scratch installation\n' \
    "$app_ref" >&2
  exit 1
fi

remote_args=(--user)
if [[ -n "${FLATPAK_GPG_PUBLIC_KEY:-}" ]]; then
  remote_args+=(--gpg-import="$FLATPAK_GPG_PUBLIC_KEY")
else
  remote_args+=(--no-gpg-verify)
fi

flatpak remote-add "${remote_args[@]}" "$remote_name" "$repo_dir"
flatpak install --user --assumeyes "$remote_name" "$app_ref"
installed_here=1
first_commit=$(flatpak info --user --show-commit "$app_ref")
test -n "$first_commit"
flatpak run --user --command=true "$app_ref"

if [[ -n "$second_repo" ]]; then
  # The installed ref keeps its original remote as its update origin. Point
  # that origin at the staged local copy, which must contain both commits.
  second_repo_url="$second_repo"
  if [[ "$second_repo_url" == /* ]]; then
    second_repo_url="file://$second_repo_url"
  fi
  flatpak remote-modify --user "$remote_name" --url="$second_repo_url"
  flatpak remote-info --user --show-commit "$remote_name" "$app_ref" >/dev/null
  flatpak update --user --assumeyes "$app_ref"
  second_commit=$(flatpak info --user --show-commit "$app_ref")
  test "$first_commit" != "$second_commit"
  flatpak update --user --assumeyes --commit="$first_commit" "$app_ref"
  test "$(flatpak info --user --show-commit "$app_ref")" = "$first_commit"
fi

flatpak uninstall --user --assumeyes "$app_ref"
scope="${second_repo:+update and rollback}"
printf 'Flatpak install, launch, %s, and app removal passed\n' \
  "${scope:-candidate smoke}"
