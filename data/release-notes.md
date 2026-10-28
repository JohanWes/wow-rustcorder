# Release notes

Commit subjects per release, written by `scripts/generate-release-notes.sh` and
compiled into the binary: the "What's new" dialog reads the section matching the
running version. Only `## <version>` headings and `- ` lines are parsed.

## 1.0.16
- Add wr-capture, a Windows replay-buffer recorder speaking the GSR protocol
- Port the GTK app to Windows, driving wr-capture over stdin/stdout
- Add AV1 recording via Matroska and a Windows 10 desktop-audio fallback
- Add Windows installer, in-app update check, and Windows release CI job
- Add Windows tray, window snapping, larger font, and diagnostics logging
- Enable GPU rendering on Windows, fix tray restore, pause startup video
- Size the replay buffer from the lead-in and drop buffer settings
- Keep domain imports on Windows after dropping ReplayStorage
- Capture Classic and PTR windows; log frame, usage and GPU stats
- Check bundled DLL imports, list bundle files, drop gst-libav and cursors
- Treat a missing Windows update manifest as no update
- Watch an empty combat log folder until WoW creates the first log
- Stop Apply sticking on Applying when a save is rejected or renamed
- Read each log file's own time when checking whether WoW runs
- Resync audio drift, follow default devices, bound replay ring, cut GPU copies
- Minimize to the taskbar by default on Windows so Win+D restores
- Handle Win+arrow, system menu and window placement on Windows
- Record only the game's client area of a windowed WoW
- Leave Ctrl, Alt and Super key combinations to the system
- Close the running app on uninstall and remove the plugin cache
- Release the playing recording before deleting it on Windows
- Capture at up to twice the recording rate so frames are not skipped
- Name wr-capture instead of gpu-screen-recorder in Windows error messages
- Rebuild the GStreamer registry when the GPUs change so hardware decoding works
- Tell the user to pick another codec when the GPU cannot encode it
- Strip the Windows app binary and drop the unused librsvg pixbuf loader
- Add Windows installation instructions to the README
- Use lzma2/ultra64 compression to shrink the Windows installer
- Rename the project to Warcraft Rustcorder and migrate existing Flatpak installs
- Keep stereo on the front speakers of 5.1 and 7.1 Windows devices
- List and record specific audio devices on Windows

- Download Windows updates in the app and install them invisibly
## 1.0.15
- Update README typical update size measured on the 1.0.14 release
- Save only the replay the pre-roll needs and skip it at zero
- Remux regular-only recordings to MP4 instead of copying the MKV
- Lower the default replay buffer from 180 to 60 seconds
- Log the detection delay, pre-roll and replay save at capture start
- Correct sidecar load comment: non-compact sidecars are read whole
- Bump bundled gpu-screen-recorder from 5.13.9 to 6.1.3
- Drive gpu-screen-recorder through its IPC socket instead of signals
- Concatenate the measured replay without the ffmpeg trim
- Drop the gsr hook sandbox patch now that no hook runs
- Cap player queues at 8 MiB from creation to cut playback memory
- Log the real combat-log delay at capture start
- Add performance handoff notes

## 1.0.14
- Read native sidecars only up to the meter during library scan
- Copy the meter through as raw JSON when protecting or tagging
- Simplify storage: derive sidecar probe, drop parallel correlation starts
- Fold library recount into enforce_limit and simplify local clock
- Trim redundant storage and coordinator tests, use sparse files and test_root
- Speed up vertical slice and drop its redundant tests and projection asserts
- Load config once, default missing keys, and keep rejected files as .bad
- Skip build output and screenshots in flatpak dir sources, update test README
- Parse the bare id|label audio list gpu-screen-recorder actually prints
- Resolve a pending end at once when the GSR child dies
- Log recorder noise directly, throttle token reads, truncate gsr.log on arm
- Drop log diagnostics queue and long-line guard, warn once per error
- Simplify FFmpeg job plumbing, idle longer, and warn on replay fallback
- Borrow combat-log fields instead of allocating, and trim parser tests
- Deduplicate activity engine code and fix region and boss percent edge cases
- Borrow spell database strings from the resource and decode icons from bytes
- Cut meter per-event allocations, fold duplicate helpers, and drop dead code
- Fold duplicated damage meter UI blocks and drop low-value tests
- Avoid quadratic id scans in library delete, protect, and suggestions
- Share one clock formatter so long recordings read h:mm:ss everywhere
- Share one elapsed label that stops ticking while unmapped
- Drop the timeline key controller the window shortcuts already shadow
- Remove dead apply branch, unused test-category data, and bell argument
- Share label column, icon button, box clearing, and plain name helpers
- Swap a backwards library date range instead of ignoring it
- Clear the settings busy warning once the recorder is idle
- Trim removal-history notes from player and viewpoint module docs
- Trim UI tests that cover std behaviour, dead branches, or nothing
- Import format_clock directly in damage meter and drop the alias
- Label audio devices by description, falling back to the device id
- Remove dead class_id fields from player and combatant summaries
- Ignore sidebar auto-selection before the first snapshot sets the category
- Route SIGTERM through the graceful shutdown path like tray Quit
- Classify audio sources by .monitor suffix so virtual mics list as inputs
- Update README footprint table with remeasured memory and scan time

