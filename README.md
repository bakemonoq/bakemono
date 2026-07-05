<p align="center">
  <img src="assets/banner.png" alt="化け物 bakemono" width="680">
</p>

Keep the content you care about available even after the original site takes it down. A board archives sources itself, stores every file in IPFS, and publishes the whole archive as one signed manifest. Volunteer keepers replicate it with stock IPFS tools; if the board's server dies, the archive is rebuilt from any keeper.

- Browsing needs nothing: the board serves everything as ordinary HTTP, so `<img>` and `<video>` just work.
- Keeping is two stock programs: Kubo + `ipfs-cluster-follow`. No Bakemono software, no account.
- Takedowns are honest: every removal is recorded in a hash-linked, signed manifest history that keepers hold and anyone can audit.

Status: main is mid-migration to this architecture. The latest release still ships the previous BitTorrent + Nostr stack, including the desktop app; both go away when the migration lands (`docs/MVP.md` has the order).

## Try it

Browse the reference board at [bakemono.app](https://bakemono.app) with nothing installed.

## Become a keeper

Donate disk and bandwidth; the archive survives as long as one keeper does.

```sh
ipfs daemon --enable-gc &
ipfs-cluster-follow bakemono init https://bakemono.app/follower.json
ipfs-cluster-follow bakemono run
```

Details, sizing, and what following commits you to: [`docs/KEEPERS.md`](docs/KEEPERS.md).

## Run your own board

The stack is one compose file: the `bakemono` binary, postgres, Kubo, and `ipfs-cluster-service`.

```
git clone https://github.com/bakemonoq/bakemono
cd bakemono
docker compose up -d --build
```

The UI comes up at `http://localhost:3000`. Board name, domain, and moderation knobs are documented in `docker-compose.yml`; scraping needs source-platform cookies added through the admin UI.

## Build from source

Single Cargo workspace, Rust stable:

```
cargo build --workspace
cargo test --workspace

# the board (needs postgres at DATABASE_URL and a local kubo)
cargo run -p bakemono-board -- serve
```

Protocol and design details live in `docs/`.

## Licence

[AGPL-3.0-or-later](LICENSE). Viral copyleft is intentional: any modification that touches the network must also be open
