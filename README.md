# Bakemono

A federated, peer-to-peer content archive protocol with reference implementations. Designed for resilience to
single-host failure with operator-level moderation autonomy.

## How it works

- Content (images, audio, video, files) lives in a BitTorrent v1 + WebRTC swarm via the `webtorrent` package. No central
  CDN.
- Metadata is published as signed Nostr events (custom kind 31063) to many independent relays. No central index.
- Users archive their own subscribed content via a desktop app, contribute the bytes to the swarm, and fan signed
  events out to multiple relays at once.
- Browsers preview content directly via WebTorrent. No plugin, no torrent client needed for normal viewing.
- A "board" is a self-hostable web instance: it runs its own relay, an indexer over a configured relay set, postgres for
  search, and a maud SSR UI. Anyone can spin one up.

## Why it exists

Centralized content archives concentrate file storage and metadata in one administrative boundary, making them brittle
to single-host failure. Bakemono separates content from index, publishes the index across many Nostr relays operated
independently, and addresses content by hash so any peer can serve any file. The loss of any single operator does not
take down the system. The archive can be reconstituted by anyone pointing a fresh indexer at the relay set.

## Try it

The reference board runs at [bakemono.app](https://bakemono.app). Browse and preview in the browser with nothing
installed. To add to the archive, run the desktop app.

## Install the desktop app

Grab the build for your OS from the [latest release](https://github.com/bakemonoq/bakemono/releases/latest):

| OS                    | File                      |
|-----------------------|---------------------------|
| Windows               | `Bakemono_x64-setup.exe`  |
| macOS (Apple Silicon) | `Bakemono_aarch64.dmg`    |
| macOS (Intel)         | `Bakemono_x64.dmg`        |
| Linux                 | `Bakemono_amd64.AppImage` |

The builds are not yet code-signed, so the first launch shows a Gatekeeper (macOS) or SmartScreen (Windows) warning. On
macOS, right-click the app and choose Open. On Windows, click "More info" then "Run anyway". The app updates itself
after that.

## Contribute bytes

1. Open the app and let it generate your key (saved locally as an `nsec`; back it up).
2. Sign in to a source platform in the app's built-in window. The session stays on your machine.
3. Pick the subscribed content you wish to back up and start. The app retrieves the files, hashes and seeds them over
   WebTorrent, and publishes a signed manifest event to the default relay set.
4. Leave it running. Closing the window keeps the daemon seeding in the background, so the files you shared stay
   previewable for everyone on the board.

Your logins never leave your computer and are never sent to any server. This is non-negotiable.

## Self-host a board

A board is the whole stack in one `docker compose`: postgres, an `nostr-rs-relay` sidecar, a WebTorrent tracker, and the
board web UI.

```
git clone https://github.com/bakemonoq/bakemono
cd bakemono
docker compose up -d --build
```

The UI comes up at `http://localhost:3000`. Configure it with environment variables (see `docker-compose.yml`):

| Variable                                     | Purpose                                                                                     |
|----------------------------------------------|---------------------------------------------------------------------------------------------|
| `BAKEMONO_RELAYS`                            | comma-separated relay URLs the indexer subscribes to (default: the bundled relay container) |
| `BAKEMONO_ICE_SERVERS`                       | JSON array of STUN/TURN servers handed to the browser player                                |
| `BAKEMONO_TURN_URLS` / `BAKEMONO_TURN_SECRET`| optional TURN: comma-separated relay URLs plus the coturn shared secret, appended to the ice servers with short-lived per-load creds; both unset means no TURN |
| `BAKEMONO_TURN_TTL`                          | lifetime of a minted TURN credential in seconds (default 3600)                              |
| `BAKEMONO_BOARD_NAME`                        | title shown in the header and browser tab                                                   |
| `BAKEMONO_MOD_TOKEN`                         | password for the `/mod` queue (HTTP Basic); unset disables `/mod`                           |
| `BAKEMONO_INSTANCE_NSEC`                     | instance key that signs kind 31064 takedowns; unset keeps hides local-only                  |
| `BAKEMONO_TRUSTED_INSTANCES`                 | peer instance npubs/hex whose takedowns this board honors                                   |
| `BAKEMONO_DMCA_CONTACT` / `BAKEMONO_CONTACT` | addresses shown on the `/info` page                                                         |

To deploy a prebuilt image instead of building on the host, see `docker-compose.deploy.yml`.

## Build from source

Single Cargo workspace. Rust stable plus, for the desktop app at runtime, Node and `gallery-dl`.

```
cargo build --workspace
cargo test --workspace

# run the board locally (needs a postgres at DATABASE_URL and a relay at BAKEMONO_RELAYS)
cargo run -p bakemono-board

# run the desktop app (dev: node + gallery-dl must be on PATH)
cargo run -p bakemono-app
```

Released installers bundle Node, the WebTorrent sidecar, and `gallery-dl`, so end users install nothing else. For
development, `pipx install gallery-dl` and a system Node are enough.

## Repo layout

- `crates/bakemono-core/` - shared library: manifest + takedown event types, tag schema, signing, validation
- `crates/bakemono-board/` - the web instance (anyone can self-host)
- `crates/bakemono-app/` - desktop client (Tauri, Windows/macOS/Linux)
- `crates/bakemono-daemon/`, `bakemono-engine/`, `bakemono-scraper/`, `bakemono-seeder/` - the app's background pieces
- `sidecars/webtorrent/` - the Node seeder driven over stdio by the daemon
- `docs/` - architecture, protocol, MVP scope, roadmap

## Docs

- `docs/ARCHITECTURE.md` - layers, file lifecycle, federation model
- `docs/PROTOCOL.md` - Nostr event kinds, tag schema, signing, relay protocol
- `docs/MVP.md` - build order and scope
- `docs/GLOSSARY.md` - terminology
- `docs/ROADMAP.md` - what comes after MVP
- `docs/RELEASING.md` - how desktop installers are built and shipped

## Licence

[AGPL-3.0-or-later](LICENSE). Viral copyleft is intentional: any modification that touches the network must also be open
