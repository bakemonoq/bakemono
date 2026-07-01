# Glossary

Quick reference for terms used throughout the Bakemono docs and code.

## Nostr

- **Nostr**: open peer-to-peer protocol for signed, replayable JSON events. We use it as our metadata wire format and federation transport.
- **Relay**: a Nostr server that accepts events from clients, stores them, and streams them to subscribers. Stateless beyond storage. No coordination with other relays.
- **Event**: the unit of Nostr data. Signed JSON with `id`, `kind`, `pubkey`, `created_at`, `tags`, `content`, `sig`. Our manifests are kind 31063 events.
- **Kind**: Nostr's event type number. Bakemono manifests are kind 31063. Takedowns 31064. The range 30000-39999 is parameterized-replaceable per NIP-33.
- **NIP**: Nostr Implementation Possibility. The spec system Nostr uses. NIP-01 is the base protocol; NIP-33 is replaceable events; NIP-19 is bech32 encoding; etc.
- **NIP-33 (parameterized replaceable events)**: lets a contributor update their own event by publishing a new one with the same `d` tag. Relays keep only the latest version per `(pubkey, kind, d)` triple.
- **secp256k1 / Schnorr / BIP-340**: the cryptography Nostr uses for keys and signatures. Same curve as Bitcoin.
- **npub / nsec / note**: bech32-encoded forms of Nostr public key / private key / event id per NIP-19. User-facing key format.
- **nostr-rs-relay**: rust implementation of a Nostr relay by Greg Heartsfield. MIT-licensed. What we run as the board's relay sidecar.
- **nostr crate / nostr-sdk**: rust-nostr.org. Rust libraries for Nostr clients, relays, and signing. Used by both `bakemono-app` and `bakemono-board`.

## Content layer

- **Content addressing**: identifying a file by the hash of its bytes rather than by location. Two files with identical bytes have identical addresses; modifying any byte changes the address.
- **Swarm**: the set of peers participating in distributing one file (or one torrent). Membership changes constantly as peers come and go.
- **Seed / seeder**: a peer with the complete file, sharing it. The opposite of a leech.
- **Leech / leecher**: a peer downloading and partially sharing. Not pejorative in this project; everyone leeches before they seed.
- **DHT (Distributed Hash Table)**: the distributed phonebook BitTorrent uses to look up "who has hash X". Each peer holds a slice; queries hop through the network in roughly 15-20 steps.
- **BitTorrent v1**: the classic BitTorrent protocol (SHA-1 infohash, urn:btih magnet links). What librqbit and standard torrent clients speak, over TCP/uTP + DHT + trackers.
- **Infohash**: the SHA-1 of a torrent's info dict, the id a swarm forms around. The board addresses content by it: `/t/{infohash}/f/{fileIndex}`.
- **HTTP gateway**: the board component that joins a swarm for a cataloged infohash, pulls the bytes, and serves them to browsers as plain HTTP (with `Range`). Browsers do no P2P; an `<img>`/`<video>` points straight at a gateway URL.
- **Magnet link**: a URI containing a torrent's infohash and trackers. Lets a peer join a swarm without downloading a .torrent file first.

## Connectivity

- **Listen port**: a seeder must accept inbound BT connections, so it opens a fixed TCP port (default 4250) that peers dial. A seeder with no reachable port can only connect outward.
- **Peer pinning**: the gateway can be handed explicit `ip:port` seeders (`BAKEMONO_GATEWAY_PEERS`) to dial directly, bypassing tracker/DHT discovery. The reliable path on a LAN or to a known seedbox.
- **Reachability**: classic BT has no browser-style rendezvous, so cold-fill needs at least one side with an open port - a home seeder dials the board's open gateway port, or the board dials a pinned seeder.

## Bakemono-specific

- **Manifest**: in Bakemono, a kind 31063 Nostr event describing one file: tags carrying hash, magnet, source platform, creator, post id, etc. The unit of replication across relays.
- **Board**: a self-hostable Bakemono web instance. Runs an embedded `nostr-rs-relay`, an indexer that subscribes to many relays, a postgres index, and a web UI.
- **Indexer**: the board component that subscribes to Nostr relays, ingests kind 31063 events, dedupes, and writes them to postgres for fast search and browse.
- **Warm cache**: small disk cache on the board holding the most-viewed files for instant preview. Compromise toward centralization on hot content only.
- **Mod queue**: queue of events from first-seen pubkeys awaiting human review on a given board. Anti-spam wall.
- **Kudos**: archival contribution credit accruing to a pubkey. Bytes contributed, breadth of indexed sources. Display-only stat; not access control.
- **Operator pubkey**: each board operator holds an instance-level Nostr keypair, used to sign kind 31064 takedown events.

## Tooling

- **gallery-dl**: existing Python content extractor supporting many source platforms. We wrap it as our retrieval engine.
- **yt-dlp**: existing Python video extractor. Handles embedded video on supported source platforms.
- **Tauri**: rust framework for cross-platform desktop apps with a web frontend. Smaller and faster than Electron. Our desktop app framework.
- **bakemono-core**: shared rust crate holding Bakemono event types, tag helpers, validation, and protocol constants. Wraps the `nostr` crate. Imported by both `bakemono-app` and `bakemono-board` to guarantee the wire format cannot drift. Pure logic, no I/O.
- **librqbit / rqbit**: rust BitTorrent client and library by ikatson. Our torrent engine: `bakemono-torrent` wraps it for both seeding (create torrent, announce, serve peers) and the board gateway (join swarm, stream a file over byte ranges).
- **axum**: rust async web framework on tokio. Board's HTTP layer.
- **sqlx**: rust async Postgres driver with compile-time-checked queries. Board's DB layer.
- **maud**: rust compile-time HTML template macro. Board's SSR rendering
