# Bakemono

Back up the content you follow and keep it available for everyone, even after the original site takes it down. Files live in a BitTorrent swarm, the index lives on independent Nostr relays, and any board serves both to a plain web browser. No single server owns the archive, so no single failure can erase it.

- Browsing needs nothing: a board streams swarm content as ordinary HTTP, so `<img>` and `<video>` just work.
- Contributing is one desktop app: it fetches your subscribed content, seeds it, and publishes a signed manifest.
- Anyone can run a board with their own moderation policy. Losing one board loses nothing; a fresh one rebuilds the index from the relays.

## Try it

Browse the reference board at [bakemono.app](https://bakemono.app) with nothing installed.

## Archive your content

Grab the desktop app from the [latest release](https://github.com/bakemonoq/bakemono/releases/latest):

| OS                    | File                      |
|-----------------------|---------------------------|
| Windows               | `Bakemono_x64-setup.exe`  |
| macOS (Apple Silicon) | `Bakemono_aarch64.dmg`    |
| macOS (Intel)         | `Bakemono_x64.dmg`        |
| Linux                 | `Bakemono_amd64.AppImage` |

Builds are not yet code-signed. On macOS the first launch says the app "is damaged"; clear the quarantine flag once:

```
xattr -d com.apple.quarantine /Applications/Bakemono.app
```

On Windows click "More info" then "Run anyway" in the SmartScreen prompt. The app updates itself after that.

Then:

1. Open the app and let it generate your key (saved locally as an `nsec`; back it up).
2. Sign in to a source platform in the app's built-in window. Your login stays on your machine and is never sent to any server.
3. Pick the subscribed content you wish to back up and start.
4. Leave it running. Closing the window keeps the daemon seeding in the background, so everything you backed up stays available to everyone.

## Run your own board

The whole stack (web UI, relay, postgres, torrent gateway) is one compose file:

```
git clone https://github.com/bakemonoq/bakemono
cd bakemono
docker compose up -d --build
```

The UI comes up at `http://localhost:3000`. Board name, relay set, gateway port, cache size, and moderation are all environment variables documented in `docker-compose.yml`. To deploy a prebuilt image instead of building on the host, see `docker-compose.deploy.yml`.

## Build from source

Single Cargo workspace, Rust stable:

```
cargo build --workspace
cargo test --workspace

# the board (needs postgres at DATABASE_URL and a relay at BAKEMONO_RELAYS)
cargo run -p bakemono-board

# the desktop app (dev: gallery-dl and ffmpeg on PATH; releases bundle both)
cargo run -p bakemono-app
```

Protocol and design details live in `docs/`.

## Licence

[AGPL-3.0-or-later](LICENSE). Viral copyleft is intentional: any modification that touches the network must also be open
