# Changelog

Notable changes to the native Linux/Wayland application. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release history before the native rewrite belongs to the upstream Electron
project, [aza547/wow-recorder](https://github.com/aza547/wow-recorder).

## 1.0.17 - 2026-10-28

### Added
- Theme setting in Settings: System (follows the desktop, the default), Dark
  or Light.

### Fixed
- The player no longer hangs on pause, including after seeking in HEVC
  recordings.
- Storage cleanup skips recordings that are in use instead of stalling the app.
- Windows: popovers no longer show black arrows and corners.
- Windows: no orphan recording opens after a cancelled start.
- Windows: the window shows after an in-app update, even with "Start
  minimized" on.

### Changed
- Windows: only the WoW executables of enabled flavors are captured.
- Windows: updates are only accepted from the project's GitHub release
  downloads.

## 1.0.16 - 2026-10-28

### Added
- Windows support: a per-user installer (`WarcraftRustcorder-1.0.16-setup.exe`)
  with in-app updates that download behind a progress bar and install
  invisibly, then restart the app. Recording uses `wr-capture`, a Windows replay-buffer
  recorder that captures the WoW window with Windows Graphics Capture and
  encodes on the GPU (H.264, HEVC, AV1). Audio can be desktop audio, the game's
  audio only, or any listed output or input device. The app has a tray icon and
  records Retail, Classic and PTR clients.

### Changed
- Renamed to Warcraft Rustcorder (app ID `io.github.JohanWes.WarcraftRustcorder`,
  repository `JohanWes/wow-rustcorder`). Existing installs move over on their
  next `flatpak update`, settings included; choose the combat-log and recording
  folders again in Settings.

## 1.0.15 - 2026-10-10

### Changed
- Starting a recording saves only the lead-in it needs from the replay buffer
  instead of the whole buffer, which was hundreds of megabytes written per
  capture, and saves nothing when no lead-in is needed.
- Finishing a recording joins the lead-in to the recording without a separate
  trim pass. The lead-in can be up to one keyframe interval longer than
  configured, and the timeline is placed against its measured length.
- The bundled gpu-screen-recorder is updated from 5.13.9 to 6.1.3 and is
  driven through its IPC socket instead of signals and a hook script.
- The default replay buffer is 60 seconds instead of 180, so new installs hold
  a third of the buffer in RAM while World of Warcraft is open. Existing
  settings keep their value.
- Playing a recording uses less memory: the player's queues are capped at
  8 MiB each instead of 64 MiB.
- The log records the combat-log delay, lead-in and replay save for every
  capture start.

### Fixed
- A recording without lead-in is stored as MP4 like every other recording
  instead of a copy of gpu-screen-recorder's Matroska file.

## 1.0.14 - 2026-10-09

### Changed
- The library startup scan is about 3x faster (165 ms to 55 ms on a
  38-recording library), protecting or tagging a large recording is faster,
  combat-log parsing is about 2.5x faster with far fewer allocations, memory
  use drops once the damage meter opens, and the app wakes up less when idle.

### Fixed
- Audio settings list the real output and input devices by name instead of
  only the defaults, and virtual microphones such as EasyEffects appear as
  inputs.
- The selected sidebar category is remembered across launches instead of
  resetting to 2v2.
- Logging out or sending SIGTERM (`systemctl`, `kill`) shuts the app down
  cleanly and stops gpu-screen-recorder instead of leaving it running.
- Recordings longer than an hour show h:mm:ss consistently in the player and
  timeline.
- The player's region is now recorded for combatants.
- A date filter with From after To swaps the dates instead of doing nothing.
- The "settings cannot be applied while recording" warning clears once the
  recorder is idle.
- A config file with a bad value is no longer silently replaced by defaults:
  missing keys fall back to defaults and a rejected file is kept as
  `config.json.bad`.
- Recovery is faster when gpu-screen-recorder exits mid-stop, FFmpeg
  replay-trim fallbacks are logged, and the warning for a disappearing Logs
  folder is logged once instead of repeatedly.

## 1.0.13 - 2026-10-03

### Changed
- The replay buffer only runs while World of Warcraft is running. It starts
  when WoW writes to its Logs folder and stops after 10 minutes without any
  write there, so the GPU's video encoder is no longer busy while the game is
  closed. Manual and test recordings still work with WoW closed.

### Fixed
- Playing a recording no longer loads the whole file into memory (about
  5 GB for a long Mythic+ key). GStreamer's AV1 parser leaves most frames of
  these recordings untimestamped, so playbin3's time-limited demuxer queue
  never filled; it now also has a 64 MiB byte limit.

## 1.0.12 - 2026-09-29

### Changed
- The app icon, sidebar logo and tray icon have an orange legendary-item
  border. The tray now draws the icon built into the app instead of looking
  it up in the installed icon theme, so it always matches the running build.

### Fixed
- The damage meter's right-click menu shows its Target entry in full instead
  of cutting it off behind a scrollbar.

## 1.0.11 - 2026-09-26

### Changed
- Damage meters load on demand for the selected recording instead of staying
  in memory for the whole library. On a 58-recording library, idle memory
  after the startup scan drops from 415 MB to 30 MB and the scan from 2.4 s to
  1.0 s; startup reads each sidecar once.
- Sidecars are written as compact JSON, about a third of the size. Existing
  ones are rewritten the next time they are tagged or protected.
- The spell database ships as a separately installed, memory-mapped resource:
  the binary shrinks from 29 MB to 5 MB, an update downloads about 5 MB instead
  of 17 MB, and about 40 MB less memory is used.
- The bundled FFmpeg drops x264, swscale, avdevice and unused filters, and
  Flatpak release builds use link-time optimization.
- The video backend starts on first playback. An empty-library window idles at
  147 MB and 31 threads instead of 186 MB and 53; a tray-only session at 57 MB
  and 12 threads.
- The combat meter updates rows in place with animations, throttles refreshes
  while scrubbing, and loads the spell database off the UI thread.
- The library keeps focus, scroll and selection across refreshes, shows a
  loading page at startup, crossfades between states, flips the protect star
  immediately, and reports busy state, clips and deletions as toasts instead
  of a layout-shifting banner.
- The window no longer polls four times a second, and the timeline repaints
  only when the playhead moves.

### Fixed
- A raid pull or Mythic+ run that started while the previous activity was
  still ending was silently dropped; both are now recorded.
- The gpu-screen-recorder restart backoff resets after a minute of stable
  running, so a later crash no longer leaves capture unarmed for 30 s.
- Discarding a capture while a save was queued or running could move that
  save's files to Recovery and fail it. Cleanup now waits for media work to
  finish and only touches the failed capture.
- Per-spell target samples in newly recorded meters were one lead-in early.
- Test recordings are labelled as such in the status card.
- Starting minimized no longer loads and plays the newest recording in the
  background.
- Player shortcuts no longer fire inside dialogs and popovers.
- Deleting the last visible recording unloads it from the player.
- Arrow keys in the category sidebar switch the category.
- The viewpoint selector refreshes when a viewpoint is added to the selected
  activity.
- The Settings dialog and row context menus no longer leak memory on every
  open.
- Unreadable sidecars are logged instead of being skipped silently.

### Removed
- Dead code: never-produced recorder statuses, the no-op timeline action,
  unread combat-event fields and recorder API, and test-only log tailer modes.
  Activity data tables moved to their own module.

## 1.0.10 - 2026-09-09

### Added
- The combat meter gains a Buffs tab with per-player buff uptime, and the
  buff list shows buff icons instead of a folded "Other" row.

### Changed
- Startup and bulk library edits are faster. Orphan detection and sidecar
  loading no longer build throwaway JSON trees, the scan orders entries
  without cloning them, and tagging or protecting many recordings lets the
  coordinator service the recorder and combat log between writes. On a
  498 MiB library the pre-library-snapshot work drops from 2.2 s to 1.1 s at
  half the peak memory, and protecting 37 recordings from 2.1 s to 0.79 s.

### Removed
- The AppImage migration path and the one-time Electron config import with its
  post-migration notice are gone. Every user is on the Flatpak now.
- The one-time startup backfill that rewrote old Electron sidecars with
  Bloodlust timelines parsed from historical combat logs is gone. Sidecars
  that were already enriched keep their timeline.
- A recording made by the old Electron application whose category is not one
  this application knows is now skipped instead of being listed under an
  "Unknown" category. Recordings in the normal categories are unaffected.
- Old Electron recordings no longer report a video codec in the library. Every
  other detail they carry is unchanged.
- Dead code left behind by earlier removals: the imported-path constructor
  orphaned by the Electron config import, an unused spell-database size
  accessor, a write-only first-time-setup flag, a duplicate spell-data fetch
  script, and a development-only meter replay example.

## 1.0.9 - 2026-09-06

### Fixed
- A seek issued while Clapper is still prerolling is deferred until the item
  is ready, instead of dropping it and wedging every later seek for that
  video.
- Scrubbing back past the first cast of a selected meter spell no longer
  panics the process.
- A second launcher invocation claims the single-instance lock before any
  storage work, so it no longer moves the first instance's active recording
  into Recovery.

### Changed
- Finishing a recording, deleting, and evicting now update the library index
  in place instead of rescanning the whole library, which blocked the
  coordinator and allocated heavily on large libraries.
- A hidden combat meter no longer rebuilds its widgets on every tick, and the
  occurrence and death histories are virtualized, keeping large meters cheap.

## 1.0.8 - 2026-09-03

### Fixed
- Captures that begin and end in the same moment no longer desynchronize the
  gpu-screen-recorder recording toggle, which had silently stopped every later
  capture from producing a file.
- A capture that produces no video file now replaces the recorder child, so
  the next recording still saves instead of going silent.

## 1.0.7 - 2026-08-21

### Added
- A local combat meter is available alongside each recording, with damage
  done, damage taken, healing, interrupts, dispels, casts and deaths. It
  supports current-fight and overall views, player spell and target
  breakdowns, and seeking from meter rows into the video.
- Previous and next controls jump between the visible combat-timeline markers.

### Changed
- Player controls now focus on video and combat review; the drawing overlay
  has been removed.

### Fixed
- Combat logs with the newer UTC-offset timestamp suffix are parsed instead of
  silently rejecting every event.

## 1.0.6 - 2026-08-13

### Added
- Midnight patch 12.1 raid encounters and Mythic+ dungeons are now recognized
  and recorded.

## 1.0.5 - 2026-08-10

### Fixed
- Resolved screen-capture crashes no longer leave stale problem indicators
  after automatic restart, rearming, or capture-target reselection.

## 1.0.4 - 2026-08-09

### Fixed
- Combat-log timestamps now use the system timezone and daylight-saving
  changes instead of UTC.
- A stale saved-recording event can no longer be attached to a newer
  recording.

## 1.0.3 - 2026-08-08

### Fixed
- Restarting the media worker no longer deadlocks when the worker is busy:
  the shutdown request now waits to be delivered instead of being dropped.
- Folder validation no longer overwrites an existing probe file: the write
  probe uses an exclusive unique name and reports write failures.

### Changed
- `install.sh` is the documented one-command install: on a machine without an
  AppImage it only adds the remote, installs the app, and starts it, and a
  re-run updates an existing install instead of failing. Missing Flatpak now
  prints the command that installs it, and the installer warns when the
  session cannot record: X11, no screen-capture portal, or no PipeWire, each
  with the fix spelled out.
- README rewritten for players: one install command, the three things the
  system needs, first-run steps, and measured footprint against the Electron
  build.

## 1.0.2 - 2026-07-28

### Added
- A "What's new" dialog on the first start after an update, listing the
  commits between the previous release and the installed version. Closing it
  records the version, so it appears once per update.

## 1.0.1 - 2026-07-28

### Fixed
- The post-migration notice stayed pending after being dismissed, so it
  reappeared on every start: a settings save carried the draft the notice
  itself had opened Settings on, writing the pending flag back.
- "Advanced combat logging is off" was reported for every sandboxed install.
  The check reads `Config.wtf` beside the Logs folder, which the folder portal
  does not export, so an unreadable file now reads as unknown rather than off.
- The AppImage migration left the old app running and its binary on disk.

### Added
- The recording folder and combat-log folder rows pulse until they are
  selected, including after a migration, where the imported paths need picking
  again before the sandbox can reach them.

## 1.0.0 - 2026-07-27

### Added
- Native Rust/GTK4 application: combat-log watching, activity detection,
  `gpu-screen-recorder` capture, JSON sidecar library, playback with a combat
  timeline, clipping, drawing overlay, and local POV switching.
- Live keystone timers from the API when computing a Mythic+ result, with
  hardcoded timers as a fallback.
- A one-time notice on the first launch after a legacy import: what changed,
  what carried over, and the two folders the sandbox needs selected again.

### Changed
- Flatpak is the only install and update path. Recordings and configuration
  stay local; legacy configuration is imported once and left untouched.
- Cloud, account, upload, and localization features are not part of this fork.
- `install.sh` migrates an AppImage install instead of replacing it: it
  installs the Flatpak, preserves the AppImage for rollback, retires the
  AppImage launchers, and starts the native app.
