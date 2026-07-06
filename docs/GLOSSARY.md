# Glossary

Quick reference for terms used throughout the Bakemono docs and code.

## IPFS

- **IPFS**: peer-to-peer content-addressed storage network. Where both the archive's bytes and its index live.
- **CID**: Content Identifier. The hash-derived address of a block or file in IPFS; identical bytes added with identical parameters yield an identical CID. We use CIDv1, base32.
- **Content addressing**: identifying data by the hash of its content rather than by location. Changing any byte changes the address, so substitution is detectable by construction.
- **Kubo**: the reference IPFS node implementation. Runs on the board host and on every keeper.
- **UnixFS**: IPFS's file encoding. Manifest objects are plain JSON stored as UnixFS files; CID references inside them are strings, not IPLD links, so pinning an object does not pin what it names.
- **Pin**: instruction to a node to keep a CID and protect it from GC. Unpinned blocks are garbage, not deleted; GC deletes them.
- **GC (garbage collection)**: `ipfs repo gc`, the pass that actually frees unpinned blocks. Nodes must run it periodically or "removed" content stays on disk.
- **Gateway**: an HTTP door into an IPFS node. Browsers load media at `/ipfs/{cid}` directly from the board host's local Kubo gateway (`Gateway.NoFetch=true`, so it serves only blocks it already holds); a reverse proxy routes `/ipfs/*` to it. The board is not in the byte path.
- **NoFetch**: gateway setting preventing the node from fetching arbitrary CIDs from the network on request. Without it a public gateway will happily serve anything, including content the board took down.
- **Denylist / nopfs**: Kubo's block-by-CID mechanism. The board writes revoked CIDs into it so the gateway refuses them regardless of what is on disk.
- **DHT**: the distributed lookup table mapping CIDs to providers. Provider records expire (~36h) and must be reannounced; at archive scale nodes run `Reprovider.Strategy=roots` and rely on direct peering inside the fleet.
- **DNSLink**: a DNS TXT record (`_dnslink.<domain>`) pointing a domain at a CID. One of the three pointer channels to the current head.
- **Peering**: Kubo config pinning permanent connections to named peers. Keeps the fleet directly connected without discovery.

## Cluster

- **IPFS Cluster**: pinset orchestration on top of Kubo. Maintains one authoritative list of CIDs and makes a fleet of nodes converge on pinning it.
- **Pinset**: the cluster's CID list; the definition of "the archive" at the replication layer. Updated by the board's publisher on every manifest publish.
- **Trusted peer**: a cluster node allowed to modify the pinset. The operator's own hosts, running `ipfs-cluster-service`.
- **Follower**: a read-only cluster member running `ipfs-cluster-follow`. Syncs the pinset and pins everything in it; unpins whatever leaves it. What volunteer keepers run.
- **Follower config**: the JSON a volunteer points `ipfs-cluster-follow` at (published by the board). Carries the cluster identity and bootstrap addresses.

## Bakemono

- **Manifest**: the archive's index as a signed DAG of JSON objects in IPFS: head -> root -> shards. The source of truth, the takedown ledger, and the federation format. Spec in `PROTOCOL.md`.
- **Head**: the signed, versioned publication object. Names the root CID, chains to the previous head by CID. What pointers point at and consumers verify.
- **Root**: the per-version index object: shard map, revoked list, peers list.
- **Shard**: one creator's posts on one platform, keyed `<platform>:<creator_id>`. Unchanged shards keep their CID across versions, so syncs fetch only deltas.
- **Pointer**: any channel resolving "current head CID": `head.json` over HTTPS, DNSLink, or the pinned head at any keeper. Untrusted transport; the head's signature carries the trust.
- **Revoked**: the append-only list of content removed on purpose, with reasons. Drives automatic unpinning at keepers and denylisting at gateways.
- **Board**: a self-hostable web instance: postgres index, scrape worker, publisher, admin UI. One binary (`bakemono`) plus Kubo (whose gateway serves the media), cluster-service, and postgres.
- **Keeper**: someone donating disk and bandwidth by replicating the pinset. Runs stock Kubo + `ipfs-cluster-follow`; no Bakemono software. See `KEEPERS.md`.
- **Fleet**: the operator's own keeper hosts, run as trusted cluster peers across failure domains.
- **Board key**: the ed25519 keypair that signs heads. The board's identity; its offline backup is the recovery lynchpin.

## Tooling

- **gallery-dl**: Python content extractor supporting many source platforms. The scrape worker's retrieval engine, invoked server-side.
- **yt-dlp**: Python video extractor, same role for video sources.
- **ffmpeg**: generates the ~400px JPEG thumbnails at scrape time.
- **bakemono-core**: shared rust crate holding manifest types, canonical JSON, ed25519 signing, and the frozen add parameters. Pure logic, no I/O.
- **axum**: rust async web framework on tokio. Board's HTTP layer.
- **sqlx**: rust async Postgres driver with compile-time-checked queries. Board's DB layer.
- **maud**: rust compile-time HTML template macro. Board's SSR rendering
