# Architecture

Bakemono is built around one core idea: separate WHAT a file is from WHERE it lives, replicate the WHAT across many independent operators, and let the bytes flow peer to peer.

## The three layers

### 1. Content layer

The actual file bytes. Lives in a classic BitTorrent swarm (TCP/uTP + DHT + trackers). Every file is addressed by its sha256 hash, so the file's identity is what it is, not where it lives. Any peer holding the bytes can serve them to any other peer, and a board joins the swarm to pull bytes it then re-serves to browsers over plain HTTP. The content layer has no central server, no CDN, no single point of failure. Taking down any host, instance, or the project's primary domain does not affect content availability as long as at least one peer is seeding.

### 2. Metadata layer

Signed Nostr events describing each file. A Bakemono manifest is a Nostr event of kind 31063 (in the NIP-33 parameterized-replaceable range) with tags carrying file hash, file size, MIME, magnet link, source platform, creator handle, post id, etc. The event is signed by the contributor's secp256k1 public key using Nostr's standard BIP-340 Schnorr scheme.

Events are published to multiple Nostr relays simultaneously. They are NOT distributed peer-to-peer like the file bytes. The metadata layer is replicated horizontally across many independent relays, because metadata is small, must be queryable (search, browse, filter), and must allow per-board moderation decisions (DMCA, takedowns, abuse).

An event does not include the file bytes. It points at them via the `x` tag (sha256) and the `magnet` tag.

### 3. Discovery layer

Each Bakemono board runs an indexer that subscribes to a configured set of Nostr relays, ingests kind 31063 events, dedupes by event id and file hash, and stores everything in a local postgres for fast querying. On top of postgres, the board serves search/browse UI. Each operator picks their own technology, design, and moderation posture. There is no custom federation protocol because Nostr's relay-multi-publish model is itself the federation.

## File lifecycle

The end-to-end flow of one file from scrape to view in another user's browser. Reference for the entire system.

1. **Retrieve.** Alice runs the desktop app. She has an active subscription on a source platform and is signed in via the embedded webview. The app uses her local session to retrieve content she already has access to; credentials never leave her machine. It writes `post123_image.jpg` (240 KB) to her hard drive.

2. **Hash and build event.** App computes sha256, gets `a3f8d2e1...`. Reads source metadata (source handle, post id, title, timestamp). Generates a small preview client-side (downscaled image, or a poster frame for video) and either inlines it in the `thumb` tag or seeds it as its own tiny file referenced by `thumb_x` + `thumb_magnet`, so a board can show previews without ever fetching the full file. Builds a Nostr event of kind 31063 with tags `x`, `size`, `m`, `magnet`, `platform`, `creator`, `post_id`, etc. Signs with Alice's secp256k1 private key using BIP-340 Schnorr (one call via the `nostr` rust crate).

3. **Seed.** App creates a torrent from the file and seeds it over classic BT with librqbit, on a fixed listen port. Joins the DHT and announces to the magnet's trackers: "I have this infohash at my IP:port". Alice's computer is now a peer in the swarm for this file.

4. **Publish to relays.** App fans out the event to its full configured relay set in parallel: our `wss://relay.bakemono.app`, plus public Nostr relays (relay.damus.io, nos.lol, nostr.wine, etc). Each relay verifies the signature, stores the event, and starts streaming it to its subscribers. NO bytes go to any relay, only the event.

5. **Browse.** Bob lands on a Bakemono board and searches for a source handle. The board's postgres (filled by its indexer subscribing to the same relays Alice published to) returns Alice's event. Page renders with post title, text, image placeholder.

6. **Preview.** The page renders `<img src="/t/{infohash}/f/0">`. The board's gateway resolves that infohash to the magnet in its catalog, joins the swarm with librqbit (finding Alice via the magnet's trackers and the DHT, or a pinned seeder), pulls the 240 KB, verifies each piece against the infohash, and streams the bytes back to Bob over HTTP with a long immutable cache header. Bob's browser did no P2P.

