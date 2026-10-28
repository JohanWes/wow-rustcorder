# <img src="data/icons/io.github.JohanWes.WarcraftRustcorder.svg" width="44" alt=""> Warcraft Rustcorder

Warcraft Rustcorder records World of Warcraft on Linux and Windows. It reads the
combat log, detects each activity and saves a video with a metadata file beside
it. It is a native Rust and GTK4 application, distributed as a Flatpak for
Wayland sessions and as a Windows installer.

It started out inspired by [Warcraft Recorder](https://github.com/aza547/wow-recorder)
and has since grown into its own project: written entirely in Rust, with a
focus on low CPU, memory and disk use while you play.

![Intro: a timed Mythic+ key, the recording library, the combat meter with spell tooltips, target filter and death recap, and the app's footprint](data/screenshots/warcraft-rustcorder-intro.avif)

## Features

- **Automatic recording** of raid pulls, Mythic+, arenas, solo shuffle and
  battlegrounds, starting from before the activity was detected.
- **Library** you can sort, search, filter by date, tag and protect. Past the
  storage limit in Settings (50 GiB by default), the oldest unprotected
  recordings are deleted.
- **Player** with a combat timeline of deaths and encounters, playback speed,
  frame stepping and clipping.
- **Combat meter** over the video (`M`): damage, healing, deaths and more, per
  player, spell and target. Click a death to jump to it.
- **Runs in the tray** while you play.

About 65 MB of RAM and near-zero CPU while idle in the tray; video encoding
runs on the GPU.

## Install

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/JohanWes/wow-rustcorder/main/install.sh | bash
```

The script adds the project's signed Flatpak remote (and Flathub if needed),
installs and starts the app, and warns if your session lacks something
recording needs.

<details>
<summary>Install without the script</summary>

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak remote-add --user --if-not-exists warcraft-rustcorder \
  https://johanwes.github.io/wow-rustcorder/index.flatpakrepo
flatpak install --user warcraft-rustcorder io.github.JohanWes.WarcraftRustcorder
```

</details>

Requirements:

- A Wayland session (X11 is not supported) and Flatpak, with Flathub for the
  GNOME 50 runtime.
- `xdg-desktop-portal` with a ScreenCast backend for your desktop, and
  PipeWire.
- A GPU with hardware video encoding.
- For the tray icon on GNOME, the
  [AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/).
  Without a tray, closing the window quits the app.

Update by re-running the script or with
`flatpak update --user io.github.JohanWes.WarcraftRustcorder`. Uninstall with
`flatpak uninstall --user io.github.JohanWes.WarcraftRustcorder`; your
recordings stay in place. Add `--delete-data` to also remove settings.

The app used to be called Warcraft Recorder. An install from before the rename
moves over on its next update, settings included. Choose the combat-log and
recording folders again in Settings afterwards.

### Windows

Download `WarcraftRustcorder-<version>-setup.exe` from the
[latest release](https://github.com/JohanWes/wow-rustcorder/releases/latest)
and run it. It installs for your user only, without admin rights, and needs
Windows 10 (version 2004) or later. The installer is not code-signed, so
SmartScreen may ask you to choose **More info** and then **Run anyway**. The app
checks for updates itself. Uninstall it from **Settings > Apps**.

On Windows, recording is done by `wr-capture`, the project's own recorder of
about 600 KB. It captures with Windows Graphics Capture and encodes through
Media Foundation, on the GPU's hardware encoder when one is available.

## Setup

1. Open Settings and choose a **recording folder** and your World of Warcraft
   **Logs folder**, for example `.../World of Warcraft/_retail_/Logs`.
2. In WoW, enable **Advanced Combat Logging** under Options → System →
   Network. The status card warns when it is off.
3. Install an addon that starts combat logging when you enter an instance,
   such as
   [SimpleCombatLogger](https://www.curseforge.com/wow/addons/simplecombatlogger).
4. Leave the app running. Each activity appears in the library when it ends.

## Development

All code is one Cargo package under `native/`; the Windows capture helper is in
`wr-capture/`. See [`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md) for the checks
to run and [`docs/RELEASING.md`](docs/RELEASING.md) for releases.

Licensed GPL-3.0-or-later. Capture on Linux uses
[`gpu-screen-recorder`](https://git.dec05eba.com/gpu-screen-recorder/).
