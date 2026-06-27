// tracker-based leecher (no dht/lsd): finds the seeder via the tracker in the magnet
import WebTorrent from 'webtorrent'

const magnet = process.argv[2]
if (!magnet) {
  console.error('usage: node tleech.mjs <magnet>')
  process.exit(1)
}

const client = new WebTorrent({ dht: false, lsd: false })
client.on('error', (e) => console.error('CLIENT ERROR', e.message))

const torrent = client.add(magnet, { path: '/tmp/bk-prod-dl' })
torrent.on('done', () => {
  console.log('DONE downloaded', torrent.downloaded)
  process.exit(0)
})
setInterval(() => {
  console.log(`dl=${torrent.downloaded}/${torrent.length || '?'} peers=${torrent.numPeers}`)
}, 2000)
setTimeout(() => {
  console.log('STOP downloaded=' + torrent.downloaded)
  process.exit(torrent.downloaded > 0 ? 0 : 2)
}, 30000)
