# Bakemono

Open-source curated peer-to-peer content archive. Content and index both live in IPFS, replicated across an operator fleet and volunteer keepers via IPFS Cluster; each board scrapes sources server-side and publishes its whole archive as one signed manifest.

The name Bakemono (化け物) means "shapeshifter": one piece of content, many forms across many instances.

## Core idea in one paragraph

Centralized archives die with their one server, and volunteer-seeding designs die of leecher economics. Bakemono stores every file in IPFS addressed by CID and describes the whole archive in a manifest: a signed DAG of plain JSON (head -> root -> per-creator shards) living in the same IPFS store. The board is the single publisher: it scrapes sources itself with operator-held cookies (no user uploads, no user keys, so provenance is first-party and there is no per-file verification queue), indexes into postgres (derived state, rebuildable from the manifest), and serves browsing over plain HTTP by proxying its local Kubo (NoFetch + denylist). Replication is IPFS Cluster: the operator's own hosts are trusted peers, volunteers follow the pinset with stock `ipfs-cluster-follow` and auto-replicate everything, takedowns included - a revoked CID leaves the pinset, followers unpin, GC frees it, and the hash-linked head chain keeps every removal auditable. Losing every server loses nothing: any keeper holds the latest signed head and `bakemono restore` rebuilds a board from it. Boards federate by importing each other's manifests, key + pointer, no shared state.

## Migration status

The IPFS stack is implemented and the Nostr/BitTorrent code is deleted (crates bakemono-app / bakemono-daemon / bakemono-engine / bakemono-torrent / bakemono-cli, the relay sidecar, kinds 31063/31064, per-file torrents, the RSS seed feed). The board schema drops the old tables on connect; there was no official launch, so no data migration exists. Remaining from `docs/MVP.md`: prod fleet deployment (kubo NoFetch + denylist wiring, DNSLink, keeper hosts).

## Components

- `bakemono-core` - shared library crate. Manifest types (head / root / shard), canonical JSON, ed25519 signing and verification, frozen `ipfs add` parameter constants. Pure logic, zero I/O. Unit-tested in isolation.
- `bakemono-scraper` - thin gallery-dl wrapper library (invocation, cookies, streaming output, download archive).
- `bakemono-board` - the `bakemono` binary. Subcommands: `serve` (web UI + `/contribute` intake + scrape worker + manifest publisher + nopfs denylist writer - the only long-running process; media is served at `/ipfs/{cid}` by the local Kubo gateway, not proxied through the board), `scrape` (one-off creator scrape with a cookies.txt), `ingest` (import a directory of already-scraped files + sidecars), `keygen` (generate the cookie RSA keypair), `autoimport` (run one keyed import round, private key from stdin), `restore` (rebuild postgres and pinset from a head CID). Rust: axum + sqlx + maud + Postgres.

## Contributor cookies

Content comes from subscribers via the `/contribute` form (platform + session cookie), not an operator account. Enumeration is delegated to gallery-dl's per-platform "your subscriptions" feed extractor (Patreon `/home`, Fanbox `/home/supporting`, Boosty `/`) - no hand-written discovery. Submit: a one-item gallery-dl feed probe validates the cookie (stdout non-empty = live); then seal it (per-cookie AES-256-GCM key, RSA-4096-OAEP-wrapped) if opted into daily import, only ciphertext stored, RSA private key stays offline. `scrape_feed` runs gallery-dl on the feed URL, ingests, derives creators from sidecars, deletes files after ingest (staging stays bounded; a sweep of the whole staging dir recovers files an interrupted run left). `bakemono autoimport` (private key piped to stdin over SSH, never on disk) decrypts live cookies in memory each round and re-scrapes the feed. `crypto.rs` = seal/open/keygen; `platform.rs` = declarative table (id, label, cookie_name, domain, feed_url, live). Only platforms with a gallery-dl subscription-feed extractor fit (Patreon/Fanbox/Boosty); SubscribeStar/Fantia/Gumroad/Afdian have none, Fansly is token-not-cookie - all out of scope. Tables: `cookies` (sealed), `cookie_creators` (derived from scrapes, display only). `BAKEMONO_COOKIE_PUBKEY` (public PEM, inline or path) enables `/contribute`; `BAKEMONO_COOKIE_PRIVKEY` makes `serve` run rounds itself (less secure convenience).

