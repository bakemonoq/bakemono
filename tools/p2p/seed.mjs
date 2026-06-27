// minimal seeder: no tracker, no dht, no relay. only accepts direct peers (addPeer from the leecher)
import WebTorrent from 'webtorrent'

const file = process.argv[2]
const nameOverride = process.argv[3] // optional filesystem-safe torrent name
if (!file) {
  console.error('usage: node seed.mjs <file> [safeName]')
  process.exit(1)
}

const client = new WebTorrent({ dht: false, lsd: false, tracker: false, utp: false })
client.on('error', (e) => console.error('CLIENT ERROR', e.message))

const opts = { announce: [] }
if (nameOverride) opts.name = nameOverride
client.seed(file, opts, (torrent) => {
  console.log('infohash :', torrent.infoHash)
  console.log('tcp port :', client.torrentPort)
  console.log('magnet   :', torrent.magnetURI)
  console.log('seeding, waiting for an incoming peer...')

  torrent.on('wire', (wire, addr) => {
    console.log('[seed] PEER CONNECTED from', addr)
    wire.on('interested', () => console.log('[seed]   peer is INTERESTED'))
    wire.on('uninterested', () => console.log('[seed]   peer uninterested'))
  })
})

setInterval(() => {
  const t = client.torrents[0]
  if (t) console.log(`[seed] status: peers=${t.numPeers} uploaded=${t.uploaded}B`)
}, 3000)
