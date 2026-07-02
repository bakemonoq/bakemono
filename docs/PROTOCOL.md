# Protocol

Bakemono uses **Nostr** as its metadata wire format and federation transport. This means: signed events published to relays, identity via secp256k1 / Schnorr keys, censorship resistance via multi-relay publishing. The actual file bytes flow over classic BitTorrent (TCP/uTP + DHT + trackers); a board joins the swarm and re-serves those bytes to browsers as plain HTTP, but the manifest protocol below is unchanged.

The full reasoning for choosing Nostr lives in `docs/ARCHITECTURE.md`. The short version: Nostr already solves federation, durability across operators, and pubkey identity. We get those for free instead of inventing them.

The canonical rust implementation of every event type and routine described in this document lives in the `bakemono-core` crate, wrapping the `nostr` crate from rust-nostr.org. Both `bakemono-app` (which builds and signs events) and `bakemono-board` (which verifies, indexes, and serves them) import it.

## The Bakemono Manifest Event

A Bakemono manifest is a Nostr event of **kind 31063** (in the parameterized-replaceable range 30000-39999 per NIP-33; chosen as a thematic riff on NIP-94 file metadata kind 1063). Every event is signed by the contributor's secp256k1 public key using the standard Nostr Schnorr signature scheme (BIP-340).

### Event shape

```json
{
  "id": "<sha256 of canonical event-form, hex>",
  "kind": 31063,
  "pubkey": "<32-byte secp256k1 x-only pubkey, hex>",
  "created_at": 1717527612,
  "tags": [
    ["d", "<platform>:<creator_id>:<post_id>:0"],
    ["x", "<sha256-hex-of-file>"],
    ["size", "245760"],
    ["m", "image/jpeg"],
    ["filename", "post123_image.jpg"],
    ["magnet", "magnet:?xt=urn:btih:<sha1-hex-of-info-dict>&dn=..."],
    ["platform", "<source-extractor-id>"],
    ["creator", "<source-handle>"],
    ["creator_id", "<source-id>"],
    ["post_id", "<post-id>"],
    ["post_title", "March art dump"],
    ["posted_at", "2026-03-14T10:00:00Z"],
    ["tier", "subscriber"]
  ],
  "content": "post body text, if any. plain text or markdown.",
  "sig": "<128-hex-char Schnorr signature>"
}
```

### Required tags

