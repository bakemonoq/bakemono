#!/bin/sh
# runs from /container-init.d on every start of the ipfs/kubo container, before the daemon

# recent kubo ships the bitswap server disabled: the node fetches but never serves blocks to
# peers. an archive node's whole job is serving blocks, so turn it on explicitly
ipfs config --json Bitswap.ServerEnabled true

# broadcast reduction stops wantlists from reaching even directly-peered fleet nodes (observed
# on kubo 0.42, despite documented peered/local exemptions). an archive node talks to a small
# known fleet, so the reduction buys nothing - restore full broadcasts
ipfs config --json Internal.Bitswap.BroadcastControl.Enable false

# announce only the CIDs manifests reference, or reproviding drowns at archive scale.
# kubo >= 0.38 wants Provide.Strategy and FATALs if a deprecated Reprovider key lingers
if ipfs config Provide.Strategy roots 2>/dev/null; then
    ipfs config --json Reprovider '{}' 2>/dev/null || true
else
    ipfs config Reprovider.Strategy roots
fi

# the gateway serves only blocks this node already holds; the board's catalog is always pinned
# locally, and a taken-down CID must not be fetchable from the network through us
ipfs config --json Gateway.NoFetch true

# nopfs only watches denylist files that already exist when the daemon starts, so make sure ours is
# there. the board rewrites it in place on every takedown, which nopfs then live-reloads (410 at once)
DENY="${IPFS_PATH:-/data/ipfs}/denylists/bakemono.deny"
mkdir -p "$(dirname "$DENY")"
[ -f "$DENY" ] || printf 'version: 1\nname: bakemono\n---\n' > "$DENY"
