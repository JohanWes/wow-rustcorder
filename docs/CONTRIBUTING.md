# Contributing

Warcraft Rustcorder is developed as one Rust package under `native/` and is
packaged as a Flatpak. GTK widgets stay on the GTK main thread; the
coordinator owns domain state and one serialized worker performs blocking
media/storage work. Keep core modules free of GTK imports and use typed
commands/snapshots over bounded standard-library channels.

## Build and test

From the repository root:

```sh
cargo fmt --manifest-path native/Cargo.toml --check
cargo clippy --manifest-path native/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path native/Cargo.toml --all-targets
cargo build --manifest-path native/Cargo.toml --release
```

The development Flatpak is the correct environment for capture, playback,
native choosers, GSR, and FFmpeg behavior. Do not add a webview, async
runtime, database, generic IPC layer, thumbnail cache, or compatibility
wrapper. JSON sidecars and the filesystem remain the library source of truth.

The native test fixtures and goldens live under `tests/native/`. The legacy
sidecar fixtures are retained because they prove the compatible tag/protection
patch behavior.

## Packaging

Use `flatpak/io.github.JohanWes.WarcraftRustcorder.Devel.yml` for development
iteration. Release builds use the stable manifest, the locked
`flatpak/cargo-sources.json`, and the tag-triggered workflow in
`.github/workflows/flatpak-release.yml`.

The release workflow checks the Rust package and AppStream version, builds
offline from pinned inputs, runs the three Flatpak Builder lints, signs the
OSTree repository with the project key held in CI secrets, and uploads a
candidate bundle. It never publishes the permanent remote automatically.

Keep English strings next to their widgets.
