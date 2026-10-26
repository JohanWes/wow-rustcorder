#!/usr/bin/env bash
set -euo pipefail

# Installer for the Warcraft Rustcorder Flatpak. Safe to pipe into bash: it adds
# the project remote, installs the app, and starts it. Re-running it updates an
# existing install.

REMOTE_NAME="warcraft-rustcorder"
REMOTE_DESCRIPTOR="${WARCRAFTRUSTCORDER_REMOTE_DESCRIPTOR:-https://johanwes.github.io/wow-rustcorder/index.flatpakrepo}"
FLATHUB_DESCRIPTOR="${WARCRAFTRUSTCORDER_FLATHUB_DESCRIPTOR:-https://dl.flathub.org/repo/flathub.flatpakrepo}"
INSTALL_PAGE="${WARCRAFTRUSTCORDER_INSTALL_PAGE:-https://github.com/JohanWes/wow-rustcorder#install}"
APP_ID="io.github.JohanWes.WarcraftRustcorder"
# Installs from before the rename from Warcraft Recorder.
OLD_REMOTE_NAME="warcraft-recorder"
OLD_APP_REF="io.github.JohanWes.WarcraftRecorder//stable"
# Always address the published branch: a leftover development install of the
# same application ID would otherwise make a bare `flatpak run` ambiguous.
APP_REF="${APP_ID}//stable"
# Kept in step with `runtime-version` in the release manifest.
RUNTIME_REF="org.gnome.Platform//50"

log() { printf '[install] %s\n' "$*"; }
warn() { printf '[install] WARNING: %s\n' "$*" >&2; }

manual_instructions() {
  printf '[install] Automatic Flatpak installation failed.\n' >&2
  printf '[install] Installation page: %s\n' "$INSTALL_PAGE" >&2
  printf '[install] Manual commands:\n' >&2
  printf '  flatpak remote-add --user --if-not-exists flathub %s\n' "$FLATHUB_DESCRIPTOR" >&2
  printf '  flatpak remote-add --user --if-not-exists %s %s\n' \
    "$REMOTE_NAME" "$REMOTE_DESCRIPTOR" >&2
  printf '  flatpak install --user %s %s\n' "$REMOTE_NAME" "$APP_REF" >&2
}

fail_with_manual_instructions() {
  manual_instructions
  return 1
}

# Flatpak itself is the one thing this script cannot install: it needs the
# distribution's package manager and a password.
require_flatpak() {
  command -v flatpak >/dev/null 2>&1 && return 0
  printf '[install] Flatpak is required and is not installed.\n' >&2
  printf '[install] Install it, log out and back in, then run this again:\n' >&2
  printf '  Arch, CachyOS, Manjaro:   sudo pacman -S flatpak\n' >&2
  printf '  Fedora, Nobara:           sudo dnf install flatpak\n' >&2
  printf '  Ubuntu, Debian, Mint:     sudo apt install flatpak\n' >&2
  printf '  openSUSE:                 sudo zypper install flatpak\n' >&2
  printf '[install] Installation page: %s\n' "$INSTALL_PAGE" >&2
  return 1
}

# The project remote carries the application only. Without a remote that
# publishes the GNOME runtime the install fails on an unresolvable dependency,
# so make sure one is reachable before asking for the app.
ensure_runtime_source() {
  if flatpak info "$RUNTIME_REF" >/dev/null 2>&1; then
    return 0
  fi
  if flatpak remotes --user --columns=name | grep -qx flathub; then
    return 0
  fi
  log "Adding Flathub for the GNOME runtime..."
  flatpak remote-add --user --if-not-exists flathub "$FLATHUB_DESCRIPTOR" ||
    warn "could not add Flathub; the runtime must come from an existing remote"
}

# Capture goes through the desktop's xdg-desktop-portal ScreenCast backend,
# and the frames arrive over PipeWire. Both are host components the Flatpak
# cannot carry, and a missing one shows up as a failed recording long after
# the install looked fine. A backend advertises the interfaces it implements
# in its `.portal` file; xdg-desktop-portal-gtk alone is not enough, because
# it does not implement ScreenCast.
screencast_backend_present() {
  local dir
  local -a data_dirs
  # Quoted iteration: a data dir is allowed to contain glob characters.
  IFS=: read -ra data_dirs <<<"${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
  for dir in "${data_dirs[@]}"; do
    if grep -qrs 'org\.freedesktop\.impl\.portal\.ScreenCast' \
      "${dir}/xdg-desktop-portal/portals"; then
      return 0
    fi
  done
  return 1
}

# Warnings only. None of this stops the app from installing or updating, and
# a user who fixes the session afterwards needs no second install.
check_session() {
  # Only x11 is a diagnosable problem. An unset or `tty` session type means
  # this is not running from the desktop the app will launch into, so the
  # checks below have nothing trustworthy to look at either.
  if [[ "${XDG_SESSION_TYPE:-}" == x11 ]]; then
    warn "this looks like an X11 session; Warcraft Rustcorder needs a Wayland session"
    return 0
  fi
  [[ -n "${WAYLAND_DISPLAY:-}" ]] || return 0
  if ! screencast_backend_present; then
    warn "no screen-capture portal found; install the portal backend for your desktop"
    warn "  before recording (for example xdg-desktop-portal-gnome, -kde,"
    warn "  -hyprland or -wlr)"
  fi
  if [[ ! -S "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/pipewire-0" ]]; then
    warn "PipeWire does not look like it is running; the portal needs it to send frames"
  fi
}

require_flatpak

log "Installing Warcraft Rustcorder..."

ensure_runtime_source

# Updating the old app moves it, data included, to the renamed one.
if flatpak info --user "$OLD_APP_REF" >/dev/null 2>&1; then
  log "Moving the Warcraft Recorder install over to Warcraft Rustcorder."
  if ! flatpak update --user --assumeyes "$OLD_APP_REF"; then
    fail_with_manual_instructions
  fi
fi

if ! flatpak remote-add --user --if-not-exists "$REMOTE_NAME" "$REMOTE_DESCRIPTOR"; then
  fail_with_manual_instructions
fi

# Re-running this script is the documented way to update, and an install
# command fails on an application that is already there — including one that
# came from a bundle, whose origin is not this remote.
if flatpak info --user "$APP_REF" >/dev/null 2>&1; then
  origin=$(flatpak info --user --show-origin "$APP_REF" 2>/dev/null || true)
  if [[ "$origin" == "$REMOTE_NAME" || "$origin" == "$OLD_REMOTE_NAME" ]]; then
    log "Warcraft Rustcorder is already installed; updating it."
    if ! flatpak update --user --assumeyes "$APP_REF"; then
      fail_with_manual_instructions
    fi
  else
    log "Replacing the existing install with the signed project release."
    if ! flatpak install --user --assumeyes --reinstall "$REMOTE_NAME" "$APP_REF"; then
      fail_with_manual_instructions
    fi
  fi
elif ! flatpak install --user --assumeyes "$REMOTE_NAME" "$APP_REF"; then
  fail_with_manual_instructions
fi

# Report session problems here rather than earlier: these do not block the
# install, and next to the launch is where they are still on screen.
check_session

# Start the app here.
log "Launching Warcraft Rustcorder..."
if command -v setsid >/dev/null 2>&1; then
  setsid flatpak run "$APP_REF" >/dev/null 2>&1 </dev/null &
else
  flatpak run "$APP_REF" >/dev/null 2>&1 </dev/null &
fi

log "Done. Warcraft Rustcorder is installed and updates through Flatpak."
