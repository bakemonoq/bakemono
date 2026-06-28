# Default servers

The bundled server lists, one entry per line. Blank lines and lines starting with `#` are ignored.

- `relays.txt` - Nostr relays the app publishes manifests to and the board subscribes to
- `trackers.txt` - BitTorrent / WebTorrent trackers for seeding and browser preview
- `stun.txt` - STUN servers for WebRTC NAT traversal

These files are the single source of truth. `bakemono-core` embeds them at build time (`include_str!`) and exposes `default_relays()` / `default_trackers()` / `default_stun()`, which the desktop app and the board both use. The packaged app does not ship these files: the lists are compiled into the Rust binary and handed to the webtorrent sidecar via env at launch. The sidecar only reads `trackers.txt` from disk as a fallback when run standalone from a repo checkout. No server URLs live in the Rust or JS source.

## Adding a server

Open a PR adding one URL on its own line to the matching file. No code changes needed. Trackers: `wss://` reach browser peers, `udp://` reach native clients
