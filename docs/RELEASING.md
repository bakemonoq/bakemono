# Releasing

How releases are built and shipped.

## What a release produces

Pushing a tag `v*` produces, attached to one draft release:

- `bakemono-server-<target>.tar.gz` per server platform (Linux x64 + macOS arm64): the `bakemono` binary plus bundled gallery-dl / yt-dlp / ffmpeg, so a host untars and runs the board from the console without Docker
- The board docker image pushed to GHCR (prod deploys via watchtower pull)

The release is a draft. Review the artifacts, then publish it as latest.

## Cutting a release

```
git tag v0.5.0
git push origin main v0.5.0
```

Bump `version` in the root `Cargo.toml` (and the lockfile) before tagging.

## Sidecar bundling

The server bundle stages standalone gallery-dl (PyInstaller) and static ffmpeg per target. At runtime the scrape worker finds them through env seams (`BAKEMONO_GALLERY_DL`, `BAKEMONO_FFMPEG`); dev builds leave those unset and fall back to PATH.

## Retired with the desktop app

The GUI installer pipeline (`release.yml`: dmg / deb / rpm / NSIS, Tauri updater keys, `latest.json`) goes away with the IPFS migration - there is no desktop app to ship. Until step 9 of `docs/MVP.md` lands, the old workflows still run on tags; ignore their artifacts.

## Known rough edges

- gallery-dl under PyInstaller - extractor submodules sometimes need extra `--hidden-import`/`--collect-*` flags
- GitHub's release API occasionally times out mid-upload; re-running the failed job clears it
