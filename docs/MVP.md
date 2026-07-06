# MVP

The smallest demoable system on the IPFS architecture: one board scraping server-side, publishing a signed manifest, replicated by keepers, restorable from them. This doubles as the migration plan off the BitTorrent + Nostr stack.

## Acceptance criteria

1. Operator adds a creator and cookies; the scrape worker retrieves the posts; they appear on the board with thumbnails.
2. A browser previews images and short video straight off `/ipfs/{cid}` (the local Kubo gateway) with `Range` working.
3. The manifest head publishes and resolves through both `head.json` and DNSLink, and verifies against the board key.
4. A second host running stock Kubo + `ipfs-cluster-follow` replicates the full pinset with no Bakemono software installed.
5. A takedown removes a file: the next manifest version carries it in `revoked`, the follower unpins it, the gateway refuses to serve it.
6. Kill the board host. On a fresh host, `bakemono restore <cid>` (head fetched from the follower) rebuilds postgres and re-pins everything; the board comes back complete.

If all six work without intervention, MVP ships.

## In scope

- `bakemono-core`: manifest types (head / root / shard), canonical JSON, ed25519 signing, frozen add parameters (see `PROTOCOL.md`)
- `bakemono-board` as the single `bakemono` binary: `serve`, `scrape`, `ingest`, `restore`
- Scrape worker inside `serve`: gallery-dl / yt-dlp invocation, cookie store, ffmpeg thumbnails
- Publisher: shard diffing, head signing, pointer update (head.json + DNSLink), cluster pinset update
- Media served at `/ipfs/{cid}` by the local Kubo gateway (`Gateway.NoFetch` + nopfs denylist); the board writes `bakemono.deny`, a reverse proxy routes `/ipfs/*` to the gateway
- Kubo + `ipfs-cluster-service` deployment on the board host and 1-2 operator keeper hosts
- Published follower config so volunteers can join with `ipfs-cluster-follow`
- Takedown flow end to end (admin UI button -> revoked -> unpin -> denylist)
- Migration of the existing catalog off BitTorrent (step 8 below)

## Out of scope for v0

- Partial adoption by keepers (per-creator or per-collection pinsets)
- Peer board import UI (the manifest format supports it; the polling/merge code comes with the second board)
- Keeper stats, kudos, leaderboards
- IPNI delegated routing
- Mobile app, comments, votes, social features
- Community uploads of any kind (permanent non-goal, not just deferred)

## Build order

Each step ships something demoable.

1. **Rewrite `bakemono-core`.** Head/root/shard types, canonical JSON serializer, ed25519 sign + verify, frozen add-parameter constants. Unit tests: sign/verify roundtrip; canonical form byte-stability; version monotonicity rejection; unchanged shard -> unchanged CID (delta stability); unknown-field tolerance.
2. **Kubo integration in the board.** Add pipeline over the Kubo HTTP API with the frozen parameters; media served at `/ipfs/{cid}` by the local Kubo gateway (`Range` passthrough, NoFetch). Demo: add a file by hand, view it through the board.
3. **Fold scraping into the board.** Move the engine/scraper code from `bakemono-engine` / `bakemono-scraper` into `bakemono-board`; per-creator scrape jobs, cookie store in postgres, thumbnail generation. `bakemono scrape <creator>` works headless; `serve` runs the same jobs on schedule.
4. **Publisher.** Build shards from postgres, diff against the previous version, sign and publish the head, write `head.json`, update DNSLink. Demo: `curl head.json | jq`, walk the DAG by hand.
5. **Cluster.** `ipfs-cluster-service` on the board host + one operator keeper host; publisher updates the pinset through the cluster API. Demo: acceptance criterion 4.
6. **Takedowns.** Admin action -> shard rebuild + revoked entry + pinset removal + denylist write. Demo: acceptance criterion 5.
7. **`bakemono restore`.** Verify chain, rebuild postgres from shards, re-pin pinset. Demo: acceptance criterion 6 on a scratch host.
8. **Catalog migration.** One-off: walk the existing postgres, pull each file's bytes from the warm cache or the still-alive torrent swarms, `ipfs add`, record CIDs, publish manifest version 1. Run while the BT stack still works; do not turn it off first.
9. **Delete the old stack.** Remove `bakemono-app`, `bakemono-daemon`, `bakemono-torrent`, `bakemono-cli`, the relay sidecar and compose service, the Nostr types in core, the GUI release workflow. Rewrite the `/keepers` page for cluster-follow.
10. **Deploy and launch.** Fleet on the prod box + keeper hosts, follower config published, keeper doc live.

## Definition of done per step

- Code reviewed and merged on main
- Integration test covering the happy path runs in CI
- Manual smoke test before merging
- README updated for any new user-facing behaviour

## Team sizing assumption

1-3 people, evenings and weekends. Steps are sequenced so each is independently demoable in case the team shrinks to one person mid-way
