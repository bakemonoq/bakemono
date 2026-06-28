# bakemono-app

Desktop client. A Tauri shell over a Tauri-free core (`src/core`):

- `identity` - nsec gen/import/export, persisted
- `config` - relay list + settings
- `scrape` - gallery-dl output (media + `.json`) -> `Manifest`
- `seeder` - long-lived webtorrent handle (one sidecar for the whole app)
- `library` - catalog of shared files (`library.json`), drives stats and re-seed
- `pipeline` - the scrape/ingest -> seed -> sign -> publish flow, emitting `Progress`

On launch the app starts the seeder and re-seeds everything in the library, so a restart resumes seeding. Each scrape records its files to the library and keeps seeding (the seeder is not torn down between jobs).

## Run the GUI

```
cargo run -p bakemono-app
```

## Headless harness

The whole pipeline runs without the GUI via the `scrapetest` bin (gated behind the `harness` feature). It spins up an embedded relay, runs the core, then subscribes and verifies every published event round-trips.

Ingest a local folder of media + `.json` sidecars and seed over WebTorrent:

```
cargo run -p bakemono-app --no-default-features --features harness --bin scrapetest -- ingest out
```

Real Patreon scrape, capped to N posts, with a cookies file:

```
cargo run -p bakemono-app --no-default-features --features harness --bin scrapetest -- \
  scrape <creator> --limit 5 --cookies patreon-cookies.txt
```

Run from the workspace root so `out/` and `sidecars/webtorrent/seed.mjs` resolve. `BAKEMONO_NSEC` pins an identity; `--no-seed` skips the Node sidecar; `--limit N` caps to N posts
