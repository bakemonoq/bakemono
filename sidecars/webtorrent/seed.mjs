import WebTorrent from 'webtorrent'
import readline from 'node:readline'

// wss trackers let browsers (WebRTC) discover the same swarm the desktop seeds to
const TRACKERS = [
  'wss://tracker.openwebtorrent.com',
  'wss://tracker.webtorrent.dev',
  'udp://tracker.opentrackr.org:1337/announce',
]

const client = new WebTorrent()

function send(message) {
  process.stdout.write(JSON.stringify(message) + '\n')
}

client.on('error', (err) => send({ event: 'error', message: String(err?.message ?? err) }))

function seed(path) {
  try {
    client.seed(path, { announce: TRACKERS }, (torrent) => {
      send({ event: 'seeded', path, magnet: torrent.magnetURI, infoHash: torrent.infoHash })
    })
  } catch (err) {
    send({ event: 'error', message: String(err?.message ?? err), path })
  }
}

function shutdown() {
  client.destroy(() => process.exit(0))
}

const rl = readline.createInterface({ input: process.stdin })
rl.on('line', (line) => {
  const text = line.trim()
  if (!text) return
  let msg
  try {
    msg = JSON.parse(text)
  } catch {
    return send({ event: 'error', message: 'invalid json command' })
  }
  if (msg.cmd === 'seed' && typeof msg.path === 'string') seed(msg.path)
  else if (msg.cmd === 'shutdown') shutdown()
  else send({ event: 'error', message: 'unknown command' })
})
rl.on('close', shutdown)

process.on('SIGINT', shutdown)
process.on('SIGTERM', shutdown)

send({ event: 'ready' })
