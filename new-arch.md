# HTTP Gateway (torrent -> HTTP)

The gateway is the only component that speaks BitTorrent. It joins swarms with full
classic BitTorrent (TCP/uTP, DHT, PEX, web seeds), pulls bytes, caches them, and exposes
plain HTTP to browsers. Clients do zero P2P: an `<img>` or `<video>` tag points at a
gateway URL and the gateway hides the fact that the bytes came from a torrent.

Content is addressed by `(infohash, file)`. Because an infohash fixes the bytes forever,
every gateway URL is immutable, which is the property the whole speed story is built on.

## Request flow

```
browser -> CDN edge -> gateway -> [cache tiers] -> swarm (cold fill only)
```

1. Request arrives for `(infohash, file, optional transform)`.
2. Cache hit (assembled file or thumbnail variant) -> serve from disk/memory, HTTP-fast. This is the common case.
3. Miss -> ensure the torrent engine is active: parsed metainfo loaded, swarm joined.
4. For a `Range` request, compute the pieces covering the byte range, prioritize them
   (offset-first, then sequential), stream bytes to the client as each piece verifies.
5. Verify every piece against the infohash (integrity is free), write verified pieces to
   the piece cache, optionally promote a fully-assembled small file to the file cache.

## HTTP API

Stable, deterministic URLs so a commodity CDN and the browser can cache everything.

- `GET /t/{infohash}/meta` -> JSON `{ name, files: [{ index, path, size, mime }] }`. Lets the UI enumerate files without downloading them.
- `GET /t/{infohash}/f/{fileIndex}` -> file bytes, `Range` supported. The target for `<video src>` and full-size images.
- `GET /t/{infohash}/f/{fileIndex}?w=320&fmt=webp` -> server-side resized/transcoded thumbnail, cached as its own object.

All content responses set `Cache-Control: public, immutable, max-age=31536000`. The URL is
the cache key and the content never changes, so caching is always safe.

## Caching tiers

Hottest first. Without WebTorrent, this stack is where speed is won.

1. Browser cache + Service Worker. Immutable URLs cache forever; revisits are zero-network. The SW can pre-cache the next page's thumbnails.
2. CDN edge. Immutable content-addressed URLs make a commodity CDN safe to put in front. The first viewer warms the edge; everyone after is CDN-fast globally.
3. Gateway file cache. Assembled files and thumbnail variants on SSD, LRU by size budget.
4. Gateway piece cache. In-flight and partially-downloaded pieces.
5. Swarm. The cold-fill origin, hit only on a true miss.

## Speed levers

- Immutable URLs + long `Cache-Control`. Single biggest lever: pushes most traffic into tiers 1-2.
- Server-side thumbnails. Never ship a 4MB original to fill a 250px grid cell. Pre-generate a few fixed sizes in webp/avif and cache them. Emit a tiny blurhash/placeholder for instant paint.
- Pre-warm on page load. When the UI requests a page of N posts, the gateway proactively resolves metainfo and fetches the first piece(s)/thumbnails for those N, hiding swarm latency behind render.
- Cache metainfo by infohash permanently. The `.torrent` is immutable; parsing it from the swarm (BEP-9) is the cold-start tax. Cache it forever, and pre-ship metainfos alongside the catalog so the gateway rarely resolves from peers.
- Prefer web seeds. If the magnet carries `ws=`, pull from the HTTP web seed directly. Faster and more reliable than waiting on peers, and it removes the need for any swarm peer on cold fill.
- Keep hot torrents warm. Maintain an LRU of active torrent engines and pin popular content so repeat requests skip metadata resolution and swarm re-handshake.
- HTTP/2 or HTTP/3 to the client. Multiplex a grid's worth of thumbnail requests over one connection instead of paying connection setup per image.
- Sequential piece priority for streaming. Low time-to-first-byte for video and range reads; lean on the operator seed for availability.

## Cold start

The honest weak point: the first-ever view of a long-tail post pays metadata resolve +
peer discovery + download-enough-pieces, which can be seconds. Mitigations, in order:

- Pre-resolve and pin metainfos from the catalog so `/meta` is instant.
- Run an always-on operator seed holding the content, so there is always one fast peer.
- Put `ws=` web-seed URLs in magnets for an HTTP cold-fill path.
- Pre-warm visible posts on page load.

After the first view, edge + local cache make it instant for everyone else.

## Eviction

LRU by total cache-size budget ("download, display, delete" at the gateway). Pin hot
content by request count/recency. Evicted content re-fills from swarm/seed on next request.
Thumbnails are cheap and high-value, so retain them longer than full-size originals.

## Security

- Not an open proxy. Only serve infohashes present in the board's catalog; otherwise the gateway becomes a tool for laundering arbitrary torrents.
- Integrity is free: every served byte is piece-verified against the infohash.
- Rate-limit cold-miss swarm fetches per client to prevent a cold-cache stampede.

## Engine choice

Use a battle-tested server torrent engine and wrap it with your own HTTP + cache + image
layer, then a CDN in front:

- Rust: `rqbit` / `librqbit`, fast, built-in HTTP streaming.

Thumbnailing via libvips (`sharp` if the HTTP layer is Node). The torrent engine, the cache,
and the image pipeline are separable processes so each scales on its own
