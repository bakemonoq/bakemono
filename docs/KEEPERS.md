# Keepers

How to donate disk and bandwidth so the archive outlives any single server. A keeper replicates the board's pinset: every file, every thumbnail, and the manifest history itself. If the board dies, the archive is rebuilt from any keeper.

You run two stock programs, no Bakemono software: [Kubo](https://docs.ipfs.tech/install/command-line/) and [ipfs-cluster-follow](https://ipfscluster.io/documentation/deployment/setup/#running-ipfs-cluster-follow). The board's `/keepers` page is the human front door: it shows the follower config URL, the current pinset size, and live fleet stats.

## Quick setup (Linux)

One command on a fresh Linux box installs kubo + ipfs-cluster-follow, points a follower at the board, and runs both under systemd. Swap in the board you want to keep:

```sh
curl -fsSL https://raw.githubusercontent.com/bakemonoq/bakemono/main/scripts/keeper-setup.sh | sudo bash -s -- https://board.example
```

The script is [`scripts/keeper-setup.sh`](../scripts/keeper-setup.sh); it is idempotent, so re-running it is safe. The board's own `/keepers` page shows the same command pre-filled with its URL.

## Manual setup

```sh
# 1. install and configure kubo
ipfs init
# serve blocks to peers - recent kubo ships with the bitswap server OFF, which would make
# your node a leech; and full want broadcasts, so fleet peers hear you without DHT luck
ipfs config --json Bitswap.ServerEnabled true
ipfs config --json Internal.Bitswap.BroadcastControl.Enable false
# announce only the roots manifests reference. kubo >= 0.38 renamed this to Provide.Strategy and
# FATALs if a deprecated Reprovider key lingers, so set the new key and drop the old one when present
if ipfs config Provide.Strategy roots 2>/dev/null; then
  ipfs config --json Reprovider '{}' 2>/dev/null || true
else
  ipfs config Reprovider.Strategy roots
fi
# your gateway serves ONLY blocks you already hold - never fetches arbitrary hashes for strangers.
# this keeps it "our files only" and, with the denylist below, blocks takedowns immediately
ipfs config --json Gateway.NoFetch true
ipfs daemon --enable-gc &

# 2. install ipfs-cluster-follow and join
ipfs-cluster-follow bakemono init https://board.example/follower.json
ipfs-cluster-follow bakemono run

# 3. keep the takedown denylist current (the quick-setup script installs a systemd timer for this).
# nopfs reads $IPFS_PATH/denylists/*.deny; the board's signed list is referenced from the manifest:
root=$(curl -fsSL https://board.example/head.json | jq -r .root)
deny=$(ipfs cat /ipfs/$root | jq -r .denylist)
[ -n "$deny" ] && ipfs cat /ipfs/$deny > $IPFS_PATH/denylists/bakemono.deny
```

That is the whole job. The follower syncs the pinset, Kubo fetches and pins every CID in it, and new publishes flow in automatically. For anything long-lived run both under systemd, which is exactly what the quick-setup script wires up for you.

## What following means

- **You mirror everything.** The pinset is the whole archive; partial adoption (one creator, one platform) is on the roadmap but not built yet. Check the pinset size on `/keepers` before committing disk.
- **Takedowns apply to you immediately, then clean up.** Unpinning removes revoked CIDs from the pinset, but GC is not prompt - kubo only frees blocks near the storage cap - so an unpinned block can stay on disk, and reachable by its direct hash, for a long time. So there are two layers: `Gateway.NoFetch` means your gateway only serves what you hold (never fetches hashes for strangers), and the board's signed denylist loaded into nopfs makes any revoked hash return 410 at once, independent of GC. The denylist lives in IPFS (referenced from the manifest), so it survives the board dying and the sync just pulls the copy you already replicate. This matters most for the categorically-illegal content the board actively moderates. The transparency chain that lets you audit every removal is in `PROTOCOL.md`.
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
