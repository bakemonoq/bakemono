# Bakemono

Open-source peer-to-peer content archive protocol. Federated metadata layer over signed Nostr events, content layer over BitTorrent v1 + WebRTC for browser-native preview, with operator-level moderation autonomy per instance.

The name Bakemono (化け物) means "shapeshifter": one piece of content, many forms across many instances.

## Core idea in one paragraph

Centralized content archives concentrate file storage and index in one administrative boundary, making them brittle to single-host failure. Bakemono separates the system into three loosely coupled layers: (1) content lives in a BitTorrent v1 + WebRTC swarm, addressed by sha256 so the file's identity is what it is, not where it lives; (2) metadata is published as signed Nostr events (custom kind 31063) to many independent relays, so losing any one relay or instance does not lose the index; (3) each board runs its own embedded relay plus a postgres indexer and web UI, sets its own local moderation policy, and inherits Nostr's relay-based federation for free. A cross-platform desktop client lets users back up their own subscribed content, contribute the bytes to the swarm, and publish signed events to multiple relays at once.

## Components

The repo is a single Cargo workspace. Three rust crates plus docs.

- `bakemono-core` - shared library crate. Bakemono Nostr event types (kind 31063 manifest, kind 31064 takedown), tag schema helpers, event-building and validation routines, protocol version constants. Wraps the `nostr` crate from rust-nostr.org. Pure logic, zero I/O. Imported by both `bakemono-app` and `bakemono-board` so the wire protocol cannot drift between client and server. Unit-tested in isolation.
- `bakemono-board` - the self-hostable web instance. Runs a `nostr-rs-relay` sidecar, runs an indexer that subscribes to a configured relay set and ingests kind 31063 events into postgres, serves search/browse UI, runs a warm cache for popular previews. Rust: axum + sqlx + maud (SSR templates) + Postgres. Depends on `bakemono-core`.
- `bakemono-app` - cross-platform desktop client. Tauri (rust + web frontend). Three thin pieces around one shared backend: a daemon (DHT, BT seeder, retrieval queue, Nostr event signing and multi-relay publish), a GUI (configure archive jobs, manage keypair and relay list, see contribution stats), a tray icon (quick status, pause/resume). Depends on `bakemono-core`. Wraps gallery-dl for image/post sources and yt-dlp for video, both invoked from the rust daemon as Tauri sidecar binaries (Python runtime bundled). Uses an embedded webview so the user signs in to source platforms themselves; sessions stay local, never sent to any server.

## Tech stack (decided)

| Concern | Choice |
|---|---|
| Desktop app shell | Tauri + rust |
| Source retrieval | gallery-dl, yt-dlp (Python sidecars invoked from rust) |
| Seeding (desktop + board warm cache) | `webtorrent` >=2.3.0 (Node sidecar; BT v1 over TCP/uTP + native WebRTC) |
| Browser preview | `webtorrent` >=2.3.0 (same package, runs in the browser via WebRTC) |
| Board backend | rust: axum + sqlx + maud + Postgres |
| Async runtime | tokio |
| HTTP client | reqwest |
| Metadata wire format | Nostr events (kind 31063 manifest, kind 31064 takedown) |
| Federation transport | Nostr relays. Multi-relay publish + subscribe over WebSocket per NIP-01 |
| Relay implementation | `nostr-rs-relay` (rust, MIT, by Greg Heartsfield) run as sidecar to the board |
| Signing | `nostr` crate from rust-nostr.org. secp256k1 Schnorr per BIP-340, NIP-01 canonicalization |
| Identity | secp256k1 keypair per user (Nostr-standard). User-facing `nsec` / `npub` bech32 per NIP-19 |
| NAT traversal | ICE (STUN public + ours, hole punching, TURN as fallback) |

## What is in MVP v0

- One reference board running at a single domain, with its own embedded relay
- Desktop app for Windows / macOS / Linux that publishes to 5+ relays by default
- Single-source-at-a-time retrieval via one gallery-dl extractor in v0 (additional extractors exposed in v1)
- Kind 31063 manifest events, in-browser WebTorrent preview for images and short video
- Warm cache on the board for top-popular files
- Manual mod queue on the board's indexer for first-time-seen pubkeys

## What is deferred

- Federation between multiple Bakemono boards (Nostr handles base federation already; multi-board comes when we run a second board ourselves)
- Kudos beyond a byte counter (leaderboards, badges, flair come v1)
- IPFS for thumbnails (BitTorrent v2 + WebTorrent does the job for v0)
- Volunteer seedbox tier and incentive design
- Mobile app (mobile is browse-only via web in MVP)
- Comments, voting, social features
- Tag federation in the Hydrus PTR style
- Hardware wallet support for keypair storage

## NAT and connectivity

Most home users are NAT'd. WebRTC's ICE handles it: STUN servers (public free ones plus our own small one) for public IP discovery, hole punching for restricted NATs, TURN as fallback for the small percentage of symmetric / carrier-grade NATs. WebTorrent and modern BitTorrent libraries include this; we wire it up, we don't implement it.

## Threat model and ethics

- Moderation is per-board. Each operator sets local policy in line with their own legal obligations. Takedowns are published as kind 31064 Nostr events signed by the operator's instance keypair, providing a built-in transparency log.
- Takedowns propagate via the Nostr relay network. Peer boards subscribe to takedown events from operators they trust and apply per local policy.
- CSAM and other categorically illegal content is actively moderated at every board. No operator runs without moderation. Boards that refuse to honor `csam`-reason takedowns get dropped from peer trust lists.
- Events on independently-operated relays are subject to those relays' own retention and moderation policies, not any single operator's. This is an inherent property of decentralized federation, shared across the Nostr ecosystem broadly.
- User session credentials stay on the user's machine, never sent to any server, never exfiltrated by the client. This is non-negotiable for the project's social licence.

## Style rules (apply to all files in this repo)

- No em-dashes or en-dashes anywhere. Plain hyphen `-` only.
- No guillemets `«»`. Use straight double quotes `"`.
- No arrow symbols. Write `->` instead of unicode arrows.
- No trailing period on the final sentence of any output, including code comments.
- No "Generated with Claude" / "Co-authored by Claude" lines in commits or anywhere else.
- Code comments: default to none. When one is needed, one short line on a non-obvious why.
- No module/file/crate doc comments that list the module's current contents or restate the task/scope (e.g. "Bakemono manifest events, tag helpers, signing"). They go stale the moment something is added. Prefer none, or one durable line stating a non-obvious invariant or why.
- Function ordering follows the stepdown rule: callers above callees, code reads top to bottom like a book.
- Avoid hedging, "it's worth noting", bullet bloat, restated obvious points.

## Commit messages

Minimal, like a human jotting a quick note mid-work:

- 3-4 words, lowercase first letter, no period.
- Subject line only - no body, no description.
- No co-author or attribution trailer of any kind.
- Examples: `add bakemono-core crate`, `wire manifest verify`, `fix canonical sort`

## Pointers

- `README.md` - public-facing overview
- `docs/ARCHITECTURE.md` - layers, file lifecycle, federation model
- `docs/MVP.md` - concrete build order and scope
- `docs/PROTOCOL.md` - Nostr event kinds, tag schema, signing, relay protocol
- `docs/GLOSSARY.md` - terminology cheat sheet
- `docs/ROADMAP.md` - what comes after MVP