7. **Persistence.** Bob's browser is a plain HTTP client and never joins the swarm. The file stays available as long as someone seeds it: Alice's daemon, any operator running a standard BT client, or the board itself once it has pulled the bytes (it uploads what it holds to other peers like any leecher). Nothing depends on the primary domain.

## Code layout

The repo is a single Cargo workspace with three rust crates:

```
Bakemono/
  Cargo.toml              # workspace root
  crates/
    bakemono-core/        # shared library crate
      src/
        events/           # kind 31063 manifest, kind 31064 takedown
        tags.rs           # tag schema constants and helpers
        validation.rs     # event well-formedness checks beyond Nostr's base
        protocol.rs       # kind constants, version constants
    bakemono-board/       # binary crate (the web instance)
      src/
        main.rs
        web/              # axum routes, maud SSR pages
        indexer/          # subscribes to relays, ingests events into postgres
        db/               # sqlx queries
        cache/            # warm cache for popular files
        mod_queue/        # first-seen-pubkey gating, takedown application
        ops_keys/         # instance keypair for signing kind 31064 takedowns
    bakemono-app/         # binary crate (Tauri desktop client)
      src/
        main.rs           # Tauri shell + GUI commands
        daemon/           # background: scrape queue, relay publisher, seeding
        scraper/          # gallery-dl / yt-dlp sidecar invocation
        seeder/           # classic-BT seeding via bakemono-torrent (librqbit)
        identity/         # local secp256k1 keypair management
        relays/           # default relay list, user-configured overrides
  docs/
```

Alongside the rust crates:

- `nostr-rs-relay` (rust binary) sits next to the board as a sidecar, exposing `ws://localhost:8080` to the indexer.
- The torrent engine is `librqbit`, used as a rust library through the `bakemono-torrent` crate: the desktop daemon seeds scraped files, the board runs it leech-only behind the HTTP gateway. No Node, no separate seeder process.
- `gallery-dl` (and `ffmpeg` for thumbnails) run as Tauri sidecar binaries in the desktop app.

`bakemono-core` is the contract. If a type or routine ends up in both `bakemono-app` and `bakemono-board`, it belongs in core. Keeping I/O out of core means the event-shaping logic stays trivially unit-testable and the workspace builds quickly.

## Component layout

```
+---------------------+
| Alice's machine     |
|---------------------|
| bakemono-app GUI    |
| bakemono-daemon     |
|   scraper           |
|   event signer      |
|   librqbit seeder --+--- classic BT (TCP/uTP + DHT + trackers) ---+
|   relay publisher   |                                             |
+---------+-----------+                                             v
          |                                          +--------------------------+
          | WebSocket EVENT publish (fan out)        |     BitTorrent swarm      |
          v                                          +------------+-------------+
+-------------------------+  +-------------------------+          ^
| wss://relay.bakemono... |  | wss://relay.damus.io    | ...      | gateway pulls
| nostr-rs-relay sidecar  |  | (public Nostr relay)    |          | (librqbit, leech-only)
+-----------+-------------+  +-----------+-------------+           |
            ^                            ^                         |
            |  WebSocket REQ subscribe   |                         |
            |  {"kinds":[31063],...}     |                         |
+-----------+----------------------------+-------------------------+
| bakemono-board                                                  |
|-----------------------------------------------------------------|
|  Indexer (subscribes to many relays) -> Postgres (searchable)   |
|  Torrent -> HTTP gateway (librqbit)                             |
|  Mod queue + takedown signer                                    |
+--------------------------------+--------------------------------+
                                 | HTTP (bytes + Range, immutable cache)
                                 v
                           Bob's browser  (<img>/<video>, no P2P)
```

## NAT and connectivity