## 1.0.13
- Update intro video with smoother meter transitions
- Run replay buffer only while World of Warcraft runs
- Cap player demuxer queue so playback stops loading whole files

## 1.0.12
- Show the damage meter's Target menu entry in full
- Give app icon an orange legendary border
- Draw the tray icon from the app's own icon so it matches the running build

## 1.0.11
- Write native sidecars as compact JSON
- Move spell data to a separate mmapped gresource and align embedded bundle
- Enable fat LTO and one codegen unit in Flatpak release builds
- Shift per-spell target samples when finalizing the meter
- End an overrunning capture early so a superseding activity is kept
- Reset the capture restart backoff after a stable child exits
- Run test recordings in Test mode so the status card labels them
- Drop x264, swscale, avdevice and unused filters from the FFmpeg build
- Remove the no-op ActivityAction::Update timeline action
- Drop the always-equal Begin.detected_at_ms from activity actions
- Load damage meters on demand instead of holding them in the index
- Drop unread recorder API: StartRequest.mode, CaptureStarted, RestartScheduled.at_ms
- Scan the library once at startup and sweep against that scan
- Remove never-produced Reconfiguring, Buffering and Fatal recorder statuses
- Drop test-only LogTailer replay mode and path accessor
- Limit runtime sweeps to failed captures and wait for idle media work
- Log skipped sidecars on rescan and drop unused storage API
- Trim Complete and Abandon actions to the id the coordinator reads
- Stop parsing combat event fields the activity engine never reads
- Resolve the config path once in Setup::from_environment
- Move the recorder's log tail helper into process.rs
- Use storage::now_unix_ms instead of the recorder's own copy
- Define EMPTY_GUID once in the parser and share it
- Share the raid-marker read and the utility-event arm in the parser
- End classic arenas through the battleground death-count path
- Move activity data tables and MD5 into activity/tables.rs
- Unload the player when a rebuild leaves the library table empty
- Select the solo-shuffle round roster through one helper pair
- Switch sidebar category on selection so arrow keys navigate
- Keep player shortcuts out of dialogs and popovers
- Drain the shell only on wakes and drop the 250 ms poll
- Use one shared row context menu and connect cell handlers once
- Reuse unchanged library row objects so rebuilds keep focus and selection
- Break reference cycles that leaked the Settings dialog on close
- Show a loading page until the first library snapshot arrives
- Repaint the timeline only when the playhead moves half a pixel
- Refresh the viewpoint selector when the same activity is reselected
- Animate library transitions and flip the protect star immediately
- Keep a single fullscreen idle tick across quick fullscreen toggles
- Crossfade between the player placeholder and the video
- Replace the busy banner with toasts and toast clip and delete results
- Fade the status light and drop dead CSS and sidebar row fields
- Update damage meter rows in place instead of rebuilding every tick
- Animate meter bar fills with libadwaita timed animations
- Throttle playhead-driven meter refreshes to ten per second
- Create the Clapper backend on the first load instead of at startup
- Drop meter hover and click workarounds that rebuilt rows needed
- Parse the spell database off the GTK thread with boxed strings
- Defer loading selections until the player is shown, not in the tray
- Reuse shared log tail helper in media jobs
- Drop unused wr-timeline CSS class from timeline widget
- Connect audio device combo handlers once instead of per refresh
- Add Unreleased changelog section for the improvement sprint
- Rewrite README with verified features, setup and measured footprint
- Bump version to 1.0.11 with release notes

## 1.0.10
- Retire the AppImage migration release
- Add buffs tab with per-player buff uptime to meter
- Cut duplicated test boilerplate and dead derived state
- Remove AppImage and Electron config migration code
- Drop legacy Bloodlust backfill and legacy_ids tracking
- Speed up sidecar parsing and keep coordinator responsive during bulk edits
- Drop the Electron sidecar reader and code it kept alive
- Restore a leaner Electron sidecar reader
- Drop the folded Other buff row and show buff icons

## 1.0.9
- Claim the single instance before touching storage
- Release the spell borrow before clearing it
- Skip hidden meter refreshes and virtualize the histories
- Defer seeks until Clapper reports the item ready
- Fold completions into the library index instead of rescanning

## 1.0.8
- Correct the documented application sizes
- Replace the GSR child when a capture produces no video file
- Stop the GSR recording toggle from desyncing
- Prove the next recording saves after a desync
- Use as_chunks for the MD5 block loop

## 1.0.7
- Add focused combat review tools
- Add resizable combat damage meter
- local dmg meter improvements
- Improve local damage meter accuracy and menus
- Sync local meter fights to capturing player combat
- Keep group meter fight active after host death
- Simplify combat meter navigation controls
- Add damage taken and death meter views
- Scope death logs to their fight
- Draw death log rows as draining health bars
- Fix meter row clicks and solidify bar fills
- Add per-spell statistics and target split to meter
- Add casts meter view, overheal detail, and seekable meter rows
- Remove meter row hover highlight and stray images
- Animate meter bar fills between samples
- Strip UTC-offset suffix from combat log timestamps before parsing
- Add spell icons and stable tooltips
- order by spell %
- remove casts in non detailed view
- Document the local combat meter
