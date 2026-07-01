# Gateway demo: seed locally, serve through a board

The board is now a pure torrent -> HTTP gateway (librqbit). It joins a swarm over classic
BitTorrent (TCP/uTP + DHT + trackers), pulls bytes, and serves plain HTTP. Nothing does WebRTC.
Seeding is done by external clients: our own daemon/app, or any standard client (qBittorrent, rqbit).

## Peer discovery, the one thing to know

Classic BT has no browser-style rendezvous, so two peers find each other via a tracker or DHT and
then one dials the other. That means at least one side needs a reachable port:

- Same machine / LAN test: public trackers hand back your public IP, which usually will not loop back
  to a local seeder. Pin the seeder directly with `BAKEMONO_GATEWAY_PEERS=ip:port` on the board.
- Deployed board + home seeder: give the board an open, port-forwarded BT port
  (`BAKEMONO_GATEWAY_PORT`). The NAT'd seeder announces to the same tracker and dials the board out.

## A. Prove the pipe (no board, no db)

Verifies the seed <-> gateway path over classic BT end to end.

```
# terminal 1: seed a file, note the infohash + port it prints
cargo run -p bakemono-torrent --example seed -- /path/to/image.jpg 4250

# terminal 2: pull it back through the gateway code, pinned at the local seeder
BAKEMONO_GATEWAY_PEERS=127.0.0.1:4250 \
  cargo run -p bakemono-torrent --example fetch -- <infohash> 0 pulled.bin
# pulled.bin should match the source byte for byte
```

## B. Board serving over HTTP (open mode)

Serves the same cold torrent through the board's real HTTP routes, including `Range`.

```
docker compose up -d postgres            # board needs Postgres
cargo run -p bakemono-torrent --example seed -- /path/to/image.jpg 4250   # keep running

BAKEMONO_GATEWAY_OPEN=1 \
BAKEMONO_GATEWAY_PEERS=127.0.0.1:4250 \
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/bakemono \
  cargo run -p bakemono-board

curl -s   localhost:3000/t/<infohash>/meta            # file list
curl -o o.bin localhost:3000/t/<infohash>/f/0         # full file, cold-filled
curl -r 0-1023 -D- -o /dev/null localhost:3000/t/<infohash>/f/0   # 206 + Content-Range
```

`BAKEMONO_GATEWAY_OPEN=1` lifts the catalog check so any infohash can be tested. Drop it for the real
flow below, where only infohashes the board indexed are served.

## C. Full loop: local app seeds, board indexes, browse the preview

```
docker compose up -d postgres relay      # relay = the Nostr relay the board indexes and the app publishes to
```

Run the seeder side so the board can pin it. Launch from a terminal so the env carries through to the
daemon the app spawns:

```
export BAKEMONO_SEED_PORT=4250
cargo run -p bakemono-app                # or run bakemono-daemon headless / the scrapetest harness
```

In the app: sign in to a creator and start a scrape. The daemon downloads the posts, seeds each file
over classic BT via librqbit, and publishes a kind 31063 manifest (with the btih magnet) to the relay.

Run the board pinned at the app's seeder:

```
BAKEMONO_GATEWAY_PEERS=127.0.0.1:4250 \
BAKEMONO_RELAYS=ws://127.0.0.1:8080 \
BAKEMONO_MOD_TOKEN=letmein \
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/bakemono \
  cargo run -p bakemono-board
```

Then: open `localhost:3000/mod` (user any, password `letmein`) and approve the new pubkey, open the
post, and the image/video streams straight from the gateway (`<img src="/t/{infohash}/f/0">`).

## D. Deploy the board to the box

`deploy.sh` builds the board image (now with libssl for librqbit), pushes it, and brings up
postgres + relay + board via `docker-compose.deploy.yml`. Open the gateway BT port on the box so home
seeders can dial in:

```
BAKEMONO_GATEWAY_PORT=4240 BAKEMONO_MOD_TOKEN=... ./deploy.sh
# then allow TCP+UDP 4240 through the box firewall / Cloudflare origin
```

A home app/daemon seeding the same content (announcing to the tracker in its published magnet) will
serve the board once the board's 4240 is reachable. For a guaranteed path, run a seedbox with an open
port and set `BAKEMONO_GATEWAY_PEERS=seedbox-ip:port` on the board.
