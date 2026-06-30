# Architecture

Bakemono is built around one core idea: separate WHAT a file is from WHERE it lives, replicate the WHAT across many independent operators, and let the bytes flow peer to peer.

## The three layers

### 1. Content layer

The actual file bytes. Lives in a BitTorrent v1 + WebRTC swarm via the `webtorrent` package. Every file is addressed by its sha256 hash, so the file's identity is what it is, not where it lives. Any peer holding the bytes can serve them to any other peer. The content layer has no central server, no CDN, no single point of failure. Taking down any host, instance, or the project's primary domain does not affect content availability as long as at least one peer is seeding.

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

3. **Seed.** App spins up a BitTorrent v2 client locally. Joins the DHT. Announces "I have hash a3f8 at my IP:port". Alice's computer is now a peer in the swarm for this file.

4. **Publish to relays.** App fans out the event to its full configured relay set in parallel: our `wss://relay.bakemono.app`, plus public Nostr relays (relay.damus.io, nos.lol, nostr.wine, etc). Each relay verifies the signature, stores the event, and starts streaming it to its subscribers. NO bytes go to any relay, only the event.

5. **Browse.** Bob lands on a Bakemono board and searches for a source handle. The board's postgres (filled by its indexer subscribing to the same relays Alice published to) returns Alice's event. Page renders with post title, text, image placeholder.

6. **Preview.** Page JS sees an image with hash `a3f8`. Spins up a WebTorrent client in Bob's browser (no plugin; uses WebRTC). WebTorrent contacts the DHT, finds Alice (and any other seeders), opens a WebRTC data channel directly to her seeder, downloads 240 KB into a Blob, creates an object URL, swaps it into `<img src>`. Image appears.

7. **Re-seed.** While Bob's tab is open, his browser also advertises hash `a3f8` to the DHT. If Carol arrives, she may fetch from Alice OR Bob. When Bob closes the tab, he drops out of the swarm. The file persists as long as anyone is seeding.

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
        daemon/           # background: scrape queue, relay publisher, IPC to webtorrent sidecar
        scraper/          # gallery-dl / yt-dlp sidecar invocation
        seeder/           # spawns and supervises the webtorrent Node sidecar
        identity/         # local secp256k1 keypair management
        relays/           # default relay list, user-configured overrides
  docs/
```

Alongside the rust crates, two Node sidecars run:

- `nostr-rs-relay` (rust binary) sits next to the board, exposing `ws://localhost:8080` to the indexer.
- `webtorrent` (Node, >=2.3.0) runs as a Tauri sidecar in the desktop app (seeds scraped files over BT v1 + WebRTC) and also runs on the board for the warm-cache fetcher/seeder. Same package, both roles.

Neither sidecar is part of the rust workspace; both are pulled in as system binaries or container images.

`bakemono-core` is the contract. If a type or routine ends up in both `bakemono-app` and `bakemono-board`, it belongs in core. Keeping I/O out of core means the event-shaping logic stays trivially unit-testable and the workspace builds quickly.

## Component layout

```
+---------------------+                       +---------------------+
| Alice's machine     |                       | Bob's machine       |
|---------------------|                       |---------------------|
| bakemono-app GUI    |                       | Browser             |
| bakemono-daemon     |                       |  webtorrent in JS   |
|   scraper           |                       |  Bakemono web UI    |
|   event signer      |                       +----------+----------+
|   webtorrent side-  |                                  |
|   car (BT+WebRTC)   |                                  | WebRTC P2P
|   relay publisher   |                                  | (direct bytes)
+---------+-----------+                                  |
          |                                              |
          | WebSocket EVENT publish                      |
          | (fan out to many relays)                     |
          v                                              |
+-------------------------+   +-------------------------+
| wss://relay.bakemono... |   | wss://relay.damus.io    | ... and more
| nostr-rs-relay sidecar  |   | (public Nostr relay)    |
+-----------+-------------+   +-----------+-------------+
            ^                             ^
            |                             |
            |  WebSocket REQ subscribe    |
            |  {"kinds":[31063],...}      |
            |                             |
+-----------+-----------------------------+-------------+
| bakemono-board                                        |
|-------------------------------------------------------|
|  Indexer (subscribes to many relays)                  |
|  Postgres metadata (deduped, searchable)              |
|  Warm cache                                           |
|  axum + maud SSR web frontend ----- HTTPS ----------> | Bob's browser
|  Mod queue + takedown signer                          |
+-------------------------------------------------------+
```

## NAT and connectivity

Most home users are behind NAT, meaning their device cannot accept unsolicited inbound TCP/UDP. P2P solves this with ICE (Interactive Connectivity Establishment):

- **STUN**: peer asks a public server "what does my public IP:port look like to you?". Free, lightweight. We use multiple public servers plus run our own.
- **Hole punching**: both peers simultaneously fire outbound packets, many NATs accept the inbound as a legitimate response.
- **TURN**: relay server, fallback for symmetric / carrier-grade NAT. Eats real bandwidth. We will run a small TURN cluster behind a rate limit.
- **UPnP / NAT-PMP**: app politely asks the home router to open a port. Many routers comply.

The `webtorrent` package (used in the browser and in the desktop daemon via Node sidecar) handles ICE end-to-end through its WebRTC layer. We wire it up; we do not reimplement it.

Realistic split: about 80% of users connect peer-to-peer directly via STUN + hole punching. About 15% need TURN relay for some or all sessions. About 5% have NAT hostile enough that connectivity degrades to "download via desktop client" rather than browser streaming. This is acceptable.

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
- **Real browser previews.** WebTorrent makes content viewable in browser without a plugin or torrent client.
- **User-owned identity.** Standard Nostr keypair stays with the user across boards and across the broader Nostr ecosystem. Backup-able in any Nostr client.
- **Bring-your-own credentials.** Each user authenticates to source platforms with their own session; no shared credentials, no pooled access infrastructure.
- **Outlives the founders.** Even if every Bakemono operator quits, the events on relays remain queryable. Anyone can spin up a new board and resurrect the experience
