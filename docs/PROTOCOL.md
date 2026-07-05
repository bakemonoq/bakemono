# Protocol

Bakemono's entire wire format is one data structure: the **manifest**, a DAG of plain JSON files stored in IPFS and signed by the board's key. A board publishes its archive as a manifest, keepers replicate the CIDs it lists, and other boards import it. There are no relays, no per-user events, no other message types.

The canonical implementation of every object and routine below lives in `bakemono-core`. Everything is plain JSON stored as UnixFS files, so the whole protocol is inspectable with `curl` and `jq` against any IPFS node.

## Object graph

```
pointer (DNSLink / head.json / cluster pinset)
  -> head    signed, versioned, chains to the previous head
     -> root    shard index + revoked list + peers
        -> shard    one creator's posts
           -> file CIDs    content bytes and thumbnails
```

One subtlety that shapes everything below: manifest objects are plain JSON, so CID references inside them are strings, not IPLD links. Pinning a shard does not pin the files it names. That is deliberate - it lets the full manifest history stay pinned forever (it is tiny) while revoked file bytes are dropped.

## Head

The unit of publication. Every publish creates a new head; the head is what the pointer names and what consumers verify.

```json
{
  "schema": 1,
  "version": 42,
  "root": "bafy...",
  "prev": "bafy...",
  "published_at": "2026-07-05T12:00:00Z",
  "pubkey": "<32-byte ed25519 public key, hex>",
  "sig": "<64-byte ed25519 signature, hex>"
}
```

- `schema` - manifest format version, fixed at 1. A breaking change bumps it.
- `version` - monotonically increasing integer, +1 per publish.
- `root` - CID of the root index.
- `prev` - CID of the previous head, `null` for the first publish. Heads form a hash-linked chain: anyone pinning old heads holds a verifiable history of every version of the archive, including every takedown. This chain is the transparency log; there is no separate one.
- `pubkey` - the board's ed25519 public key, and the board's identity.
- `sig` - signature over the canonical form.

### Signing

Serialize the head without the `sig` field as canonical JSON: UTF-8, object keys sorted bytewise, no insignificant whitespace. Sign the bytes with the board's ed25519 key, hex-encode into `sig`. The head holds only strings and integers by design, so canonicalization needs no float or unicode-normalization rules.

### Verification

Consumers (keeper tooling, peer boards, `bakemono restore`) accept a head only if:

