# Releasing

How desktop installers are built and shipped.

## What a release produces

Pushing a tag `v*` runs `.github/workflows/release.yml`, which builds the Tauri app on three runners and drafts a GitHub release with:

- Per-OS installers: `.dmg` (macOS arm64), `.deb` + `.rpm` (Linux x64), NSIS `-setup.exe` + `.msi` (Windows x64)
- `latest.json` plus per-platform `.sig` files for the auto-updater on macOS and Windows; the Linux `.deb` updates through the system package manager, not the in-app updater
- Stable, versionless copies of each installer (`Bakemono_aarch64.dmg`, `Bakemono_amd64.deb`, `Bakemono_x64-setup.exe`) so `releases/latest/download/<name>` keeps resolving across versions
- A headless server bundle per desktop platform (`bakemono-server-<target>.tar.gz`, Linux x64 + macOS arm64): the daemon + cli plus the same bundled node / gallery-dl / ffmpeg / webtorrent, so a server can untar and run scrape + seed from the console without Docker

The release is a draft. Review the artifacts, then publish it.

## One-time setup

Add these repo secrets (Settings -> Secrets and variables -> Actions):

- `TAURI_SIGNING_PRIVATE_KEY` - contents of the updater private key (generated at `~/.bakemono/updater.key`)
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` - its password (empty for the current key)

The matching public key is committed in `crates/bakemono-app/tauri.conf.json` under `plugins.updater.pubkey`. The updater key is independent of OS code-signing: keep the private key safe; if it is lost, shipped apps can no longer verify updates and every user must reinstall by hand.

## Cutting a release

```
git tag v0.1.0
git push origin v0.1.0
```

Bump `version` in the root `Cargo.toml` and `crates/bakemono-app/tauri.conf.json` together, and keep the tag in sync, before tagging.

## How sidecars are bundled

The installed app needs Node + the webtorrent script, plus `gallery-dl`, none of which a normal user has. The release workflow stages them per target into `crates/bakemono-app/`:

- `binaries/bakemono-daemon-<triple>` - our daemon, built for the target
- `binaries/node-<triple>` - the runner's Node runtime
- `binaries/gallery-dl-<triple>` - a standalone gallery-dl built with PyInstaller
- `sidecars/webtorrent/` - `seed.mjs` and its `node_modules`

Tauri bundles these via `externalBin` and `resources`. At runtime the GUI points the daemon at them through the env seams the engine already reads (`BAKEMONO_NODE`, `BAKEMONO_WEBTORRENT`, `BAKEMONO_GALLERY_DL`); dev builds leave those unset and fall back to PATH and the in-repo sidecar. These staged dirs are gitignored.

## Not done yet

- **OS code-signing.** Installers are unsigned, so first launch warns (macOS Gatekeeper, Windows SmartScreen). Wire an Apple Developer ID + notarization and a Windows code-signing cert into the workflow once available.
- **macOS is Apple Silicon only.** Intel (`x86_64`) was dropped: GitHub's `macos-13` Intel runners are scarce and a universal build is impractical (the bundled Node runtime and webtorrent native addon are arch-specific). Intel Mac users browse via the web board.
- **Linux ships `.deb` + `.rpm`, no AppImage.** linuxdeploy fails with an opaque error (tauri-apps/tauri#14796) that survives `libfuse2`, `APPIMAGE_EXTRACT_AND_RUN`, and `NO_STRIP`, so there is no in-app auto-update on Linux. Revisit when tauri's new AppImage bundler lands.

## Known rough edges

- gallery-dl under PyInstaller - extractor submodules sometimes need extra `--hidden-import`/`--collect-*` flags
- the `bundle_glob` paths, if a Tauri version changes bundle output layout
- the stable-name upload step (guarded with `continue-on-error`, so it cannot fail the release)
- GitHub's release API occasionally times out mid-upload; re-running the failed job clears it
