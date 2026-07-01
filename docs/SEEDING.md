# Seeding

How to help keep content online. Every file on a board is a classic BitTorrent torrent addressed by infohash, so any standard client can seed it - no Bakemono software required. The board publishes a torrent RSS feed so your client learns what to seed.

The board's `/keepers` page is the human front door to all of this: it explains the model, shows your board's feed URLs, and lists what is endangered right now. This doc is the reference.

## The feed

`GET /feed.xml` on any board returns an RSS 2.0 feed. Each `<item>` is one torrent, newest first, with the magnet in its `<enclosure>`:

```
<item>
  <title>Creator - Post title</title>
  <link>https://board.example/p/patreon/123/456</link>
  <guid isPermaLink="false">&lt;infohash&gt;</guid>
  <pubDate>Mon, 30 Jun 2026 12:00:00 +0000</pubDate>
  <enclosure url="magnet:?xt=urn:btih:..." length="4096" type="application/x-bittorrent"/>
</item>
```

Point your client's RSS auto-download at it and it adds + seeds every new torrent:

- **qBittorrent**: View -> RSS, add `https://board.example/feed.xml`, then add an auto-download rule matching everything.
- **Deluge**: YaRSS2 plugin, add the feed, rule with an empty/`.*` match.
- **ruTorrent**: the RSS plugin, add the feed URL.
- **Transmission**: no native RSS; use Flexget or a `transmission-rss` script pointed at the feed.

Auto-download only ever catches what is new since your client last polled, and clients cap how many articles they keep per feed. That is fine for staying current, but it does not mirror the archive. For that, see backfill below.

## Scoped feeds

Narrow the feed with query params so you can adopt just the slice you care about (this is how a "keeper" backs a single creator without hauling the whole board):

- `?platform=patreon&creator=<creator_id>` - one creator
- `?platform=patreon&creator=<creator_id>&post=<post_id>` - one post
- `?npub=<npub or hex>` - one contributor's uploads

Params combine. A scoped feed is small and complete for its slice, so a stock client subscribed to it seeds everything in that slice with no extra tooling.

## Keepers and endangered content

Adopted from RuTracker's keepers model: rather than everyone re-seeding whatever is already healthy, keepers steer effort toward torrents that are about to die. The board runs a background probe that scrapes each torrent's trackers (BEP 15 UDP scrape) for a live seeder count, and ranks the archive fewest-seeders-first.

- `?sort=endangered` - a feed of the least-seeded torrents, most at-risk first. Point a client here to always be working on whatever is closest to vanishing. This is a priority list, not a full mirror, so it has no `rel="next"` cursor.
- The `/keepers` page shows the same ranking with live seeder counts and an "adopt creator" link per item.

The probe is gentle by design. Each tick it scrapes a batch of the least-recently-checked torrents; the batch auto-sizes off catalog size so the whole catalog is covered once per recheck window (`ceil(catalog / (recheck / interval))`), clamped between a floor and a cap. Until the first pass completes, the endangered list is empty rather than misleading.

Env knobs:

- `BAKEMONO_HEALTH_DISABLE` - turn the probe off entirely
- `BAKEMONO_HEALTH_INTERVAL_SECS` (900) - seconds between ticks
- `BAKEMONO_HEALTH_RECHECK_SECS` (10800) - how stale a count may get before re-scraping; the batch is sized to cover the catalog within this window
- `BAKEMONO_HEALTH_BATCH` - pin a fixed batch size, overriding auto-sizing
- `BAKEMONO_HEALTH_BATCH_MIN` (20) / `BAKEMONO_HEALTH_BATCH_MAX` (1000) - clamp on the auto-sized batch
- `BAKEMONO_HEALTH_CONCURRENCY` (8) - torrents probed in parallel per tick
- `BAKEMONO_HEALTH_TIMEOUT_SECS` (4) - per-tracker wait

## Full mirror (backfill)

To grab everything, not just the newest window, walk the cursor. A full page carries `<atom:link rel="next" href="...?before=<timestamp>&limit=...">`; follow it until no next link remains. Torrent clients do not follow this themselves, so run a small script and hand the magnets to your client.

```python
#!/usr/bin/env python3
# walk a bakemono seed feed to the end, print every magnet. pipe into your client.
import sys, urllib.request, xml.etree.ElementTree as ET

ATOM = "{http://www.w3.org/2005/Atom}"
url = sys.argv[1] if len(sys.argv) > 1 else "https://board.example/feed.xml?limit=1000"

while url:
    with urllib.request.urlopen(url) as r:
        root = ET.fromstring(r.read())
    chan = root.find("channel")
    for item in chan.findall("item"):
        enc = item.find("enclosure")
        if enc is not None:
            print(enc.get("url"))
    nxt = chan.find(f"{ATOM}link[@rel='next']")
    url = nxt.get("href") if nxt is not None else None
```

```sh
# add everything to qBittorrent via its Web API
python3 backfill.py "https://board.example/feed.xml?limit=1000" > magnets.txt
while read m; do
  curl -s -F "urls=$m" http://localhost:8080/api/v2/torrents/add >/dev/null
done < magnets.txt
```

Scope the backfill the same way, e.g. `.../feed.xml?limit=1000&platform=patreon&creator=<id>`.

## What seeding does and does not do

Seeding a magnet keeps that torrent alive in the swarm, so the board's gateway (and other seeders) can always cold-fill it. It is the direct fix for the single-node-down problem: the more independent seeders a torrent has, the longer it survives.

A commodity client will not automatically honor a board's `kind 31064` takedown - it seeds whatever magnets you gave it. New propagation of a taken-down item stops because the board drops it from the feed, and the board gateway still enforces its catalog for browsers, but bytes already on a third-party seeder are outside any board's control. That is inherent to open BitTorrent.
