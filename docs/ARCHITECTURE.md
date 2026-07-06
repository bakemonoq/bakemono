# Architecture

Bakemono is built around one core idea: separate WHAT a file is from WHERE it lives, describe the whole archive in one signed structure, and let anyone replicate that structure with stock tools. One curated publisher per board, many independent replicators.

## The three layers

### 1. Content layer

The file bytes. Every file lives in IPFS, addressed by CID, so its identity is what it is, not where it lives. The nodes holding the bytes are the board's own Kubo, the operator's keeper fleet (a few hosts in different failure domains), and volunteer keepers. IPFS does not replicate anything by itself; replication is explicit and orchestrated by the next layer.

### 2. Replication layer

IPFS Cluster maintains the pinset: the authoritative list of every CID that makes up the archive. The operator's own hosts run `ipfs-cluster-service` as trusted peers; volunteers run `ipfs-cluster-follow`, which syncs the pinset and pins everything in it automatically. When a publish adds CIDs they flow out to the whole fleet; when a takedown removes CIDs every follower unpins them on next sync. A keeper needs zero Bakemono software - Kubo plus cluster-follow, both stock (see `docs/KEEPERS.md`).

### 3. Index layer

The manifest: a signed DAG of plain JSON in the same IPFS store (head -> root -> per-creator shards; full spec in `docs/PROTOCOL.md`). It is the source of truth for what the archive contains, the takedown ledger, and the federation format all at once. Because the manifest itself is pinned by the cluster, index durability equals content durability: any keeper can hand you the latest signed head, and the whole board rebuilds from it.

The board's postgres is derived state - a query index built from the manifest, never the truth. Operational data (source cookies, scrape job history, view counters) is host-local and explicitly NOT part of the archive.

## File lifecycle

End-to-end flow of one file, from source to a viewer's browser.

1. **Contribute.** A subscriber pastes their source-platform session cookie into the `/contribute` form. The board validates it against the platform and discovers which creators it can reach, all while it still holds the plaintext (see "Contributor cookies" below).
2. **Scrape.** The board's scrape worker invokes gallery-dl / yt-dlp server-side against each reachable creator. Files land in a staging directory with source metadata sidecars. Because the board fetched the bytes from the source itself, provenance is first-party: there is no substituted-file problem and no per-file verification queue.
3. **Add.** Each file is hashed (sha256), thumbnailed (ffmpeg, ~400px JPEG), and added to the local Kubo with the frozen parameters from `PROTOCOL.md`, yielding a CID. Metadata rows land in postgres.
4. **Publish.** The publisher rebuilds the shards whose content changed, writes a new root, signs a new head (version+1, prev = old head), updates the pointer (head.json + DNSLink), and updates the cluster pinset: new CIDs in, revoked CIDs out.
5. **Replicate.** Cluster peers and followers see the pinset change and fetch the new CIDs from the board's node (and from each other). Within minutes the file exists on every keeper.
6. **Browse.** A viewer hits the board's web UI (axum + maud over postgres). The grid renders thumbnails, the post page renders `<img src="/f/{cid}">` / `<video>`.
7. **Serve.** The `/f/{cid}` route checks the catalog and the denylist, then proxies the local Kubo gateway (`Gateway.NoFetch=true`, so only local blocks are ever served). `Range` passes through; the browser does no P2P and needs no connectivity to anything but the board. A CDN can sit in front; every URL is immutable.
8. **Persist.** The file stays available as long as any node pins it. The board host dying does not remove it from the fleet; every keeper serves it to any IPFS peer, and a resurrected board re-pins everything from them.

## Code layout

Single Cargo workspace, three crates:

```
Bakemono/
  crates/
    bakemono-core/     # manifest types (head/root/shard), canonical JSON, ed25519
                       # signing, frozen add parameters. Pure logic, zero I/O.
    bakemono-scraper/  # thin gallery-dl wrapper: invocation, cookies, streaming, download archive
    bakemono-board/    # the `bakemono` binary
      serve            #   web UI, scrape worker, publisher, gateway proxy
      scrape           #   one-off scrape of a source url into the archive
      ingest           #   import a directory of already-scraped files + sidecars
      restore          #   rebuild postgres + pinset from a head CID
      keygen           #   generate the cookie encryption keypair
      autoimport       #   run one keyed import round (private key from stdin)
  docs/
```