Alongside on the board host: postgres, Kubo, `ipfs-cluster-service`. Operator keeper hosts run Kubo + `ipfs-cluster-service` (trusted peers); volunteer keepers run Kubo + `ipfs-cluster-follow` and zero Bakemono software.

## Tech stack (decided)

| Concern | Choice |
|---|---|
| Content + index storage | IPFS (Kubo) |
| Replication | IPFS Cluster: operator hosts as trusted peers, volunteers via `ipfs-cluster-follow` |
| Index wire format | signed manifest: plain JSON DAG in IPFS (head -> root -> shards), spec in `docs/PROTOCOL.md` |
| Signing | ed25519 over canonical JSON (bytewise-sorted keys, no insignificant whitespace); one board key, no user keys |
| Current-head pointer | `head.json` over HTTPS + DNSLink + head pinned in the cluster pinset |
| File addressing | CIDv1 base32, raw leaves, sha2-256, fixed 1 MiB chunker (frozen constants); raw-byte sha256 kept alongside |
| Board backend | rust: axum + sqlx + maud + Postgres |
| Source retrieval | gallery-dl, yt-dlp invoked server-side by the scrape worker; ffmpeg for thumbnails |
| Async runtime | tokio |
| HTTP client | reqwest |
| Browser delivery | media served at `/ipfs/{cid}` by the local Kubo gateway (`Gateway.NoFetch`, nopfs denylist) behind a reverse proxy, plain HTTP with `Range`; the board is out of the byte path |
| Fleet connectivity | cluster bootstrap + `Peering.Peers`; public DHT best-effort with `Reprovider.Strategy=roots` |

## What is in MVP v0

- One reference board scraping server-side, publishing a signed manifest
- Kubo + cluster on the board host and 1-2 operator keeper hosts; published follower config for volunteers
- Takedown flow end to end: revoked list -> pinset removal -> follower unpin -> gateway denylist
- `bakemono restore` proven: kill the board host, rebuild it from a keeper
- Migration of the existing catalog off BitTorrent before the old stack is deleted

## What is deferred

- Partial adoption by keepers (per-creator / per-platform pinsets)
- Peer board import pipeline and second board (manifest format already supports it)
- Keeper stats, kudos, leaderboards
- IPNI delegated routing
- Mobile (browse-only via web), comments, voting, social features
- Community uploads: permanent non-goal, not deferred - the single-writer model is the trust model

## Threat model and ethics

- Moderation is per-board. Admission is source-level (which platforms, which creators); there is no upload endpoint. Removal is the revoked list in the signed manifest: the fleet and followers unpin automatically, gateways denylist, and the hash-linked head chain is a built-in transparency log of every takedown.
- CSAM and other categorically illegal content is actively moderated at every board. Peer boards apply `csam`-reason revocations unconditionally; boards that refuse get dropped from peers lists.
- Nodes outside the cluster that pinned a CID independently are beyond anyone's control. Inherent property of open networks; the manifest records intent and scopes enforcement to the trust boundary.
- Contributor cookies are sealed with a hybrid RSA-4096 + AES-256-GCM scheme; the private key never lives on the server, so a database dump exposes no usable token. Plaintext exists only transiently (submission validation, and in-memory during a keyed round) and is never published, shared, or written into the archive. The `/contribute` form states this plainly and offers a one-time-import option that stores nothing.
- The board key's offline backup is the recovery lynchpin: losing it stops future publishes but does not endanger published content or history.

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
- `docs/ARCHITECTURE.md` - layers, file lifecycle, federation, recovery, why the BT+Nostr design was retired
- `docs/MVP.md` - acceptance criteria and migration build order
- `docs/PROTOCOL.md` - manifest objects, signing, pointer, pinset, revocation semantics
- `docs/GLOSSARY.md` - terminology cheat sheet
- `docs/ROADMAP.md` - what comes after MVP
- `docs/KEEPERS.md` - how volunteers replicate the archive
- `docs/RELEASING.md` - release artifacts and tagging
