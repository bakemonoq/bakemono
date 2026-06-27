# Bakemono

A federated, P2P-backed archive of paywalled creator content. Spiritual successor to Kemono, designed to survive single-server failure and per-jurisdiction takedowns.

## How it works

- Content (images, audio, video, files) lives in a BitTorrent v1 + WebRTC swarm via the `webtorrent` package. No central CDN.
- Metadata is published as signed Nostr events (custom kind 31063) to many independent relays. No central index.
- Users scrape their own paid subscriptions via a desktop app, contribute the bytes to the swarm, and fan signed events out to multiple relays at once.
- Browsers preview content directly via WebTorrent. No plugin, no torrent client needed for normal viewing.
- A "board" is a self-hostable web instance: it runs its own relay, an indexer over a configured relay set, postgres for search, and a maud SSR UI. Anyone can spin one up.

## Why it exists

Kemono had a single point of failure at its file servers (the n1-n4 subdomains, one IP block, one upstream provider). When that broke, the entire archive went dark for months. Bakemono separates content from index, publishes the index across many Nostr relays operated independently, and addresses content by hash so any peer can serve any file. Taking down any one operator does not take down the system. The archive can be resurrected by a stranger pointing a fresh indexer at the relay set.

## Status

Pre-MVP. See `docs/MVP.md` for the build plan and `docs/ARCHITECTURE.md` for the technical picture.

## Repo layout

Single Cargo workspace. Three crates, plus docs.

- `crates/bakemono-core/` - shared library: manifest types, signing, canonical JSON. Imported by app and board.
- `crates/bakemono-board/` - the web instance (anyone can self-host)
- `crates/bakemono-app/` - desktop client (Tauri, Windows/macOS/Linux)
- `docs/` - architecture, protocol, MVP scope, roadmap

## Licence

AGPL-3.0 (planned). Viral copyleft is intentional: any modification that touches the network must also be open
