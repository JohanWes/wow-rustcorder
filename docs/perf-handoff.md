# Performance handoff: next candidates

This lists the riskier performance changes worth exploring after v1.0.14. They
are ranked by expected payoff. Each one names what to measure first, because
two of them depend on numbers nobody has collected yet. All measurements were
taken on 2026-10-09 against the published v1.0.14 Flatpak on the
9800X3D / RX 9070 machine.

Keep AGENTS.md's KISS/YAGNI rule in mind: if the first measurement shows the
win is small, stop there.

## Baseline (v1.0.14)

| State | RSS | Split | Threads |
|---|---|---|---|
| Tray only (started minimized) | 59 MB | 15 anon / 44 file-backed | 13 |
| Window open, empty library | 101 MB | 25 anon / 76 file-backed | 24 |
| Window + `GSK_RENDERER=cairo` | 91 MB | 21 anon / 49 file / 21 shmem | 13 |
| Window + `GSK_RENDERER=ngl` | 195 MB | n/a | 26 |

The Flatpak already sets `MALLOC_ARENA_MAX=2` in finish-args, so allocator
arena tuning is done.

The following are measured with harnesses linked against `native/` as a path
dependency:

- Parser plus `ActivityEngine` on 600 MB of a real retail log (86 minutes of
  play, 1.6 M retained events): 1.6 s of CPU in total. That is **0.03% of one
  core** during play.
- `storage::load_meter` on a 14 to 17 MB sidecar: 25 to 30 ms, a peak of
  +28 to +32 MB, and **+15 MB retained** while the recording is selected.
- `SpellDb::parse`, which runs on the first meter view: 14 ms, an +8 MB index,
  plus about 13 MB of the mmapped JSON paged in.
- Sidecar sizes in the 38-recording library: median 4 KB, p90 14 MB, max 71 MB
  (one legacy pretty-printed file), 198 MB in total. Nearly all of it is
  `meter`.
- Copying a 4.2 GB recording inside the library takes 0.8 to 1.1 s through the
  host path. Through the document-portal FUSE path
  (`/run/user/1000/doc/...`, which is what the app uses) it takes 2.1 to 2.2 s,
  and `xdg-document-portal` spends 1.7 s of CPU.

Not measured yet: gpu-screen-recorder's RSS while WoW is running. That figure
needs the game open, and it is the first thing to collect for item 1.

## 1. Right-size the replay buffer (likely the largest memory win)

**What:** gsr holds the replay buffer in RAM by default
(`replay_storage: Ram`). Its size is bitrate × `replay_buffer_seconds`. That
is roughly 300 MB in your config (20 Mbps × 120 s) and roughly 450 MB with the
defaults (20 Mbps × 180 s), plus audio. It is held in a separate process for
as long as WoW runs.

**Why it is oversized:** the app only uses `late_by_ms + extra_lead_in_seconds`
of it (`coordinator.rs`, near line 881). `extra_lead_in_seconds` is capped at
30 s, and `late_by_ms` is the delay before the combat log reveals the
activity. Unless `late_by` is regularly over a minute, most of the buffer is
never read.

**Steps:**
1. Measure gsr's RSS with WoW running at 120 s and at 180 s, to confirm the
   arithmetic.
2. Add a `tracing::info!` with `late_by_ms` and `requested_replay_ms` where
   the pre-roll is computed. There is no capture-start log line today. Play
   normally for a week, covering M+, raid, arena and shuffle, then read
   `app.log`.
3. Set the default and the validation range in `config.rs` from the observed
   maximum plus margin (likely about 60 s). Existing configs keep their
   value, so decide whether to migrate values above the new default or leave
   them.

**Risk:** pre-roll is clamped to capacity, so a late detection would silently
get less lead-in. Pay particular attention to the `deferred_begin` path,
where the previous capture is still finalizing, because that is where
`late_by` grows.

**Alternative:** default to `replay_storage: Disk`. That costs about 2.5 MB/s
of continuous writes and keeps RAM flat. Combined with item 3, those writes
would also go through FUSE.

## 2. Tray mode without GTK: split into a daemon and a UI process

**What:** today the full shell is built even when starting minimized
(`ui/mod.rs`, near line 383: it is built and then `hide_to_tray()`). After
the window has been opened once, closing it to the tray keeps the window, its
widget tree and the Vulkan renderer alive.

The large but clean option is to run the coordinator, the tray and storage in
a GTK-free process. That process launches the GTK UI as a separate process on
Open and lets it exit on close. The UI talks to the daemon over a Unix socket
or D-Bus using the existing command/snapshot types, which are already the
coordinator's only interface (`domain.rs`).

**Expected:** most of the 44 MB of file-backed GTK/libadwaita/GStreamer pages
and most of the 15 MB anon go away while in the tray. The target is unknown:
**measure it before committing**. A throwaway `main` that runs only
`coordinator::start` and `TrayBackend` gives the floor in about an hour.

File-backed pages are shared and reclaimable. The honest framing is "looks
much smaller in system monitors" more than "frees 45 MB under memory
pressure".

**Cheaper intermediate step** (do this first, and stop there if it is good
enough):
- Do not build the shell when starting minimized; build it on the first Open.
- Destroy the window, rather than hiding it, on close-to-tray, and rebuild it
  on Open.

Measure first the RSS after open → close-to-tray, which is not in the
baseline above. If it stays at about 100 MB, this step recovers the anon and
renderer share. It does not recover the mapped libraries.

