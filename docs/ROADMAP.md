# Roadmap

What comes after MVP. Not commitments, just the working order.

## v0 (MVP)

See `MVP.md`. One board scraping server-side, signed manifest in IPFS, operator fleet + volunteer keepers via IPFS Cluster, restore-from-keeper proven.

## v1: Scoped adoption, keeper visibility, more sources

- Partial adoption for keepers: today a follower pins the whole archive. Split the pinset per collection (a creator or a platform slice) as separate cluster follows, so a keeper can back just the slice they care about with a bounded disk budget.
- Keeper stats on `/keepers`: cluster peer count, replication per shard, total bytes held by the fleet, endangered slices (fewest replicas first).
- Additional platforms: the gallery-dl ecosystem supports many extractors; wire more into the scrape worker and admin UI.
- Denylist distribution: publish the board's revoked list in a format Kubo's nopfs can consume directly, so keepers and unrelated gateways can subscribe.

## v2: Second board, real federation

- Operate a second board in a different jurisdiction: own key, own moderation policy, importing the first board's manifest.
- Peer import pipeline: pointer polling, head verification, shard merge into the local index, revoked application per local policy.
- Board directory: known boards, uptime, moderation posture, link to each takedown chain.
- Optional co-hosting: an importing board follows the origin's cluster to serve bytes locally instead of proxying.
- IPNI delegated routing if public-DHT discovery proves too slow for non-fleet retrieval.

## v3: Search quality, polish, localization

- Full-text search on post bodies via postgres tsvector, fuzzy creator names, faceted filters by platform / tier / date range.
- Gateway streaming improvements: seek-ahead prefetch for video, HTTP/3 to the browser.
- Localization (Japanese, Russian, Chinese, Spanish, Portuguese).

## v4 and beyond

- Tor / i2p access for boards that want it.
- Read-only mobile-friendly UI improvements (mobile stays browse-only).
- Cold-storage keeper tier: keepers that hold rarely-read slices with laxer latency expectations.

## Things we will not build

- A payment system that compensates source creators. This is an archive utility, not a payments platform.
- Community uploads or per-user publishing. The board is a curated archive with one writer; that is the trust model, not a missing feature.
- Live streaming, comments, votes, follow graph. Not what this is for.
- Anything that turns the project into a publisher rather than an archive (paid premium content, exclusivity deals). Hard line
