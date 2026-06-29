# Releasing

How desktop installers are built and shipped.

## What a release produces

Pushing a tag `v*` runs `.github/workflows/release.yml`, which builds the Tauri app on four runners and drafts a GitHub release with:

- Per-OS installers: `.dmg` (macOS arm64 + x64), `.AppImage` (Linux x64), NSIS `-setup.exe` (Windows x64)
- `latest.json` plus per-platform `.sig` files for the auto-updater
- Stable, versionless copies of each installer (`Bakemono_aarch64.dmg`, `Bakemono_x64.dmg`, `Bakemono_amd64.AppImage`, `Bakemono_x64-setup.exe`) so `releases/latest/download/<name>` keeps resolving across versions

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
- **macOS is per-arch, not universal.** A universal build is impractical here: the bundled Node runtime and webtorrent's native addon are arch-specific. The board's `/contribute` download list still names one `Bakemono_universal.dmg`; reconcile it to the two arch-specific dmgs when the board changes next land.

## First-run risks to watch

The workflow is authored but has not run against live runners. Most likely to need a tweak on the first tag:

- gallery-dl under PyInstaller - extractor submodules sometimes need extra `--hidden-import`/`--collect-*` flags
- the `bundle_glob` paths, if a Tauri version changes bundle output layout
- the stable-name upload step (guarded with `continue-on-error`, so it cannot fail the release)
