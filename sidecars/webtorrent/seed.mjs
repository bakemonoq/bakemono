import WebTorrent from 'webtorrent'
import readline from 'node:readline'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

// announce list; override with BAKEMONO_TRACKERS (comma-separated) to use your own tracker
const OVERRIDE = (process.env.BAKEMONO_TRACKERS ?? '')
  .split(',')
  .map((t) => t.trim())
  .filter(Boolean)
const TRACKERS = OVERRIDE.length > 0 ? OVERRIDE : defaultTrackers()
if (TRACKERS.length === 0) {
  console.error('warning: no trackers (BAKEMONO_TRACKERS unset and defaults/trackers.txt unreadable); swarm will be DHT-only')
}

// standalone fallback: read the shared list at /defaults/trackers.txt (the rust daemon sets the env)
function defaultTrackers() {
  try {
    const path = fileURLToPath(new URL('../../defaults/trackers.txt', import.meta.url))
    return readFileSync(path, 'utf8')
      .split('\n')
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith('#'))
  } catch {
    return []
  }
}

// BAKEMONO_ISOLATE=1 turns off DHT/LSD so only our own tracker forms the swarm (avoids VPN/foreign peers)
const isolate = process.env.BAKEMONO_ISOLATE === '1'
// BAKEMONO_RTC_BIND pins WebRTC to one interface so we don't advertise VPN addresses to browsers
const rtcConfig = {}
if (process.env.BAKEMONO_RTC_BIND) rtcConfig.bindAddress = process.env.BAKEMONO_RTC_BIND
// BAKEMONO_RTC_PORTS=begin-end pins the WebRTC UDP range so a firewalled host opens only that range
if (process.env.BAKEMONO_RTC_PORTS) {
  const [begin, end] = process.env.BAKEMONO_RTC_PORTS.split('-').map(Number)
  rtcConfig.portRangeBegin = begin
  rtcConfig.portRangeEnd = end
}
if (process.env.BAKEMONO_STUN) {
  rtcConfig.iceServers = process.env.BAKEMONO_STUN.split(',').map((urls) => ({ urls: urls.trim() }))
}
const opts = { tracker: { rtcConfig } }
// BAKEMONO_MAX_UP/DOWN are Mbit/s caps (0 or unset = unlimited); webtorrent wants bytes/s
const MBIT = 125000
const up = Number(process.env.BAKEMONO_MAX_UP) || 0
const down = Number(process.env.BAKEMONO_MAX_DOWN) || 0
if (up > 0) opts.uploadLimit = Math.round(up * MBIT)
if (down > 0) opts.downloadLimit = Math.round(down * MBIT)
if (isolate) {
  opts.dht = false
  opts.lsd = false
}
const client = new WebTorrent(opts)

// when the parent dies the pipe breaks; exit quietly instead of throwing an unhandled EPIPE
process.stdout.on('error', (err) => {
  if (err?.code === 'EPIPE') process.exit(0)
})

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