**Risk:**
- Rebuilding the window loses UI state: selection, scroll position, filters
  and player position. Persist whichever of those matter.
- The single-instance and activation path in `main.rs` needs care.
- The daemon split also changes the shutdown ordering that `main.rs` relies on
  (the tray goes down first, then the coordinator flushes gsr).

## 3. Stop routing capture and finalize I/O through the document portal

**What:** the recording directory is a portal grant. gsr writes, and the
finalize job's FFmpeg trim+concat reads and rewrites the whole recording,
through `xdg-document-portal`'s FUSE mount. That makes a 4 GB finalize copy
roughly 2× slower and costs about 1.7 s of portal-daemon CPU per 4 GB.

**Options:**
- **(a)** Add `--filesystem=xdg-videos` (or similar), so that the default
  location bypasses FUSE. This is a sandbox-permission change. Weigh it
  against the Flathub review stance and the lint exceptions.
- **(b)** Put the capture/staging root (the `separate_buffer_dir` machinery
  already exists) in the app's own data dir. That data dir is not FUSE. Then
  write only the final file into the portal path. This does not reduce I/O,
  but it moves gsr's live writes off FUSE.
- **(c)** The bigger win is to not rewrite the recording at all. Finalize
  currently writes every recording twice: once by gsr and once by the concat.
  That doubles SSD writes, about 4 GB extra per long key. Research whether
  current gsr can start a regular recording seeded with the last N seconds of
  the replay buffer. If it can, the trim+concat job (`media_jobs.rs`, near
  line 243) and its temp files disappear.

**Test:** time "end of activity → recording appears in library" on a 4 GB
capture before and after. Also check that portal-granted non-default
locations still work.

## 4. Compact meter storage

**What:** each 500 ms sample is a JSON object with repeated keys (`at_ms`,
`amount`, `hits`, `overheal`, `min`, `max`) per spell and per target per actor
per fight. A 15 MB sidecar is >99% this.

Store samples columnar instead, for example
`{"start": 460500, "step": 500, "amount": [...], "hits": [...], ...}` with
implicit timestamps and sparse gaps encoded. Alternatively, move the meter to
a separate `.meter` file in a compact form, which lets the sidecar stay tiny
and human-readable.

**Expected:**
- Sidecars shrink about 5 to 10× (more if compressed).
- `load_meter` drops from about 25 ms to a few ms.
- The +15 MB retained per selected recording drops if the in-memory
  `MeterData` also stores samples as packed `Vec<u32>` instead of
  per-sample structs.

**Risk:** this is a schema change.
- Old sidecars must keep loading. Keep the old deserializer behind
  `schema_version`.
- A background one-time rewrite must use temp-file+rename, and it must never
  run while a recording is being finalized.
- The goldens in `tests/native/` need regenerating.

Rewriting the 71 MB legacy sidecar also removes the +67 MB scan peak it causes
on every start.

## 5. Spell DB without a parse step

**What:** on the first meter view the app parses 13 MB of JSON into an
86,738-entry `HashMap`: 14 ms, +8 MB index, and about 13 MB of the mapping
paged in. The alternative is to generate a sorted binary index at build time,
for example name → (description offset, icon offset), in
`scripts/fetch-spell-data.py` or `build.rs`. Then binary-search it straight
out of the mmapped gresource.

**Expected:** about 20 MB less RSS after the first meter view, and zero parse
time.

**Risk:** low, but it adds a generated format to maintain. Do this only if
items 2 and 4 make meter-view RSS the thing people notice.

## Measured and not worth pursuing

- **Combat log parsing and activity engine:** 0.03% of a core. Interning
  strings or a zero-copy parser would not show up anywhere.
- **`GSK_RENDERER=cairo`:** saves about 10 MB and 11 threads with an empty
  window, but composites video on the CPU. Only revisit this after testing
  1440p60 AV1 playback, and probably not even then.
- **`GSK_RENDERER=ngl`:** about 2× RSS. Keep the default Vulkan renderer.
- **Allocator swap (mimalloc/jemalloc), more arena tuning:** the arena limit
  is already set, and an empty window's anon heap is about 25 MB. There is
  little to win.
- **Replacing the 50 ms coordinator poll with inotify:** idle CPU is already
  about 0.1%.

## Measurement recipes

The RSS split comes from `/proc/<pid>/status` (`RssAnon`, `RssFile`,
`RssShmem`), read 20 s after launch. Use a throwaway config so the
release-notes view does not inflate the number. Flatpak resets
`XDG_CONFIG_HOME`, so set it inside the sandbox:

```bash
flatpak run --command=sh io.github.JohanWes.WarcraftRustcorder \
  -c "GSK_RENDERER=... XDG_CONFIG_HOME=$HOME/.var/app/io.github.JohanWes.WarcraftRustcorder/qa/config exec warcraft-rustcorder"
```

The throwaway config is a copy of the real one with:
- `last_seen_version` set to the installed version
- `storage.recording_dir.path` pointed at an empty dir under
  `~/.var/app/<id>/`
- `interface.start_minimized` set as needed

Delete it afterwards.

For library-level timings, use a small binary crate depending on
`warcraft-rustcorder = { path = ".../native" }` that calls `Storage::scan`,
`storage::load_meter`, `SpellDb::parse`, or `parser::parse_line` +
`ActivityEngine::handle`, and reads `VmHWM`/`VmRSS` around the call. Build
these outside the repo.