1. `sig` verifies against `pubkey` over the canonical form.
2. `pubkey` equals the key already trusted for this board. Keys are exchanged out of band (or learned from a trusted board's `peers` list); the pointer channels are untrusted transport.
3. `version` is strictly greater than the last version accepted from this key. This rejects replay of an older, validly-signed head.

## Root

The index of the archive at one version.

```json
{
  "shards": {
    "patreon:12345": { "cid": "bafy...", "posts": 210, "bytes": 5203942342 },
    "fanbox:9876":   { "cid": "bafy...", "posts": 34,  "bytes": 120394234 }
  },
  "revoked": [
    { "cid": "bafy...", "sha256": "<hex>", "reason": "dmca-us", "revoked_at": "2026-06-27T20:00:00Z" },
    { "creator": "patreon:666", "reason": "csam", "revoked_at": "2026-06-01T00:00:00Z" }
  ],
  "peers": [
    { "name": "board.example", "pubkey": "<hex>", "pointer": "https://board.example/head.json" }
  ]
}
```

- `shards` - keyed by `<platform>:<creator_id>`. A publish rewrites only shards whose content changed; an unchanged shard keeps its CID, so keepers and peer boards fetch only the delta.
- `revoked` - content removed on purpose, as opposed to merely absent. Each entry carries `reason`, `revoked_at`, and at least one target: `cid` (one file), `sha256` (same file by byte hash), `post` (`platform:creator_id:post_id`), or `creator` (`platform:creator_id`). `reason` is free-form with conventional values: `dmca-us`, `eu-court-order`, `csam`, `spam`, `wrong-content`. The list is append-only across versions.
- `peers` - other boards this board knows and trusts enough to name. This is the discovery gossip: importing one board's manifest teaches you about others.

## Shard

All archived posts of one creator on one platform.

```json
{
  "platform": "patreon",
  "creator_id": "12345",
  "creator": "somehandle",
  "posts": [
    {
      "post_id": "98765",
      "title": "March art dump",
      "body": "post body text, plain or markdown",
      "posted_at": "2026-03-14T10:00:00Z",
      "tier": "subscriber",
      "files": [
        {
          "cid": "bafy...",
          "sha256": "<hex of the raw file bytes>",
          "size": 245760,
          "mime": "image/jpeg",
          "filename": "post123_image.jpg",
          "thumb": "bafy..."
        }
      ]
    }
  ]
}
```

- `cid` - the file, added with the frozen parameters below.
- `sha256` - hash of the raw bytes. Independent of the CID (a chunked file's CID is a DAG root, not the byte hash). Kept for dedup across boards and for integrity checks outside IPFS.
- `mime` - from magic bytes, not extension.
- `thumb` - CID of the preview JPEG, generated at scrape time (longest side ~400px, a poster frame for video). Optional.
- `tier` - `free`, `subscriber`, or `unknown`.

Posts sort newest first. `body` is an empty string when the source post has none.

## File addition parameters

Same bytes MUST map to the same CID on every board, or cross-board dedup and revocation-by-CID silently break. Files are added with frozen parameters, protocol constants in `bakemono-core`:

- CIDv1, base32 text encoding
- raw leaves
- sha2-256
- fixed chunker `size-1048576` (1 MiB)
- no wrapping directory, no filename in the DAG

Any deviation forks the CID for identical bytes. Manifest JSON objects are added with the same parameters.

## Pointer

The head CID must be discoverable even when the board is down. Three redundant channels, all untrusted (the signature carries the trust):

1. **HTTPS**: `GET https://<board>/head.json` returns the current head verbatim.
2. **DNSLink**: `_dnslink.<board>` TXT record `dnslink=/ipfs/<head-cid>`, resolvable by any IPFS node without touching the board.
3. **Cluster pinset**: the head is pinned, so every keeper holds the newest head their follower has synced. Disaster recovery is "ask any keeper".

## Pinset

What the cluster pins, i.e. what keepers replicate:

- every file CID and thumbnail CID referenced by the current version
- every shard, root, and head, including all historical ones (they are small JSON; keeping them preserves the transparency chain)
- NOT the file CIDs of revoked content: those leave the pinset in the same publish that revokes them

Because JSON references are not IPLD links, pinned historical shards do not re-anchor revoked bytes; the hashes remain auditable while the content is gone from every honoring node.

## Revocation semantics

Removing content from the archive:

1. The entry disappears from its shard; the target lands in `revoked` with a reason.
2. A new version publishes; the revoked CIDs leave the cluster pinset.
3. The operator fleet and every follower unpin on their next sync; periodic GC frees the bytes.
4. The board's gateway denylist gains the CID, so the gateway will not serve it even if some node still holds it.
5. Peer boards see the new `revoked` entries on their next import and apply them per local policy. `csam` entries are applied unconditionally by every board; a board that refuses is dropped from `peers` lists.

What revocation cannot do: force a node outside the cluster that pinned the CID independently to drop it. The manifest records intent; enforcement ends at the trust boundary, as in any open network.

## Identity

One ed25519 keypair per board. It signs heads and nothing else.

- Storage: a file on the board host, mode 600, plus an offline backup. The key outliving the server is the whole recovery story, so back it up before first publish.
- Losing the key stops publication permanently; already-published history remains valid and fetchable.
- Rotation: publish a final head under the old key whose root `peers` entry names the new key, then re-establish trust with keepers and peer boards out of band.

There are no user keys. Users do not publish; the board is the only writer of its manifest.

## Versioning

- New optional fields may appear in any object; consumers MUST ignore fields they do not know.
- No new required fields within a schema version.
- A breaking change bumps `schema`. A consumer seeing an unknown schema stops and reports rather than guessing
