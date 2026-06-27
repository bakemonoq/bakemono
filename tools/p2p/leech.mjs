// minimal leecher: no tracker, no dht. connects straight to the seeder by ip:port
import WebTorrent from 'webtorrent'

const magnet = process.argv[2]
const peer = process.argv[3]
if (!magnet || !peer) {
  console.error('usage: node leech.mjs <magnet> <seederIp:port>')
  process.exit(1)
}

const client = new WebTorrent({ dht: false, lsd: false, tracker: false, utp: false })

setTimeout(() => {
  console.log('[leech] STOP downloaded=' + torrent.downloaded)
  process.exit(torrent.downloaded > 0 ? 0 : 2)
}, 25000)
client.on('error', (e) => console.error('CLIENT ERROR', e.message))

const torrent = client.add(magnet, { path: './bk-dl' })
torrent.on('infoHash', () => console.log('addPeer', peer, '->', torrent.addPeer(peer)))

torrent.on('metadata', () => console.log('[leech] METADATA ok, length =', torrent.length))
torrent.on('warning', (w) => console.log('[leech] warning:', w.message))
torrent.on('wire', (wire, addr) => {
  console.log('[leech] PEER CONNECTED to', addr)
  wire.on('unchoke', () => console.log('[leech]   UNCHOKED by', addr))
  wire.on('choke', () => console.log('[leech]   choked by', addr))
})
torrent.on('done', () => {
  console.log('[leech] DONE downloaded', torrent.downloaded, 'bytes')
  process.exit(0)
})

setInterval(() => {
  if (torrent.numPeers === 0 && torrent.infoHash) torrent.addPeer(peer)
  console.log(
    `[leech] status: peers=${torrent.numPeers} downloaded=${torrent.downloaded}/${torrent.length || '?'} speed=${Math.round(torrent.downloadSpeed)}B/s`
  )
}, 3000)