- `d` - the **replaceable identifier**. Format `<platform>:<creator_id>:<post_id>:<file_index>` where `file_index` is `0` for single-file posts. Drives NIP-33 dedup: relays keep only the latest event per `(pubkey, kind, d)` triple, which lets a contributor update or replace their own manifest cleanly.
- `x` - sha256 of the file bytes, lowercase hex (no `sha256:` prefix; Nostr convention is bare hex).
- `size` - file size in bytes as a decimal string.
- `m` - MIME type from the file's magic bytes, not extension.
- `magnet` - BitTorrent v1 magnet link, full URI. Format: `magnet:?xt=urn:btih:<sha1-of-bencoded-info-dict>&dn=<filename>&tr=...`. The infohash is sha1, what BT v1 uses and what librqbit produces when it creates the torrent. The `x` tag above is sha256 of the file bytes and is independent of the infohash; the two coexist (one identifies the torrent the board's gateway joins, the other identifies the file content for dedup).
- `platform` - lowercase identifier matching the source extractor name. Clients SHOULD use the gallery-dl extractor identifier for interoperability. Any non-empty lowercase string is valid; indexers MUST treat unknown values as opaque.
- `creator` - human-readable source handle.
- `creator_id` - source-specific creator identifier.
- `post_id` - source-specific post identifier.

### Optional tags

- `filename` - original filename if known.
- `post_title` - title of the source post.
- `posted_at` - ISO 8601 timestamp of when the source post was published.
- `tier` - `free`, `subscriber`, or `unknown`. Whether the source content required an authenticated subscription.
- `t` - free-form topic tags per NIP-12. Multiple `t` tags allowed. E.g., `["t", "furry"]`, `["t", "art"]`.
- `thumb` - inline thumbnail as a base64 `data:` URL. Only for very small previews (target a few KB). Lets a board render a placeholder with zero swarm fetch. See Thumbnails below.
- `thumb_x` - sha256 of a separately-seeded thumbnail file, used when the preview is too large to inline. Same encoding as `x`.
- `thumb_magnet` - magnet for the thumbnail referenced by `thumb_x`. The thumbnail seeds as its own tiny torrent, so a board can preview without ever fetching the full-resolution file.

### Content field

The `content` field is the source post's body text, plain or markdown. Empty string if the source post has no body.

## Thumbnails

Previews are generated client-side by the desktop app at scrape time, never by the board. After hashing the original file the app shells out to a bundled ffmpeg to make one downscaled JPEG frame (longest side ~400px) - a single frame for images and animated GIFs, a poster frame a second in for video - and attaches it one of two ways:

- A seeded preview is its own small file, referenced by `thumb_x` (sha256) + `thumb_magnet`. It joins the swarm like any other file, so previews stay decentralized and survive the original's seeders going away. This is what v0 produces.
- A tiny preview (a few KB) can instead go inline in the `thumb` tag as a base64 `data:` URL, rendered by a board with no swarm activity at all. The tag exists for this but v0 does not emit it.

A board therefore never pulls the full-resolution file just to fill a grid of previews; the full file is fetched only when a user actually opens it. Thumbnails are immutable and content-addressed like everything else, so identical previews dedupe by hash

Signing follows Nostr's standard NIP-01. No custom canonicalization, no `serde_jcs`.

1. Build a JSON array: `[0, pubkey, created_at, kind, tags, content]` with no insignificant whitespace, UTF-8 encoded.
2. sha256 the bytes. The result is the event `id`, lowercase hex.
3. Sign the 32-byte id with the contributor's secp256k1 private key using BIP-340 Schnorr signatures. The signature is 64 bytes, hex-encoded.

In rust this is one call via the `nostr` crate:

```rust
use nostr::{EventBuilder, Keys, Kind, Tag};

let keys = Keys::generate();
let event = EventBuilder::new(Kind::Custom(31063), content)
    .tags(tags)
    .sign(&keys)
    .await?;
```

## Identity

Contributors are identified by their secp256k1 public key (32 bytes, x-only form per BIP-340).

- Generation: `Keys::generate()` from the `nostr` crate.
- Storage: file in app data dir, mode 600. Optionally encrypted with a user passphrase (out of scope for MVP).
- Export / backup: standard Nostr `nsec` format (bech32-encoded private key per NIP-19). Compatible with any Nostr client, so users can back up their identity in any Nostr-aware tool.
- Hardware wallet support: secp256k1 is what Bitcoin uses. Hardware wallets that support BIP-340 Schnorr can hold Bakemono keys directly. Post-MVP convenience.

## Relays

A relay is a server that accepts events, stores them, and serves them to subscribers. Bakemono manifests are published to **multiple relays simultaneously** to ensure durability.

### What the reference instance runs

- `nostr-rs-relay` (rust, MIT, by Greg Heartsfield) as a sidecar process colocated with the board. Exposed at `wss://relay.bakemono.app`.
- The board's indexer connects to its own relay (`ws://localhost:8080`) AND to a configured list of public relays.

### Default relay set for the desktop app

The app ships with sensible defaults; users can edit the list. v0 default:

- `wss://relay.bakemono.app` (ours)
- `wss://relay.damus.io` (public, large)
- `wss://nos.lol` (public, persistent)
- `wss://relay.snort.social` (public)
- `wss://nostr.wine` (public; we maintain a paid write account, free read)

Every publish fans out to ALL configured relays. If our relay is down, the rest still receive the event. If a public relay is down, the rest still receive it. The publish operation succeeds as long as at least one relay accepts the event.

### Relay protocol

Standard Nostr WebSocket protocol per NIP-01:

- **Publish**: client sends `["EVENT", <event_json>]`. Relay responds `["OK", <event_id>, true|false, "<message>"]`.
- **Subscribe**: client sends `["REQ", "<sub_id>", <filter>, ...]`. Relay streams matching events, then `["EOSE", "<sub_id>"]` (end of stored events) and continues streaming new ones as they arrive.
- **Close subscription**: client sends `["CLOSE", "<sub_id>"]`.

### Indexer subscription filter

The board's indexer pulls all Bakemono manifests with:

```json
["REQ", "bakemono-ingest", {"kinds": [31063], "since": <last_seen_unix>}]
```

Where `last_seen_unix` is the highest `created_at` the indexer has stored. The relay returns everything newer, plus streams new events in real time.

## Replaceable event semantics

Because we use kind 31063 (parameterized-replaceable per NIP-33), each unique `(pubkey, kind, d_tag)` triple is replaced by the most recent event with the same triple. This means:

- A contributor can update their own manifest (e.g., re-scrape after the creator edited the post) by publishing a new event with the same `d` tag. Relays drop the old version.
- A contributor cannot replace another contributor's manifest. The `pubkey` is part of the dedup key.
- Two different contributors who both scraped the same post create two events with the same `d` content but different `pubkey`. Both events exist. Our indexer dedupes at the display layer by the file `x` (hash) tag.

## Mod actions

Mod actions are separate event kinds, signed by instance operators' keys (instances also hold secp256k1 keypairs and act as first-class Nostr identities).

### Kind 31064: takedown

Signed by an instance operator. Records that this instance has chosen to hide a specific event, hash, or pubkey.

```json
{
  "kind": 31064,
  "tags": [
    ["d", "takedown:<target_type>:<target_value>"],
    ["e", "<event_id>"],
    ["reason", "dmca-us"],
    ["applied_at", "2026-06-27T20:00:00Z"]
  ],
  "content": "optional human-readable explanation"
}
```

Target is one of `e` (event id), `x` (file sha256), `i` (torrent infohash), `p` (contributor pubkey), `post` (`platform:creator_id:post_id`), or `creator` (`platform:creator_id`). An `i` takedown suppresses the bytes at the gateway for every manifest pointing at that swarm, so dedup-by-content cannot keep taken-down bytes reachable through a second manifest. The `reason` tag is a free-form string with conventional values: `dmca-us`, `eu-court-order`, `csam`, `spam`, `wrong-content`, etc

### How peer instances apply mod actions

Each instance's indexer subscribes to kinds 31064 from peer instance pubkeys it trusts (configurable). When it receives a takedown signed by a trusted peer, it applies the takedown to its own postgres index per local policy.

`csam` and other categorically illegal categories are applied unconditionally by every instance, no operator runs without this. Instances that refuse to honor `csam` takedowns get dropped from peer trust lists.

## NIPs we follow

- **NIP-01** - basic protocol, events and relays
- **NIP-11** - relay information document (relay capability discovery)
- **NIP-12** - generic tag queries (the `t` tag for topic queries)
- **NIP-19** - bech32-encoded entities (`nsec` / `npub` / `note` for user-facing keys and ids)
- **NIP-33** - parameterized replaceable events (our kind 31063 lives in this range)
- **NIP-65** - relay list metadata (clients publish their preferred relay set as a kind 10002 event)

## Versioning

- The Bakemono manifest kind is fixed at 31063 for v1 of the schema. Breaking changes introduce a new kind (e.g., 31066) and we publish to both for a transition period.
- Tag additions within v1 must be ignorable by older indexers. No required new tags after launch.

## Reserved kinds

- **31065** is reserved for a future periodic transparency-log aggregate event (signed daily summary of an instance's mod actions). Not implemented in MVP; do not reuse for anything else

## Reserved tags

For future use; do not put unrelated data in these:

- `collection` - parent collection / pack id
- `replaces` - id of a specific event this supersedes for the same contributor, beyond `d` tag dedup
- `lang` - content language (ISO 639-1)
- `nsfw` - boolean string `true` / `false`, content rating hint for clients
