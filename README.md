# <img src="data/icons/io.github.JohanWes.WarcraftRustcorder.svg" width="44" alt=""> Warcraft Rustcorder

Warcraft Rustcorder records World of Warcraft on Linux and Windows. It reads the
combat log, detects each activity and saves a video with a metadata file beside
it. It is a native Rust and GTK4 application, distributed as a Flatpak for
Wayland sessions and as a Windows installer
([download](https://github.com/JohanWes/wow-rustcorder/releases/latest)).

It started out inspired by [Warcraft Recorder](https://github.com/aza547/wow-recorder)
and has since grown into its own project: written entirely in Rust, with a
focus on low CPU, memory and disk use while you play.

![Intro: a timed Mythic+ key, the recording library, the combat meter with spell tooltips, target filter and death recap, the app's footprint, and the app on Windows](data/screenshots/warcraft-rustcorder-intro.avif)

## Features

- **Automatic recording** of raid boss pulls, Mythic+ dungeons, arenas, solo
  shuffle and battlegrounds. `gpu-screen-recorder` keeps a replay buffer, so
  each video starts before the activity was detected.
- **Library** with a category sidebar and a table you can sort by column and
  filter by search chips (player, spec, zone, encounter, result, difficulty)
  and date range. Recordings can be tagged, protected and deleted. The oldest
  unprotected recordings are removed when the library exceeds the storage
  limit in Settings.
- **Player** with a combat timeline showing deaths and encounter and round
  boundaries, playback speeds from 0.25x to 2x, frame stepping while paused
  (`,` and `.`), jumping between markers (`[` and `]`), and clipping.
- **Combat meter** over the video, toggled with `M`. It shows damage done,
  damage taken, healing, interrupts, dispels, casts, deaths and buff uptime,
  for the current fight or the whole recording. You can filter by target, open
  a player to see their spells, and click a death or event row to seek the
  video to a few seconds before it.
- **Background recording** from a tray icon. Closing or minimizing the window
  hides it to the tray by default, and Settings can start the app minimized.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/JohanWes/wow-rustcorder/main/install.sh | bash
```

The installer adds the project's signed Flatpak remote, installs the app and
starts it. If Flatpak is missing, it stops and prints the package command for
common distributions. It adds Flathub when the GNOME 50 runtime is not
available from another remote. After installing, it warns about an X11
session, a missing screen-capture portal or PipeWire not running.

Or add the remote and install directly:

```sh
flatpak remote-add --user --if-not-exists warcraft-rustcorder \
  https://johanwes.github.io/wow-rustcorder/index.flatpakrepo
flatpak install --user warcraft-rustcorder io.github.JohanWes.WarcraftRustcorder
```

Requirements:

- A Wayland session. X11 is not supported.
- Flatpak, with Flathub for the GNOME 50 runtime.
- `xdg-desktop-portal` with a ScreenCast backend for your desktop, and
  PipeWire.
- A GPU with hardware video encoding.

`gpu-screen-recorder`, the Clapper video player and FFmpeg are bundled in the
Flatpak.

### Windows

Download `WarcraftRustcorder-<version>-setup.exe` from the
[latest release](https://github.com/JohanWes/wow-rustcorder/releases/latest)
and run it. It installs for your user only, without admin rights, and needs
Windows 10 (version 2004) or later. The installer is not code-signed, so
SmartScreen may ask you to choose **More info** and then **Run anyway**. The app
checks for updates itself. Uninstall it from **Settings > Apps**.

## Setup

1. Open Settings and choose a **recording folder** and your World of Warcraft
   **Logs folder**, for example `.../World of Warcraft/_retail_/Logs`.
2. In WoW, enable **Advanced Combat Logging** under Options → System →
   Network. The status card warns when it is off.
3. Install an addon that starts combat logging when you enter an instance,
   such as
   [SimpleCombatLogger](https://www.curseforge.com/wow/addons/simplecombatlogger).
4. Leave the app running. Each activity appears in the library when it ends.

The tray icon uses the StatusNotifierItem protocol. GNOME needs the
[AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/)
to show it. Without a tray, closing the window quits the app.

## Footprint

Measured on an AMD Ryzen 7 9800X3D with a Radeon RX 9070:

| Measurement | Value |
|---|---|
| Window open, empty library | about 100 MB RSS, 24 threads |
| Tray only (started minimized) | about 59 MB RSS, 13 threads |
| CPU while idle | about 0% |
| Startup scan, 38 recordings | about 55 ms, about 1 MB above the empty baseline |
| Installed size | about 38 MB |
| First download | about 18 MB |
| Typical update | about 2 MB |

The installed size covers the app binary (about 5 MB), the spell database and
the bundled FFmpeg, `gpu-screen-recorder` and Clapper. The GNOME runtime is
shared with other Flatpaks and not included. The 23 MB spell database is a
separate file, so an update only downloads it again when it changes. Combat
meter data is loaded for the selected recording only. Video encoding runs on
the GPU through `gpu-screen-recorder`.

## Update and uninstall

Re-run the installer, or:

```sh
flatpak update --user io.github.JohanWes.WarcraftRustcorder
```

The app used to be called Warcraft Recorder. An install from before the rename
moves over on its next update, settings included. Choose the combat-log and
recording folders again in Settings afterwards.

To uninstall:

```sh
flatpak uninstall --user io.github.JohanWes.WarcraftRustcorder
```

Recordings are ordinary video and JSON files in your recording folder. An
uninstall does not remove them. Add `--delete-data` to also remove the app's
settings.

## Development

All code is one Cargo package under `native/`. From the repository root:

```sh
cargo fmt --manifest-path native/Cargo.toml --check
cargo clippy --manifest-path native/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path native/Cargo.toml --all-targets
cargo build --manifest-path native/Cargo.toml --release
```

See [`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md) and [`docs/RELEASING.md`](docs/RELEASING.md). Licensed GPL-3.0-or-later.
Capture uses [`gpu-screen-recorder`](https://git.dec05eba.com/gpu-screen-recorder/); based on the original [Warcraft Recorder](https://github.com/aza547/wow-recorder).