`serve` is the only long-running process. `scrape`/`ingest`/`add` exist for backfills without a running board; `keygen`/`autoimport` drive the sealed-cookie import flow; `restore` exists because on the day it is needed there is no web UI to click.

Alongside, on the board host: postgres, Kubo, `ipfs-cluster-service`. On the operator's other keeper hosts: Kubo + `ipfs-cluster-service` (trusted peers). Volunteers: Kubo + `ipfs-cluster-follow`.

## Component layout

```
+----------------------------------------------+
| board host                                   |
|----------------------------------------------|
| bakemono serve                               |
|   axum web UI  <- postgres (derived index)   |
|   scrape worker (gallery-dl / yt-dlp)        |
|   publisher (shards, head, pointer, pinset)  |
|   /f/{cid} proxy -> local Kubo gateway       |
| kubo (NoFetch, denylist)                     |
| ipfs-cluster-service (trusted peer)          |
+---------------------+------------------------+
                      | cluster pinset sync + bitswap
        +-------------+--------------+
        v                            v
+------------------+       +----------------------+
| operator keeper  |  ...  | volunteer keeper     |
| kubo + cluster-  |       | kubo + ipfs-cluster- |
| service (trusted)|       | follow               |
+------------------+       +----------------------+

viewer's browser -> board web UI + /f/{cid} over plain HTTP (Range, immutable cache)
peer board       -> GET /head.json, verify sig, import shards, apply revoked
```

## Contributor cookies

The archive stays current by scraping on behalf of many subscribers, not one operator account. Enumeration is gallery-dl's job: each supported platform has a "your subscriptions" feed extractor (Patreon `/home`, Fanbox `/home/supporting`, Boosty `/`) that, given the cookie, walks every creator the cookie reaches and paginates the full history. We do not hand-write per-platform discovery; adding a platform is a feed URL plus a cookie name. The lifecycle is built around never holding a usable credential at rest:

- **Submission.** `/contribute` takes a platform + a session cookie value. While it holds the plaintext, the board runs a one-item feed probe with gallery-dl: a live cookie yields content, a dead one yields nothing and is rejected. Nothing is stored on rejection.
- **Encryption.** If the contributor opts into daily import, the token is sealed - a fresh AES-256-GCM key per cookie, RSA-4096-OAEP-wrapped - and only the ciphertext, the wrapped key, and the nonce are stored. The RSA private key never lives on the server, so a database dump yields nothing decryptable. Without opt-in, the token is used for one import and discarded.
- **First import.** Runs immediately in the background using the plaintext already in hand: gallery-dl scrapes the whole subscription feed, the board ingests each file and derives the creator set from the sidecar metadata.
- **Daily round.** `bakemono autoimport` reads the RSA private key from stdin (piped over SSH from the operator's machine, never written to disk), decrypts each live cookie in memory, re-scrapes its feed (gallery-dl's download archive keeps it to new posts), and re-seals nothing. A cookie the platform rejects is marked dead. Operators who accept keeping the key on the box can set `BAKEMONO_COOKIE_PRIVKEY` and let `serve` run the round on a schedule instead.

Only platforms with a subscription-feed extractor in gallery-dl fit this model (Patreon, Fanbox, Boosty). Platforms that expose only per-creator scraping cannot enumerate a cookie's subscriptions and are out of scope. Cookies are operational state: they live only in postgres, never in the manifest, never replicated to keepers.

## Moderation and takedowns

Moderation happens in the manifest, not in the network. Two decisions exist:

1. **What gets in.** Ingestion is the board's own scrape worker driven by contributor cookies, so admission control is source-level: which platforms, which creators. There is no file upload endpoint, no per-file review queue, no first-seen-contributor gating - those defended against untrusted writers, and there are none.
2. **What comes out.** A takedown removes the entry from its shard, appends the target to `revoked` with a reason, publishes a new version, and drops the CIDs from the pinset. From there it is automatic: the fleet and every follower unpin, periodic GC frees the bytes, and the gateway denylist blocks serving the CID regardless. Full semantics in `PROTOCOL.md`.

