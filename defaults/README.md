# Default servers

The bundled server lists, one entry per line. Blank lines and lines starting with `#` are ignored.

- `relays.txt` - Nostr relays the app publishes manifests to and the board subscribes to
- `trackers.txt` - BitTorrent trackers announced to when seeding and used by the board gateway to find peers

These files are the single source of truth. `bakemono-core` embeds them at build time (`include_str!`) and exposes `default_relays()` / `default_trackers()`, which the desktop app and the board both use. The packaged app does not ship these files: the lists are compiled into the Rust binary. No server URLs live in the Rust source.

## Adding a server

Open a PR adding one URL on its own line to the matching file. No code changes needed. Trackers are classic `udp://` / `http://` announce URLs
