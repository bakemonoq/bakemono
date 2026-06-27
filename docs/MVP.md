# MVP

The smallest demoable system. One reference board (with its own embedded relay), one desktop app, one user flow that works end-to-end.

## Acceptance criteria

A new user finds Bakemono via a Reddit post. They:

1. Visit the reference board, browse content, preview images and short video in-browser
2. Read the "How to Contribute" page, download the desktop app for their OS
3. Install the app, generate their Nostr keypair (saved locally as `nsec`), log into Patreon in the embedded webview
4. Select creators they sub to, click "Scrape and Share"
5. App scrapes, hashes, seeds, signs kind 31063 events and publishes them to the default relay set
6. Their contributions appear on the board within seconds (the board's indexer is subscribed to the same relays)
7. Their contribution byte counter (proto-kudos) ticks up

If all seven steps work without intervention, MVP ships.

## In scope

### bakemono-board v0

- `nostr-rs-relay` running as a sidecar at `wss://relay.bakemono.example`
- Indexer that subscribes to a configured relay set (ours + 4-5 public Nostr relays) with filter `{"kinds": [31063]}`
- Postgres schema for events (deduped by event id, indexed by file hash, pubkey, creator, platform, posted_at), mod queue, takedown ledger
- Search and browse UI: by creator, by recent, by hash, by simple text search on post title and content
- Post view page with embedded WebTorrent player for video, direct WebTorrent fetch for images
- Warm cache: a small disk cache of the top N most-viewed files, served via plain HTTPS when present
- Mod queue UI for board operators to approve / reject events from first-seen pubkeys
- Instance operator keypair management, kind 31064 takedown signing
- One STUN endpoint at `stun.bakemono.example` (small public-facing service)

### bakemono-app v0

- Tauri shell, single binary per OS (Windows, macOS, Linux x86_64 + arm64)
- Three internal pieces: daemon, GUI, tray icon
- Daemon: BT v2 seeder via librqbit, DHT, scrape queue, Nostr event signer, multi-relay publisher
- GUI: keypair management (`nsec` import/export), Patreon login webview, creator selection, scrape progress, contribution stats, relay list editor
- Tray: status, pause/resume seeding, open GUI, quit
- Scraper: gallery-dl wrapped for Patreon (Python sidecar), exposing one creator at a time
- Default relay set baked in: ours + relay.damus.io + nos.lol + relay.snort.social + nostr.wine
- Auto-update via Tauri's updater (signed release artifacts)
- Bandwidth limits configurable, default cap at 20 Mbit up
- Daemon autostart configurable, default off (user opts in)

### Common

- `bakemono-core` with kind 31063 / 31064 / 31065 event types, tag helpers, validation (see `PROTOCOL.md`)
- Signing and verification via the `nostr` rust crate (secp256k1 Schnorr)
- BitTorrent v2 with WebTorrent compatibility (hybrid trackers if needed)
- TURN fallback via a small coturn instance behind rate limit

## Out of scope for v0

- Multiple Bakemono boards run by us (Nostr handles base federation already; we run one board, but events are durable across the relay network from day 1)
- Kudos UI beyond a byte counter
- Leaderboards, badges, flair, social features
- IPFS for any layer
- Volunteer seedbox tier
- Mobile app (browse-only via web is fine)
- Comments, votes, ratings
- Tag taxonomy, PTR-style tag federation
- Multi-creator parallel scraping (one at a time)
- Other source platforms beyond Patreon (gallery-dl supports them; expose in v1)
- Hardware wallet key storage
- NIP-13 proof-of-work anti-spam, NIP-57 zap-gated writes
- Backup torrents of the postgres snapshot (post-MVP, low effort)

## Build order

Loose sequence. Each step ships something demoable.

1. **Workspace + `bakemono-core`.** Set up the Cargo workspace under `crates/`. Build `bakemono-core` first: kind 31063 manifest event type, kind 31064 takedown, kind 31065 mod publication, tag schema constants and helpers, event validation, wrapping the `nostr` crate. Unit tests covering: event build + sign + verify roundtrip; tag schema validation (missing required tag is rejected); replaceable event semantics (same `(pubkey, kind, d)` triple replaces older event in a mock store); forged-signature rejection. This is the foundation everything else imports.
2. **Tiny CLI smoke test.** Throwaway binary that uses `bakemono-core` to read a file from disk, build a kind 31063 event, sign it, and publish it to a single local relay over WebSocket. Throwaway tiny subscriber that connects to the same relay and prints received events. Proves the core works end-to-end against a real relay before any product code.
3. **Wrapper around gallery-dl.** Scrapes one Patreon creator into a folder. CLI only; Python sidecar invoked from rust. Output is just files on disk.
4. **Scraping pipeline producing signed events.** Wire steps 1-3 together: scraper outputs files, each file gets hashed, each gets a signed kind 31063 event built and published. CLI driver still.
5. **Add seeder using librqbit.** Each scraped file is also seeded via BT v2 with a magnet link that matches the `magnet` tag in the published event. Test on a single machine first, then a second machine downloading via WebTorrent.
6. **Build `bakemono-board` v0 skeleton.** Run `nostr-rs-relay` sidecar locally. Build the indexer that subscribes to the local relay with `{"kinds": [31063]}` and writes events into postgres. Build a stub axum + maud frontend with creator-list and post-view pages. Add the WebTorrent JS player on the post-view page.
7. **End-to-end loop demo.** Machine A runs the CLI: scrape, sign, publish to its local relay, seed. Machine B runs the board: indexer ingests events from a relay it subscribes to (could be A's), web UI shows them, WebTorrent player streams the file from A. This is the milestone where the architecture is proven real.
8. **Wrap CLI scraper in the Tauri GUI.** Add keypair management (generate, import via `nsec`, export, backup prompt), Patreon webview, creator picker, scrape progress UI. Daemon shape emerges.
9. **Add the daemon/tray split.** Background seeding and publishing continue after GUI is closed. Tray icon shows status. Autostart hooks for each OS.
10. **Multi-relay publish in the app.** App publishes every event to its full configured relay set in parallel. Default list baked in; user can edit. Add public relays (damus, nos.lol, etc) and confirm the board's indexer (also subscribed to these) sees the events too, not just our relay.
11. **Build the mod queue UI on the board.** First-seen-pubkey gating: events from a new pubkey are held in a queue, operator approves / rejects, future events from approved pubkeys flow through automatically.
12. **Warm cache on the board.** Top-N viewed files cached on disk for instant preview. Eviction by least-recent-use.
13. **Takedown signing and publishing.** Board operator UI for marking events hidden locally + publishing kind 31064 takedowns signed by the instance keypair. Tested by another instance (if we have one) honoring the takedown.
14. **Polish, release builds, signed installers, auto-update, public launch.**

## Definition of done per step

- Code reviewed and merged on main
- Integration test covering the happy path runs in CI
- Manual smoke test on the relevant OS(es) before merging
- README updated for any new user-facing behaviour

## Team sizing assumption

This MVP plan assumes 1-3 people, evenings and weekends. The sequencing is set up so each step is independently demoable, in case the team shrinks to one person mid-way
