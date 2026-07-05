# Keepers

How to donate disk and bandwidth so the archive outlives any single server. A keeper replicates the board's pinset: every file, every thumbnail, and the manifest history itself. If the board dies, the archive is rebuilt from any keeper.

You run two stock programs, no Bakemono software: [Kubo](https://docs.ipfs.tech/install/command-line/) and [ipfs-cluster-follow](https://ipfscluster.io/documentation/deployment/setup/#running-ipfs-cluster-follow). The board's `/keepers` page is the human front door: it shows the follower config URL, the current pinset size, and live fleet stats.

## Quick start

```sh
# 1. install and start kubo with periodic GC
ipfs init
ipfs config --json Reprovider.Strategy '"roots"'
ipfs daemon --enable-gc &

# 2. install ipfs-cluster-follow and join
ipfs-cluster-follow bakemono init https://board.example/follower.json
ipfs-cluster-follow bakemono run
```

That is the whole job. The follower syncs the pinset, Kubo fetches and pins every CID in it, and new publishes flow in automatically. Run both under systemd or in containers for anything long-lived; the `/keepers` page links ready-made unit files and a compose file.

## What following means

- **You mirror everything.** The pinset is the whole archive; partial adoption (one creator, one platform) is on the roadmap but not built yet. Check the pinset size on `/keepers` before committing disk.
- **Takedowns apply to you automatically.** When the board revokes content, its CIDs leave the pinset; your follower unpins them and your next GC frees the space. This is intentional: you are hosting a moderated archive, not a write-once mirror. The full reasoning and the transparency chain that lets you audit every removal are in `PROTOCOL.md`.
- **You also hold the index.** Manifest heads, roots, and shards are pinned alongside the content. In a disaster the operator fetches the latest head from you and rebuilds the board with `bakemono restore`.
- **Stopping is safe.** Kill the processes whenever; nothing depends on your uptime individually. Rejoining resyncs from where the pinset stands.

## Sizing and configuration

- **Disk**: pinset size plus ~20% headroom for blockstore overhead and GC lag.
- **GC**: run the daemon with `--enable-gc`. Without it, unpinned (revoked) content stays on your disk forever.
- **Ports**: outbound-only works. Opening the libp2p port (4001 TCP+UDP) makes your node fetchable by other keepers and IPFS peers at large, which is most of the point - open it if you can.
- **Bandwidth**: Kubo's defaults are chatty. Cap connections with `Swarm.ConnMgr` if the node shares a home line.

## Verifying your contribution

```sh
ipfs-cluster-follow bakemono list | wc -l    # CIDs the follower tracks
ipfs pin ls --type=recursive | wc -l         # pins actually held locally
```

The `/keepers` page shows the cluster's view: peer count and how much of the pinset each allocation holds.

## What keeping does and does not do

Keeping guarantees the archive survives the board and every other keeper individually; the more independent keepers, the more failure domains the archive spans. It does not make you a publisher: only the board's key can change the manifest, and your follower will faithfully mirror those changes, removals included. If a removal offends you enough to fork, the manifest history you already pin is everything needed to start your own board - which is the design working as intended
