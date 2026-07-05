#!/bin/sh
# runs from /container-init.d on every start of the ipfs/kubo container, before the daemon

# recent kubo ships the bitswap server disabled: the node fetches but never serves blocks to
# peers. an archive node's whole job is serving blocks, so turn it on explicitly
ipfs config --json Bitswap.ServerEnabled true

# broadcast reduction stops wantlists from reaching even directly-peered fleet nodes (observed
# on kubo 0.42, despite documented peered/local exemptions). an archive node talks to a small
# known fleet, so the reduction buys nothing - restore full broadcasts
ipfs config --json Internal.Bitswap.BroadcastControl.Enable false

# announce only the CIDs manifests reference, or reproviding drowns at archive scale
ipfs config Reprovider.Strategy roots 2>/dev/null || ipfs config Provide.Strategy roots 2>/dev/null || true
