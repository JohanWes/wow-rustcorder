# Releasing

Flatpak is the only native release format. The stable application ID is
`io.github.JohanWes.WarcraftRecorder`; the permanent signed remote is
`https://johanwes.github.io/wow-recorder-linuxwayland/`.

## Candidate build

1. Update `native/Cargo.toml` and the first `<release version="...">` in
   `data/io.github.JohanWes.WarcraftRecorder.metainfo.xml` to the same version.
2. Run `bash scripts/generate-release-notes.sh <version>` as the last thing
   before the release commit. It prepends the commit subjects since the
   previous tag to `data/release-notes.md`, which is compiled into the binary
   and shown once, after the update, by the "What's new" dialog. The release
   commit itself is not in the list; keep it a version-bump-only commit.
3. Run the four standard Rust checks from the root.
4. Create an annotated tag whose name is exactly `v<version>` and push it. The
   tag workflow rejects a version whose Cargo, AppStream, or release-notes
   entries disagree before building.
5. Configure the `release` environment secrets:
   `FLATPAK_GPG_PRIVATE_KEY` contains the armored private key and
   `FLATPAK_GPG_KEY_ID` contains only its public key ID. Never commit the key
   or print either secret.
6. Download the workflow's release-candidate artifact and verify the recorded
   SHA-256. The bundle is suitable for disposable-user testing and does not
   configure a remote.

CI builds from the locked `native/Cargo.lock` and `flatpak/cargo-sources.json`,
uses the pinned GNOME 50 SDK/runtime, runs `flatpak-builder-lint` for manifest,
AppStream, and repository, and exports a signed static OSTree repository plus
one `.flatpak` bundle. The documented linter exceptions in
`flatpak/lint-exceptions.json` are intentional: the canonical reverse-DNS ID,
and the Wayland-only product scope. Rebuilding from the same commit should
produce the same application payload; OSTree/bundle container metadata may
vary by build time.
The AppStream screenshot is served from the committed `main` tree, while the
candidate repository also carries its mirrored `screenshots/x86_64` ref.

## Manual remote publication

Never run this for an unapproved candidate. After a candidate is approved,
publish the `repo/` directory produced by that build to the project GitHub
Pages site at the permanent URL. Keep
`index.flatpakrepo`, `summary`, the signed summary, objects, and static deltas
together. Verify with only the public key:

```sh
gpg --import public-release-key.asc
flatpak remote-add --user --if-not-exists warcraft-recorder \
  https://johanwes.github.io/wow-recorder-linuxwayland/index.flatpakrepo
flatpak install --user warcraft-recorder io.github.JohanWes.WarcraftRecorder
flatpak remote-info --user --show-commit warcraft-recorder \
  io.github.JohanWes.WarcraftRecorder
```

If the signing key is lost, users must remove and re-add the remote with a new
public key. There is no rotation framework or staging remote.

## Install, update, rollback, uninstall

For a stable install, add the permanent remote and install the application as
shown above. Update with `flatpak update --user`. For candidate testing use:

```sh
flatpak install --user ./warcraft-recorder.flatpak
```

To roll back to the previous signed deployment, inspect the remote log and
deploy its previous commit:

```sh
flatpak remote-info --user --log warcraft-recorder \
  io.github.JohanWes.WarcraftRecorder
flatpak update --user --commit=<previous-commit> \
  io.github.JohanWes.WarcraftRecorder
```

Uninstalling the app does not delete recordings:

```sh
flatpak uninstall --user io.github.JohanWes.WarcraftRecorder
```

Use `--delete-data` only when deleting the native app's private data is
intentional.

## Windows

The tag job also builds `WarcraftRecorder-<version>-setup.exe` (Inno Setup,
per-user, no admin) and `latest.json` on a Windows runner with
`windows/build.sh`, and attaches both to the GitHub release. The installer is
not code-signed yet, so SmartScreen warns on first run.

Installed apps check
`https://johanwes.github.io/wow-recorder-linuxwayland/windows/latest.json` at
startup and from "Check for updates", then download the installer, verify its
SHA-256, and run it silently. As with the Flatpak repo, nothing reaches users
until that file lands: copy the release's `latest.json` to `windows/` on the
`gh-pages` branch together with the Flatpak `repo/` publication.

To build locally, run `bash windows/build.sh` in an MSYS2 UCRT64 shell with
the packages listed at the top of the script and Inno Setup 6. Output lands in
`build-windows/`. Uninstalling leaves config and recordings in place.
