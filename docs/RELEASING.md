# Releasing

Linux releases ship as a Flatpak; Windows releases as an installer (see
[Windows](#windows)). The stable application ID is
`io.github.JohanWes.WarcraftRustcorder`; the permanent signed remote is
`https://johanwes.github.io/wow-rustcorder/`.

## Candidate build

1. Update `native/Cargo.toml` and the first `<release version="...">` in
   `data/io.github.JohanWes.WarcraftRustcorder.metainfo.xml` to the same version,
   add a `CHANGELOG.md` entry, and run `cargo build --release` so
   `native/Cargo.lock` records the new version.
2. Run `bash scripts/generate-release-notes.sh <version>` as the last thing
   before the release commit. It prepends the commit subjects since the
   previous tag to `data/release-notes.md`, which is compiled into the binary
   and shown once, after the update, by the "What's new" dialog. The release
   commit itself is not in the list; keep it a version-bump-only commit.
3. Run the four standard Rust checks from the root.
4. Create an annotated tag whose name is exactly `v<version>` and push it. The
   tag workflow rejects a version whose Cargo, AppStream, or release-notes
   entries disagree before building, and ends by creating a **draft** GitHub
   release with the `.flatpak` bundle, its checksums, the Windows installer
   and `latest.json`. The bundle is suitable for disposable-user testing and
   does not configure a remote.

The signing secrets live in the `release` environment:
`FLATPAK_GPG_PRIVATE_KEY` contains the armored private key and
`FLATPAK_GPG_KEY_ID` contains only its public key ID (`8FAF1364A2686F0F`; the
private key is backed up outside the repository). Never commit the key or
print either secret.

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

## Publishing

Publishing the draft release ships it:

```sh
gh release edit v<version> --draft=false --repo JohanWes/wow-rustcorder
```

That triggers `.github/workflows/pages.yml`, which takes the tag's signed
`repo/` and `latest.json` from its CI run, adds `data/site/index.html`, and
deploys them to GitHub Pages (source: GitHub Actions). Until then, no user
sees the release. To redeploy a tag, run
`gh workflow run pages.yml -f tag=v<version>`. CI artifacts expire after 90
days, so a draft must be published within that time. Never hand-edit the
Pages site. `gh` defaults to the upstream fork parent, so pass
`--repo JohanWes/wow-rustcorder` on every call.

Once the Pages run is green, the served summary must match its line in the
release's `.sha256` asset:

```sh
curl -s https://johanwes.github.io/wow-rustcorder/summary | sha256sum
```

`flatpak remote-info` answers from cached metadata; run
`flatpak update --appstream <remote>` before trusting it.

If a source fetch fails in CI (GitHub archives, or the gpu-screen-recorder
tarball on dec05eba.com), re-run once with `gh run rerun <id> --failed` before
investigating.

Verify with only the public key:

```sh
gpg --import public-release-key.asc
flatpak remote-add --user --if-not-exists warcraft-rustcorder \
  https://johanwes.github.io/wow-rustcorder/index.flatpakrepo
flatpak install --user warcraft-rustcorder io.github.JohanWes.WarcraftRustcorder
flatpak remote-info --user --show-commit warcraft-rustcorder \
  io.github.JohanWes.WarcraftRustcorder
```

If the signing key is lost, users must remove and re-add the remote with a new
public key. There is no rotation framework or staging remote.

## The rename from Warcraft Recorder

The app was `io.github.JohanWes.WarcraftRecorder`, served from
`https://johanwes.github.io/wow-recorder-linuxwayland/`. Every release repo
also carries the old ID marked end-of-life with a rebase to the new one, so a
plain `flatpak update` moves old installs over. Flatpak moves
`~/.var/app/<old id>` on the first start and the app moves its config folder
(`config::migrate_old_config`). Folders granted through the file chooser belong
to the old ID, so the app asks for them again.

One-time steps for the first release after the rename:

1. Rename the GitHub repository to `wow-rustcorder`. Pages then serves the
   existing `gh-pages` at the new URL, and the old URL stops working.
2. Tag and publish the release as usual, to the new URL.
3. Keep the old URL alive: the repository `JohanWes/johanwes.github.io`
   serves, in its `wow-recorder-linuxwayland/` folder, a signed, empty
   repository whose summary redirects old remotes to the new URL. It was
   built once, for 1.0.16, and needs no updates. Keep it, and the
   end-of-life rebase in the release workflow, as long as old installs may
   still update (at least until 2027-04); the redirect alone does not migrate
   them.
4. Check with an old install: `flatpak update --user` should report the
   end-of-life rebase and install `io.github.JohanWes.WarcraftRustcorder`.

## Install, update, rollback, uninstall

For a stable install, add the permanent remote and install the application as
shown above. Update with `flatpak update --user`. For candidate testing use:

```sh
flatpak install --user ./warcraft-rustcorder.flatpak
```

The remote holds only the latest build, so there is no server-side rollback:
fix forward with a new patch release. A user can go back by installing an
older release's `.flatpak` bundle from GitHub Releases.

Uninstalling the app does not delete recordings:

```sh
flatpak uninstall --user io.github.JohanWes.WarcraftRustcorder
```

Use `--delete-data` only when deleting the native app's private data is
intentional.

## Windows

The tag job also builds `WarcraftRustcorder-<version>-setup.exe` (Inno Setup,
per-user, no admin) and `latest.json` on a Windows runner with
`windows/build.sh`, and attaches both to the GitHub release. The publish job
uploads the installer to VirusTotal and links its report in the release notes;
the API key is `VT_API_KEY` in the `release` environment. The installer is
not code-signed yet, so SmartScreen warns on first run.

Installed apps check
`https://johanwes.github.io/wow-rustcorder/windows/latest.json` at
startup and from "Check for updates", then download the installer, verify its
SHA-256, and run it silently. `pages.yml` deploys that file together with the
Flatpak repo when the release is published.

To build locally, run `bash windows/build.sh` in an MSYS2 UCRT64 shell with
the packages listed at the top of the script and Inno Setup 6. Output lands in
`build-windows/`. Uninstalling leaves config and recordings in place.