Three surfaces, by reach:

- **Own fleet and followers**: takedown fully enforced, automatically. Distinctly stronger than the old BitTorrent model, where a third-party seeder could not be stopped.
- **Peer boards**: see `revoked` on next import, apply per local policy. `csam` is applied unconditionally everywhere; refusing boards get dropped from `peers` lists.
- **Independent IPFS nodes** that pinned a CID on their own: outside anyone's control, inherent to open networks. The signed history chain at least proves what was removed, when, and why.

Every takedown is permanently auditable: heads chain by hash, and the chain stays pinned.

## Federation between boards

A board imports another board's archive with two facts: the peer's pubkey and a pointer URL.

1. Poll the pointer (HTTPS or DNSLink), verify the head signature and version monotonicity.
2. Fetch the root, then only the shards whose CIDs changed since the last import; merge entries into the local postgres index.
3. Apply the peer's `revoked` list per local policy.
4. Optionally run `ipfs-cluster-follow` against the peer's cluster to co-host their bytes, not just their index. Without it, an importing board links or proxies to the origin for content it does not hold.
5. The peer's `peers` list seeds discovery of further boards.

There is no push, no handshake protocol, no shared state - polling a signed pointer is the whole transport. Two boards importing each other converge without coordinating.

## Connectivity notes

- **Inside the fleet**: cluster peers and followers connect directly (the follower config carries bootstrap addresses; `Peering.Peers` keeps fleet nodes glued). Content transfer between keepers never depends on public DHT lookups.
- **Public DHT**: best-effort extra reachability. At archive scale, reproviding every block is the classic Kubo pain, so nodes run `Reprovider.Strategy=roots` - only the CIDs manifests actually reference get announced. Delegated routing (IPNI) can come later.
- **Ports**: a keeper works outbound-only; an open libp2p port (4001) makes it fetchable by strangers and is recommended, not required.
- **Browsers**: plain HTTP clients of a board. No P2P, no connectivity requirements.

## Recovery

Total loss of the board host:

1. Provision a new host: postgres, Kubo, cluster-service, the `bakemono` binary.
2. Restore the board key from offline backup.
3. Obtain the latest head CID: DNSLink if DNS survived, otherwise any keeper.
4. `bakemono restore <cid>`: verifies the chain, rebuilds postgres from the shards, re-pins the pinset. Bytes flow back from keepers.
5. Point DNS at the new host; `serve` resumes. Source cookies are re-added by hand (operational state, never in the archive).

If at least one keeper survives, the archive survives. The board key backup is the only thing that must never be lost - and losing it stops future publishes, not existing content.

## What changed from the previous design and why

Until mid-2026 Bakemono was a desktop app (Tauri) whose users scraped sources themselves, seeded files over classic BitTorrent, and published per-file manifests as signed Nostr events to public relays; boards indexed relays and gatewayed torrents to HTTP. It was retired for four reasons:

- **Leecher economics.** Nearly all app users downloaded and closed the app; durable seeding came only from dedicated parties. The design now targets those parties (keepers) directly.
- **Per-file torrents do not scale operationally.** One swarm per file meant hundreds of thousands of tiny swarms: per-infohash DHT announces, tracker scrapes, and torrent-client UIs that degrade past tens of thousands of entries. A pinset is one list managed by one daemon.
- **Untrusted writers forced heavy verification.** User-published manifests meant byte-verification pipelines and moderation queues to catch substituted files. Server-side scraping makes provenance first-party and deletes that machinery.
- **Remote takedown was impossible.** A commodity BT client seeds whatever it was given. Cluster followers honor pinset removals automatically, which the project's moderation posture requires.

Nostr went with it: with one publisher per board there is nothing multi-writer left to federate, and a signed manifest in IPFS carries the index, the takedown ledger, and the transparency log in one structure that keepers already replicate
