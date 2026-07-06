<p align="center">
  <img src="assets/banner.png" alt="化け物 bakemono" width="680">
</p>

Keep the content you care about available even after the original site takes it down. A board archives sources itself, stores every file in IPFS, and publishes the whole archive as one signed manifest. Volunteers replicate it with stock IPFS tools; if the board's server dies, the archive is rebuilt from any one of them.

- **Browsing needs nothing.** Every file is served over ordinary HTTP straight from the board host's IPFS gateway, so `<img>` and `<video>` just work in any browser - no plugin, no P2P client.
- **Keeping needs two stock programs.** Kubo and `ipfs-cluster-follow` - no Bakemono binary, no account - mirror the whole archive and honour every takedown automatically.
- **Takedowns are honest.** Each removal is recorded in a hash-linked, signed manifest history that keepers hold and anyone can audit.

Status: `main` implements this architecture. Releases older than the mid-2026 pivot shipped a different (BitTorrent + Nostr) stack and are obsolete; [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains why it was retired.

## Browse

The reference board runs at [bakemono.app](https://bakemono.app) - a normal website, nothing to install.

## Become a keeper

Donate disk and bandwidth; the archive survives as long as one keeper does. On any machine with [Kubo](https://docs.ipfs.tech/install/command-line/) and [ipfs-cluster-follow](https://ipfscluster.io/documentation/deployment/setup/):

```sh
ipfs daemon --enable-gc &
ipfs-cluster-follow bakemono init https://bakemono.app/follower.json
ipfs-cluster-follow bakemono run
```

On a fresh Linux box the one-liner in [`docs/KEEPERS.md`](docs/KEEPERS.md) installs both and runs them under systemd. That page also covers sizing and what following commits you to.

## Run your own board

The whole stack is one compose file: the `bakemono` binary, postgres, Kubo, and `ipfs-cluster-service`.

```sh
git clone https://github.com/bakemonoq/bakemono
cd bakemono
docker compose up -d --build
```

Everything comes up on one origin at `http://localhost:3000`: a bundled reverse proxy routes `/ipfs/*` to the Kubo gateway (media is served straight from IPFS, the board stays out of the byte path) and everything else to the board. A production deployment swaps that proxy for your own with TLS and a real domain.

Basic knobs - board name, contacts, mod password, scrape proxy - are environment variables documented inline in [`docker-compose.yml`](docker-compose.yml). For richer customization - mascot, welcome and about text, accent colour, community links - copy [`board.toml.example`](board.toml.example) to `board.toml` and uncomment its mount in the compose file. Scraping needs source-platform cookies, added through the board's `/contribute` page.

## Build from source

Single Cargo workspace, Rust stable:

```sh
cargo build --workspace
cargo test --workspace

# the board binary is `bakemono`; serve needs postgres at DATABASE_URL and a local kubo
cargo run -p bakemono-board -- serve
```

Design details live in [`docs/`](docs): [architecture](docs/ARCHITECTURE.md), [manifest protocol](docs/PROTOCOL.md), [glossary](docs/GLOSSARY.md). Code contributions are welcome ([`CONTRIBUTING.md`](CONTRIBUTING.md)); to report a vulnerability privately see [`SECURITY.md`](SECURITY.md).

## License

Copyright (c) 2026 Bakemono contributors. Licensed under [AGPL-3.0-or-later](LICENSE); the viral copyleft is intentional, so any modification reachable over the network must ship its source too
