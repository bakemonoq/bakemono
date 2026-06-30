# Roadmap

What comes after MVP. Not commitments, just the working order.

## v0 (MVP)

See `MVP.md`. One reference board (with its own embedded relay), one desktop app publishing to a default 5-relay set, end-to-end loop working. Federation is already real from day 1 because events are spread across many independent Nostr relays.

## v1: Second board, multi-source scraping, kudos UI

- Operate a second board in a different jurisdiction. Different operator keypair, independent local moderation policy, same event kind.
- Expose additional source platforms in the desktop app's picker. The gallery-dl ecosystem already supports many; we just wire them in.
- Federated kudos: byte counter aggregates contributions across pubkey, fetched from any board that has indexed our events.
- Leaderboards per board and across boards.
- Replace first-seen-pubkey mod queue with automated reputation thresholds (auto-approve after N approved events; relay-side spam-rate scoring).
- Backup torrents of nightly postgres snapshots so anyone can bootstrap a fresh board without replaying every event from time zero.

## v2: Volunteer seedboxes, anti-spam hardening, transparency tooling

- Volunteer seedbox tier: users who run a node 24/7 with X TB of pinned content get badges, kudos multipliers, priority search.
- NIP-13 proof-of-work requirement on incoming events for boards that want it (tunable difficulty).
- NIP-57 Lightning zap gating for boards that want paid writes (configurable; we run the reference board without it).
- Public board directory: list of known healthy boards, uptime stats, jurisdiction, moderation posture, takedown log links.
- Hydrus PTR-compatible export: nightly dump of our event data in PTR tag-mapping format so Hydrus users can pull it directly.

## v3: Search quality, polish, localization

- Better search: full-text on post bodies via postgres tsvector, fuzzy creator name, faceted filters by platform / tier / date range.
- Tag taxonomy with curated topic tags; lightweight per-board tag review pipeline.
- Bigger warm cache, smarter eviction (frecency).
- WebTorrent improvements: video chapters, seek-ahead prefetch, better mobile playback.
- Localization (Japanese, Russian, Chinese, Spanish, Portuguese).

## v4 and beyond

- Mobile app (browse + push manifests; no daemon seeding due to OS constraints).
- IPFS sidecar for thumbnails and small images.
- Tor / i2p access for boards that want it.
- Hardware wallet integration for keypair storage (secp256k1 is what Bitcoin uses, so Ledger / Trezor / Coldcard work natively for BIP-340 Schnorr).
- Browser extension that lets users contribute events directly from any source page they are already viewing (no full app install needed for occasional contributors).

## Things we will not build

- A payment system that compensates source creators. This is an archive utility, not a payments platform; monetization introduces a category of legal and accounting complexity orthogonal to the archival mission.
- Live streaming, comments, votes, follow graph. Not what this is for.
- Account recovery via email or password. Pubkey identity is the model; users back up their own `nsec`.
- Cloud sync of user data. Local-first or not at all.
- Anything that turns the project into a publisher rather than an archive (paid premium content, exclusivity deals). Hard line