Classic BitTorrent has no browser-style rendezvous, so cold-fill needs at least one reachable side. The browser never touches any of this - it is a plain HTTP client of the board's gateway, so a viewer needs no peer connectivity at all. The burden sits entirely between seeders and the gateway:

- **Seeder listen port**: the daemon/app seeds on a fixed TCP port (default 4250). A seeder behind NAT can still dial out to a reachable peer and upload, so if the board's gateway port is open, the NAT'd seeder connects to it and serves.
- **Gateway listen port**: the board opens `BAKEMONO_GATEWAY_PORT` (default 4240) so NAT'd seeders can reach it. This cannot go through an HTTP-only proxy like Cloudflare; it needs a direct port-forward.
- **Peer pinning**: the gateway can be handed explicit `ip:port` seeders (`BAKEMONO_GATEWAY_PEERS`) to dial directly. The reliable path on a LAN (public trackers hand back the public IP and rarely hairpin two peers behind one NAT) or to a known seedbox.
- **DHT + trackers**: discovery for everyone else. The magnet carries the trackers; the session also runs the DHT.

Realistic split: on a LAN or with a pinned seeder, connection is immediate. Across the internet, an operator seedbox with an open port (or the board's own open gateway port for home seeders to dial) covers the common case; a fully-firewalled seeder with no reachable counterpart is the degraded case, same as any classic torrent.

## Federation via Nostr

Bakemono federates over Nostr's relay model. There is no custom instance-to-instance sync protocol because Nostr already solves this.

### How it works

1. A contributor's desktop app holds a list of relays it publishes to (typically 5-10). When the app publishes a manifest, it fans out the event to ALL configured relays in parallel.
2. Each relay independently stores the event. Relays do not coordinate with each other; they are independent stores connected only by clients that publish to several of them.
3. A board's indexer subscribes to a list of relays (its own embedded relay + several public Nostr relays known to carry Bakemono kind 31063 events). The subscription filter is `{"kinds": [31063], "since": <last_seen_unix>}`. Relays stream matching events to the indexer in real time.
4. The indexer stores received events in postgres for fast search/browse, deduping by event id and by `x` tag (file hash). The postgres store is just a cache; the source of truth is the union of events across all relays.
5. Existing Nostr clients (Damus, Amethyst, snort, nostrudel) can also subscribe to kind 31063 directly. Their UX is degraded (they do not know our source-platform tags) but the data is reachable from outside our ecosystem.

### Resilience

- **Our relay goes down**: events already exist on every other relay in the default set. Contributors keep publishing to the rest. Our board's indexer keeps pulling from them. When our relay returns, it backfills from peer relays via standard Nostr re-sync.
- **Our entire board goes down**: search/browse web UI dies (that piece is specifically our infrastructure). The data still exists across many relays. Anyone can spin up a clone board: point its indexer at the same relay set, subscribe from event time zero, postgres fills within minutes. The clone looks and feels identical.
- **A public relay goes down**: events still exist on the others. Contributors' apps detect the failed relay and continue publishing to the rest. Our indexer drops that relay from its sources and keeps going.
- **All relays in the default set go down simultaneously**: would require coordinated failure across many independent operators in many jurisdictions. Not a realistic single-point failure mode.
- **Network partition between regions**: nothing breaks. Relays in each partition keep accepting events. When the partition heals, indexers eventually see events from the other side and the postgres stores reconverge. Signed events with deterministic ids make merge trivial; no conflict resolution needed.

### Mod actions across the federation

Instance operators are themselves first-class Nostr identities, holding their own secp256k1 keypairs. They publish mod actions as kind 31064 events signed by the instance's pubkey. Other boards subscribe to kind 31064 from peer operator pubkeys they trust (configurable). When a board's indexer receives a takedown from a trusted peer, it applies the takedown to its own postgres index per local policy.

A takedown does NOT delete the original event from any relay. It records that this instance has chosen to hide a particular event for a particular reason. Relays we do not operate are unaffected. The event lives wherever it was published.

This reflects the federated model: no single operator holds global authority over the network. Each operator's moderation decisions apply within their own boundary; other operators apply their own policies independently.

### Why this is better than the custom federation we originally specced

- Day-1 federation, not v1. Manifests are redundantly stored across many independent relays from the moment they are published.
- No custom HTTP polling logic to write or maintain.
- Existing infrastructure does the work: nostr-rs-relay handles event storage and subscription; the `nostr` rust crate handles client publishing and verification.
- No relay is privileged, including ours; the protocol does not designate a coordinating authority.
- Bakemono can be reconstituted by anyone, anywhere, by running an indexer pointed at the relay set. The archive does not depend on any single operator's continued participation.

## Identity and auth

Every user's app generates a secp256k1 keypair on first run (the Nostr standard). Private key never leaves the user's machine. Public key is the user's Nostr identity across the entire relay network, including but not limited to Bakemono boards. All manifests are signed by the pubkey. Kudos accrue to the pubkey. The user has the same identity on every relay and every board.

User-facing key format is `nsec` (private) / `npub` (public) bech32-encoded per NIP-19, compatible with any Nostr-aware tool. Users can back up their identity in Damus, Amethyst, Alby, or any other Nostr client.

No accounts. No emails. No passwords. The keypair IS the account.

Anti-spam:

- New pubkeys: each board's indexer holds events from never-seen-before pubkeys in a small mod queue for human review.
- After N approved events (configurable per board, default 3), the pubkey flows freely on that board.
- Spam or abuse: the board publishes a kind 31064 takedown targeting the pubkey (`p` tag) and propagates to peer boards.
- For higher-friction anti-spam, boards can require NIP-13 proof-of-work on incoming events or charge for write access via NIP-57 zaps. Out of scope for v0.

## DMCA and takedowns

The system has three independent surfaces, each with its own scope:

1. **Board's postgres index**: operator marks the event hidden for its local users. Content no longer appears in search or browse on this board. Affects only this board.
2. **Kind 31064 takedown event**: operator publishes a signed takedown to relays. Peer boards subscribing to this operator's takedowns receive it and decide whether to honor it based on jurisdiction. Relays themselves do not automatically delete the targeted event; they just carry the takedown alongside it.
3. **Network-wide content layer**: not under any single operator's control. Independently-operated relays apply their own retention policies. The BitTorrent swarm persists as long as peers continue to seed. This is an inherent property of decentralized protocol design, shared by federated and peer-to-peer systems generally.

Every takedown is itself a signed Nostr event, providing a built-in transparency log. Anyone can subscribe to kind 31064 from a given operator's pubkey and audit their takedown history.

CSAM and other categorically illegal content is moderated proactively at every board regardless of takedown receipt. Boards that refuse to honor `csam`-reason takedowns get dropped from peer trust lists. Hash-based blacklists circulate as kind 31064 events with `x`-tag targets.

## What this architecture buys us

- **No central content host.** No single-IP-block failure mode that has historically taken centralized archives offline.
- **No central metadata host either.** Manifests live on many independent Nostr relays from the first publish. The archive is genuinely distributed.
- **Per-jurisdiction posture.** Each operator determines their own moderation posture in line with local legal obligations. Takedowns are per-board, not global. Relays operated by other parties are not bound by any single operator's decisions.
- **Real browser previews.** The board's torrent -> HTTP gateway makes content viewable in any browser without a plugin, torrent client, or in-page P2P.
- **User-owned identity.** Standard Nostr keypair stays with the user across boards and across the broader Nostr ecosystem. Backup-able in any Nostr client.
- **Bring-your-own credentials.** Each user authenticates to source platforms with their own session; no shared credentials, no pooled access infrastructure.
- **Outlives the founders.** Even if every Bakemono operator quits, the events on relays remain queryable. Anyone can spin up a new board and resurrect the experience
